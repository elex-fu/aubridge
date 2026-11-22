//! ChatGPT(codex 订阅)网关通道(设计文档 §6.3/§7)。
//!
//! OAuth 凭据独立于源 API key(§7:登录由用户跑
//! `codex login`,aubridge 只读 `~/.codex/auth.json` + 代刷新)。
//! 内存缓存进程级共享;刷新串行化(tokio Mutex)——并发请求同时
//! 401 时只发一次刷新请求,避免 refresh token 轮换竞态(magpie
//! 铁律:刷新必等完,丢弃应答会让手里的旧 token 已被作废)。
//! 写回经 credentials::chatgpt(字段级合并 + 原子 rename,红线)。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use credentials::chatgpt::{
    self as cgpt, write_back_codex_tokens, CODEX_CLIENT_ID, CODEX_TOKEN_ENDPOINT,
};
use credentials::oauth::{read_codex_login, CodexTokens};
use credentials::opencode::{
    read_opencode_openai_login, refresh_opencode_openai_via_http, OpenCodeRefreshOutcome,
};
use credentials::refresh::{refresh_via_http, RefreshOutcome};

/// UA 对齐 Codex CLI(§6.3:对齐真实客户端,不做指纹随机化——
/// 安全红线明令禁止)。
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.166.0 (Mac OS 15.6; arm64) xterm-256color";

pub(crate) const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_OPENAI_BETA: &str = "responses=experimental";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoginSource {
    Codex,
    OpenCode,
}

impl LoginSource {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
}

#[derive(Clone, PartialEq)]
struct FileLogin {
    kind: LoginSource,
    tokens: CodexTokens,
    expires_ms: Option<u64>,
}

#[derive(Clone)]
struct CachedLogin {
    source: Arc<FileLogin>,
    effective: Arc<CodexTokens>,
    expires_ms: Option<u64>,
}

struct LoginState {
    selected: Option<LoginSource>,
    selection: Arc<()>,
    cached: Option<CachedLogin>,
}

/// Request-bound identity; selection changes cannot redirect a 401 recovery.
#[derive(Clone)]
pub struct LoginSnapshot {
    selection: Arc<()>,
    login: CachedLogin,
    refreshed: bool,
}

impl std::ops::Deref for LoginSnapshot {
    type Target = CodexTokens;

    fn deref(&self) -> &Self::Target {
        &self.login.effective
    }
}

impl LoginSnapshot {
    pub(crate) fn compaction_subject(&self) -> String {
        credentials::quota::subject_tag(
            "compaction-login",
            &format!(
                "{}:{}",
                self.login.source.kind.as_str(),
                self.quota_subject()
            ),
        )
    }

    pub(crate) fn quota_subject(&self) -> String {
        let identity = cgpt::resolve_account_id(self).unwrap_or_else(|| self.access_token.clone());
        credentials::quota::subject_tag("chatgpt-account", &identity)
    }
}

pub struct LoginMetadata {
    pub source: Option<&'static str>,
    pub status: &'static str,
    pub path: Option<String>,
}

/// chatgpt 订阅凭据通道(网关侧缓存 + 刷新编排)。
pub struct ChatGptChannel {
    auth_json: PathBuf,
    config_toml: PathBuf,
    opencode_auth: Option<PathBuf>,
    /// 刷新端点(生产恒官方端点;测试注入回放地址)。
    token_endpoint: String,
    responses_base: String,
    login: Mutex<LoginState>,
    /// 刷新串行锁(§7 刷新必等完 + 轮换竞态)。
    refresh_lock: tokio::sync::Mutex<()>,
    /// 刷新请求专用 client(读环境代理,与转发互不影响)。
    http: reqwest::Client,
    /// "与 CLI 共享登录"(§7 opt-in,默认 false = 只读):开启时
    /// 刷新产物才写回 auth.json;关闭时只更新内存缓存,用户 CLI 文件
    /// 分毫不动。开关状态由 GatewayState 持久化(config.json)。
    share_login: RwLock<bool>,
}

impl ChatGptChannel {
    pub const RESPONSES_BASE: &str = "https://chatgpt.com/backend-api/codex";

    pub fn new(auth_json: PathBuf, config_toml: PathBuf) -> Self {
        Self {
            auth_json,
            config_toml,
            opencode_auth: None,
            token_endpoint: CODEX_TOKEN_ENDPOINT.to_string(),
            responses_base: Self::RESPONSES_BASE.to_string(),
            login: Mutex::new(LoginState {
                selected: Some(LoginSource::Codex),
                selection: Arc::new(()),
                cached: None,
            }),
            refresh_lock: tokio::sync::Mutex::new(()),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build OAuth client"),
            share_login: RwLock::new(false),
        }
    }

    /// auth.json 路径(stash 备份/一键还原用)。
    pub fn auth_json_path(&self) -> &Path {
        &self.auth_json
    }

    /// "与 CLI 共享登录"开关(§7):由 GatewayState 在持久化 +
    /// stash 备份完成后设置,通道只管执行口径。
    pub fn set_share_login(&self, on: bool) {
        *self.share_login.write().expect("share_login lock") = on;
    }

    pub fn share_login(&self) -> bool {
        *self.share_login.read().expect("share_login lock")
    }

    pub(crate) fn restore_stash(&self, stash_dir: &Path) -> io::Result<()> {
        // Publish the restored file and invalidate the login generation together.
        let mut login = self.login.lock().expect("chatgpt login lock");
        credentials::stash::restore(stash_dir, &self.auth_json)?;
        login.cached = None;
        Ok(())
    }

    pub fn with_opencode_auth(mut self, path: PathBuf) -> Self {
        self.opencode_auth = Some(path);
        self
    }

    /// 首启自动判定缺省来源(opencode-auth-design §4,2026-11-14):
    /// 仅在无持久化选择时由 Runtime 启动期调用一次。Codex 登录可读
    /// 则 codex(与原缺省一致);仅当 Codex 干净缺失(Ok(None),含
    /// keyring guard——本通道读不到)且 OpenCode openai 登录可读时才
    /// 缺省 opencode。Codex 文件损坏(Err)不判给 opencode,保持
    /// codex 缺省由状态面板暴露错误。运行期不重估:来源切换仍只能
    /// 显式设置(§4 fail-closed 不变)。
    pub fn detect_login_source(&self) -> LoginSource {
        // OpenCode 路径未接线(单测默认通道等)无从判定,保持 codex
        // 缺省,也不读 Codex 文件。
        if self.opencode_auth.is_none() {
            return LoginSource::Codex;
        }
        if matches!(
            read_codex_login(&self.auth_json, &self.config_toml),
            Ok(None)
        ) {
            if let Some(path) = self.opencode_auth.as_deref() {
                if matches!(read_opencode_openai_login(path), Ok(Some(_))) {
                    return LoginSource::OpenCode;
                }
            }
        }
        LoginSource::Codex
    }

    pub fn set_login_source(&self, source: Option<LoginSource>) {
        let mut login = self.login.lock().expect("chatgpt login lock");
        if login.selected == source {
            return;
        }
        login.selected = source;
        login.selection = Arc::new(());
        login.cached = None;
    }

    pub fn login_source(&self) -> Option<&'static str> {
        self.login
            .lock()
            .expect("chatgpt login lock")
            .selected
            .map(LoginSource::as_str)
    }

    pub fn metadata(&self) -> LoginMetadata {
        let mut login = self.login.lock().expect("chatgpt login lock");
        let path = match login.selected {
            Some(LoginSource::Codex) => Some(self.auth_json.as_path()),
            Some(LoginSource::OpenCode) => self.opencode_auth.as_deref(),
            None => None,
        };
        let status = match self.reconcile(&mut login) {
            Ok(Some(_)) => "ready",
            Ok(None) => "missing",
            Err(_) => "unavailable",
        };
        LoginMetadata {
            source: login.selected.map(LoginSource::as_str),
            status,
            path: path.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    /// 注入刷新端点(集成测试回放;生产路径不调用)。
    pub fn with_token_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.token_endpoint = endpoint.into();
        self
    }

    /// Replay tests replace the endpoint explicitly. Manifests cannot set it.
    pub fn with_responses_base(mut self, base: impl Into<String>) -> Self {
        self.responses_base = base.into();
        self
    }

    pub(crate) fn owns(&self, manifest: &manifest::schema::Manifest) -> bool {
        manifest.id == "chatgpt"
            && manifest.provider.protocol == manifest::schema::WireProtocol::Responses
            && manifest.provider.base == self.responses_base
    }

    pub(crate) fn has_login(&self) -> bool {
        self.current().is_some()
    }

    /// Restore previews validate the requested login source without changing the active one.
    pub(crate) fn has_login_for(&self, source: LoginSource) -> bool {
        self.read_login(source).ok().flatten().is_some()
    }

    fn read_login(&self, source: LoginSource) -> io::Result<Option<FileLogin>> {
        match source {
            LoginSource::Codex => Ok(read_codex_login(&self.auth_json, &self.config_toml)?
                .and_then(|login| login.tokens)
                .filter(|tokens| !tokens.access_token.is_empty())
                .map(|tokens| FileLogin {
                    kind: source,
                    tokens,
                    expires_ms: None,
                })),
            LoginSource::OpenCode => {
                let path = self.opencode_auth.as_deref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "OpenCode login path unavailable")
                })?;
                Ok(read_opencode_openai_login(path)?.map(|login| FileLogin {
                    kind: source,
                    tokens: CodexTokens {
                        access_token: login.access,
                        refresh_token: login.refresh,
                        account_id: login.account_id,
                        id_token: String::new(),
                    },
                    expires_ms: login.expires_ms,
                }))
            }
        }
    }

    fn reconcile<'a>(&self, state: &'a mut LoginState) -> io::Result<Option<&'a mut CachedLogin>> {
        // A partial file read cannot authorize a request. Keep the last
        // observation so a subsequent read can confirm its rotated tokens.
        let selected = state.selected.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ChatGPT login source unavailable",
            )
        })?;
        let disk = self.read_login(selected)?;
        let cached = &mut state.cached;
        match disk {
            Some(disk) if cached.as_ref().is_some_and(|login| *login.source == disk) => {}
            Some(disk) => {
                let expires_ms = disk.expires_ms;
                let effective = Arc::new(disk.tokens.clone());
                let source = Arc::new(disk);
                *cached = Some(CachedLogin {
                    effective,
                    expires_ms,
                    source,
                });
            }
            None => *cached = None,
        }
        Ok(cached.as_mut())
    }

    fn current(&self) -> Option<LoginSnapshot> {
        let mut state = self.login.lock().expect("chatgpt login lock");
        let login = self.reconcile(&mut state).ok()??.clone();
        Some(LoginSnapshot {
            selection: state.selection.clone(),
            login,
            refreshed: false,
        })
    }

    /// Polling quota alone must not rotate a CLI's real refresh token.
    pub(crate) fn quota_snapshot(&self) -> Option<LoginSnapshot> {
        self.current()
    }

    /// A response on the same account may finish across its ordinary token
    /// refresh, but never across a selected-source change or logout.
    pub(crate) fn update_current_quota_login(
        &self,
        snapshot: &LoginSnapshot,
        update: impl FnOnce(),
    ) {
        let mut state = self.login.lock().expect("chatgpt login lock");
        if !Arc::ptr_eq(&state.selection, &snapshot.selection) {
            return;
        }
        let Ok(Some(current)) = self.reconcile(&mut state) else {
            return;
        };
        let current_identity = cgpt::resolve_account_id(&current.effective)
            .unwrap_or_else(|| current.effective.access_token.clone());
        let current_subject = credentials::quota::subject_tag("chatgpt-account", &current_identity);
        if current_subject == snapshot.quota_subject() {
            update();
        }
    }

    /// Commit a quota read only to the login that sent it. Reconcile while
    /// holding the selection lock so a source switch cannot race the commit.
    pub(crate) fn update_current_login(&self, snapshot: &LoginSnapshot, update: impl FnOnce()) {
        let mut state = self.login.lock().expect("chatgpt login lock");
        if !Arc::ptr_eq(&state.selection, &snapshot.selection) {
            return;
        }
        let Ok(Some(current)) = self.reconcile(&mut state) else {
            return;
        };
        if Arc::ptr_eq(&current.source, &snapshot.login.source)
            && Arc::ptr_eq(&current.effective, &snapshot.login.effective)
        {
            update();
        }
    }

    /// 发送前取凭据(§7):access token 余量 < 5 分钟先主动刷新。
    pub async fn ready(&self) -> Option<CodexTokens> {
        self.ready_snapshot()
            .await
            .map(|snapshot| snapshot.login.effective.as_ref().clone())
    }

    pub async fn ready_snapshot(&self) -> Option<LoginSnapshot> {
        let snapshot = self.current()?;
        let login = &snapshot.login;
        let needs_refresh = if login.source.kind == LoginSource::OpenCode {
            login.expires_ms.map_or_else(
                || cgpt::needs_refresh(&snapshot.access_token, cgpt::unix_now()),
                |expires| expires.saturating_sub(unix_now_ms()) < 300_000,
            )
        } else {
            cgpt::needs_refresh(&snapshot.access_token, cgpt::unix_now())
        };
        if needs_refresh {
            return self.refresh_snapshot(snapshot).await;
        }
        Some(snapshot)
    }

    /// 强制刷新(401 兜底,§7:刷新一次重试)。失败返回 None,
    /// 由候选循环把 401 交回失败分类换源。token 值不落日志。
    pub async fn refresh(&self, rejected_access: &str) -> Option<CodexTokens> {
        let snapshot = self.current()?;
        if snapshot.access_token != rejected_access {
            return Some(snapshot.login.effective.as_ref().clone());
        }
        self.refresh_snapshot(snapshot)
            .await
            .map(|snapshot| snapshot.login.effective.as_ref().clone())
    }

    pub async fn refresh_snapshot(&self, snapshot: LoginSnapshot) -> Option<LoginSnapshot> {
        if snapshot.refreshed {
            return None;
        }
        let _guard = self.refresh_lock.lock().await;
        let current = self.current()?;
        if !Arc::ptr_eq(&snapshot.selection, &current.selection) {
            return None;
        }
        if !Arc::ptr_eq(&snapshot.login.source, &current.login.source) {
            return (snapshot.login.source.kind == LoginSource::Codex).then_some(current);
        }
        let login = current.login;
        let old = &login.effective;
        if !Arc::ptr_eq(&login.effective, &snapshot.login.effective) {
            return Some(LoginSnapshot {
                selection: current.selection,
                login,
                refreshed: true,
            });
        }
        let kind = login.source.kind;
        if old.refresh_token.is_empty() {
            eprintln!("aubridge: {} 登录缺少刷新凭据，请重新登录", kind.as_str());
            return None;
        }
        let (outcome, expires_in) = match kind {
            LoginSource::Codex => (
                refresh_via_http(
                    &self.http,
                    &self.token_endpoint,
                    CODEX_CLIENT_ID,
                    &old.refresh_token,
                )
                .await,
                None,
            ),
            LoginSource::OpenCode => match refresh_opencode_openai_via_http(
                &self.http,
                &self.token_endpoint,
                &old.refresh_token,
            )
            .await
            {
                Ok(OpenCodeRefreshOutcome::Refreshed {
                    access,
                    refresh,
                    id_token,
                    expires_in,
                }) => (
                    Ok(RefreshOutcome::Refreshed {
                        access,
                        refresh,
                        id_token,
                    }),
                    expires_in,
                ),
                Ok(OpenCodeRefreshOutcome::Revoked) => (Ok(RefreshOutcome::Revoked), None),
                Err(error) => (Err(error), None),
            },
        };
        let mut cache = self.login.lock().expect("chatgpt login lock");
        if !Arc::ptr_eq(&snapshot.selection, &cache.selection) {
            return None;
        }
        let mut current = self.reconcile(&mut cache).ok()??;
        // Logout or a cache invalidation may have been observed even when
        // the file has since returned to identical credentials.
        if !Arc::ptr_eq(&login.source, &current.source) {
            return (kind == LoginSource::Codex).then(|| LoginSnapshot {
                selection: snapshot.selection,
                login: current.clone(),
                refreshed: true,
            });
        }
        let fresh = match outcome {
            Ok(RefreshOutcome::Refreshed {
                access,
                refresh,
                id_token,
            }) => Arc::new(CodexTokens {
                access_token: access,
                refresh_token: if refresh.is_empty() {
                    old.refresh_token.clone()
                } else {
                    refresh
                },
                id_token: if id_token.is_empty() {
                    old.id_token.clone()
                } else {
                    id_token
                },
                account_id: old.account_id.clone(),
            }),
            Ok(RefreshOutcome::Revoked) => {
                eprintln!("aubridge: {} 登录已失效，请重新登录", kind.as_str());
                return None;
            }
            Err(_) => {
                eprintln!("aubridge: {} 登录刷新失败，请重试或重新登录", kind.as_str());
                return None;
            }
        };
        let share_login = self.share_login.read().expect("share_login lock");
        if *share_login && kind == LoginSource::Codex {
            match write_back_codex_tokens(
                &self.auth_json,
                &self.config_toml,
                &login.source.tokens,
                &fresh,
                cgpt::unix_now(),
            ) {
                Ok(true) => {
                    current.source = Arc::new(FileLogin {
                        kind,
                        tokens: fresh.as_ref().clone(),
                        expires_ms: None,
                    });
                }
                Ok(false) => {
                    return Some(LoginSnapshot {
                        selection: snapshot.selection,
                        login: self.reconcile(&mut cache).ok()??.clone(),
                        refreshed: true,
                    });
                }
                Err(e) => {
                    eprintln!("aubridge: auth.json 写回失败: {e}");
                    current = self.reconcile(&mut cache).ok()??;
                    if !Arc::ptr_eq(&login.source, &current.source) {
                        return Some(LoginSnapshot {
                            selection: snapshot.selection,
                            login: current.clone(),
                            refreshed: true,
                        });
                    }
                }
            }
        }
        current.effective = fresh.clone();
        current.expires_ms = if kind == LoginSource::OpenCode {
            expires_in
                .and_then(|seconds| seconds.checked_mul(1000))
                .and_then(|duration| unix_now_ms().checked_add(duration))
        } else {
            None
        };
        Some(LoginSnapshot {
            selection: snapshot.selection,
            login: current.clone(),
            refreshed: true,
        })
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Responses 上游请求构造(§6.3 头全集:Bearer / chatgpt-account-id /
/// originator / UA 对齐 Codex CLI / OpenAI-Beta)。
pub fn responses_builder(
    client: &reqwest::Client,
    url: &str,
    tokens: &CodexTokens,
) -> reqwest::RequestBuilder {
    let mut b = client
        .post(url)
        .header("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(600))
        .bearer_auth(&tokens.access_token)
        .header("originator", CODEX_ORIGINATOR)
        .header("user-agent", CODEX_USER_AGENT)
        .header("OpenAI-Beta", CODEX_OPENAI_BETA);
    if let Some(acc) = cgpt::resolve_account_id(tokens) {
        b = b.header("chatgpt-account-id", acc);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::{collections::BTreeMap, fs, time::Duration};
    use tokio::sync::Notify;

    use crate::state::GatewayState;

    fn jwt_with_exp(exp: u64) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let payload = format!(r#"{{"exp":{exp}}}"#);
        let mut encoded = String::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in payload.bytes() {
            buffer = (buffer << 8) | u32::from(byte);
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                encoded.push(ALPHABET[((buffer >> bits) & 0x3f) as usize] as char);
            }
        }
        if bits > 0 {
            encoded.push(ALPHABET[((buffer << (6 - bits)) & 0x3f) as usize] as char);
        }
        format!("e30.{encoded}.sig")
    }

    struct OpenCodeFixture {
        dir: PathBuf,
        codex: PathBuf,
        auth: PathBuf,
    }

    impl OpenCodeFixture {
        fn new(tag: &str, access: &str, expiry: Option<u64>) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aubridge-oc-channel-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let codex = dir.join("codex.json");
            let auth = dir.join("opencode.json");
            fs::write(
                &codex,
                json!({"tokens":{"access_token":access,"refresh_token":"codex-refresh","account_id":"codex-account"}}).to_string(),
            ).unwrap();
            let mut document = json!({"openai":{
                "type":"oauth", "access":access, "refresh":"oc-refresh", "accountId":"oc-account"
            }});
            if let Some(expiry) = expiry {
                document["openai"]["expires"] = json!(expiry);
            }
            fs::write(&auth, document.to_string()).unwrap();
            Self { dir, codex, auth }
        }

        fn channel(&self, endpoint: &str) -> ChatGptChannel {
            let mut channel = ChatGptChannel::new(self.codex.clone(), self.dir.join("codex.toml"))
                .with_opencode_auth(self.auth.clone())
                .with_token_endpoint(endpoint);
            channel.http = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            channel.set_login_source(Some(LoginSource::OpenCode));
            channel
        }
    }

    impl Drop for OpenCodeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn opencode_expiry_uses_milliseconds_before_access_jwt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let oauth = Router::new().route(
            "/token",
            post({
                let count = count.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"access_token":"fresh-opaque","expires_in":3600}))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, oauth).await.unwrap() });
        for (access, expiry, refreshes) in [
            ("opaque".to_string(), Some(unix_now_ms() + 3_600_000), 0),
            (jwt_with_exp(1), Some(unix_now_ms() + 3_600_000), 0),
            (jwt_with_exp(cgpt::unix_now() + 3600), Some(0), 1),
            (jwt_with_exp(1), None, 1),
            ("opaque".to_string(), None, 0),
        ] {
            let fixture = OpenCodeFixture::new("expiry", &access, expiry);
            let channel = fixture.channel(&endpoint);
            let before = count.load(Ordering::SeqCst);
            let first = channel.ready().await.unwrap();
            let second = channel.ready().await.unwrap();
            assert_eq!(count.load(Ordering::SeqCst) - before, refreshes);
            assert_eq!(first.access_token, second.access_token);
            assert_eq!(
                first.access_token,
                if refreshes == 1 {
                    "fresh-opaque"
                } else {
                    &access
                }
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn opencode_refresh_is_single_flight_and_read_only_even_with_codex_sharing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = OpenCodeFixture::new("single-flight", "opaque", Some(0));
        let codex_before = fs::read(&fixture.codex).unwrap();
        let oc_before = fs::read(&fixture.auth).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let oauth = Router::new().route("/token", post({
            let count = count.clone();
            move |headers: axum::http::HeaderMap, body: String| {
                let count = count.clone();
                async move {
                    assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
                    assert!(body.contains("refresh_token=oc-refresh"));
                    assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"access_token":"fresh-opaque","refresh_token":"rotated","expires_in":3600}))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, oauth).await.unwrap() });
        let channel = fixture.channel(&endpoint);
        channel.set_share_login(true);
        let first = channel.current().unwrap();
        let second = channel.current().unwrap();
        let (one, two) = tokio::join!(
            channel.refresh_snapshot(first),
            channel.refresh_snapshot(second)
        );
        let one = one.unwrap();
        assert_eq!(one.access_token, "fresh-opaque");
        assert_eq!(two.unwrap().access_token, "fresh-opaque");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(channel.refresh_snapshot(one).await.is_none());
        assert_eq!(fs::read(&fixture.auth).unwrap(), oc_before);
        channel.set_login_source(Some(LoginSource::OpenCode));
        assert_eq!(channel.ready().await.unwrap().access_token, "fresh-opaque");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let mut unrelated: Value = serde_json::from_slice(&oc_before).unwrap();
        unrelated["unrelated"] = json!({"type":"api","key":"unrelated-fixture"});
        fs::write(&fixture.auth, unrelated.to_string()).unwrap();
        assert_eq!(channel.ready().await.unwrap().access_token, "fresh-opaque");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(&fixture.codex).unwrap(), codex_before);
        assert_eq!(
            unrelated["openai"],
            serde_json::from_slice::<Value>(&oc_before).unwrap()["openai"]
        );
        assert!(!fixture.dir.join("stash").exists());
        server.abort();
    }

    #[tokio::test]
    async fn opencode_refresh_drops_late_reply_after_source_change_logout_or_rotation() {
        for action in ["source", "logout", "rotation", "corrupt"] {
            let fixture = OpenCodeFixture::new(action, "shared-access", None);
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let oauth = Router::new().route("/token", post({
                let started = started.clone();
                let release = release.clone();
                move || {
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        started.notify_one();
                        release.notified().await;
                        Json(json!({"access_token":"late-access","refresh_token":"late-refresh","expires_in":3600}))
                    }
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, oauth).await.unwrap() });
            let channel = Arc::new(fixture.channel(&endpoint));
            channel.set_share_login(true);
            let snapshot = channel.ready_snapshot().await.unwrap();
            let refreshing = tokio::spawn({
                let channel = channel.clone();
                async move { channel.refresh_snapshot(snapshot).await }
            });
            tokio::time::timeout(Duration::from_secs(3), started.notified())
                .await
                .unwrap();
            match action {
                "source" => channel.set_login_source(Some(LoginSource::Codex)),
                "logout" => fs::remove_file(&fixture.auth).unwrap(),
                "rotation" => fs::write(&fixture.auth, json!({"openai":{
                    "type":"oauth", "access":"shared-access", "refresh":"new-refresh", "accountId":"new-account"
                }}).to_string()).unwrap(),
                _ => fs::write(&fixture.auth, "{").unwrap(),
            }
            let codex_before = fs::read(&fixture.codex).unwrap();
            let oc_before = fs::read(&fixture.auth).ok();
            release.notify_one();
            assert!(tokio::time::timeout(Duration::from_secs(3), refreshing)
                .await
                .unwrap()
                .unwrap()
                .is_none());
            assert_eq!(fs::read(&fixture.codex).unwrap(), codex_before);
            assert_eq!(fs::read(&fixture.auth).ok(), oc_before);
            match action {
                "source" => assert_eq!(channel.ready().await.unwrap().account_id, "codex-account"),
                "rotation" => assert_eq!(channel.ready().await.unwrap().account_id, "new-account"),
                _ => assert!(channel.ready().await.is_none()),
            }
            server.abort();
        }
    }

    async fn ready_with_refresh_counter(
        tag: &str,
        access: &str,
        id_token: &str,
    ) -> (CodexTokens, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = std::env::temp_dir().join(format!("aubridge-ready-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        let original = json!({"auth_mode":"chatgpt","tokens":{
            "access_token":access,"refresh_token":"fixture-refresh","id_token":id_token
        }})
        .to_string();
        fs::write(&auth, &original).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let oauth = Router::new().route(
            "/oauth/token",
            post({
                let count = count.clone();
                move |Json(body): Json<Value>| async move {
                    assert_eq!(body["refresh_token"], "fixture-refresh");
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"access_token":"fresh-access","refresh_token":"fresh-refresh"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, oauth).await.unwrap() });
        let mut channel = ChatGptChannel::new(auth.clone(), dir.join("config.toml"))
            .with_token_endpoint(endpoint);
        channel.http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();

        let ready = tokio::time::timeout(Duration::from_secs(3), channel.ready())
            .await
            .unwrap()
            .unwrap();
        let refresh_count = count.load(Ordering::SeqCst);
        assert_eq!(fs::read_to_string(&auth).unwrap(), original);
        server.abort();
        fs::remove_dir_all(dir).unwrap();
        (ready, refresh_count)
    }

    #[tokio::test]
    async fn ready_uses_valid_access_without_refresh_when_id_expired() {
        let access = jwt_with_exp(cgpt::unix_now() + 3600);
        let id_token = jwt_with_exp(1);
        let (ready, refresh_count) =
            ready_with_refresh_counter("valid-access", &access, &id_token).await;
        assert_eq!(refresh_count, 0, "a valid access token needs no refresh");
        assert_eq!(ready.access_token, access);
        assert_eq!(ready.id_token, id_token);
    }

    #[tokio::test]
    async fn ready_refreshes_expiring_access_when_id_is_fresh() {
        let access = jwt_with_exp(cgpt::unix_now() + 60);
        let id_token = jwt_with_exp(cgpt::unix_now() + 3600);
        let (ready, refresh_count) =
            ready_with_refresh_counter("expiring-access", &access, &id_token).await;
        assert_eq!(refresh_count, 1, "an expiring access token must refresh");
        assert_eq!(ready.access_token, "fresh-access");
        assert_eq!(ready.refresh_token, "fresh-refresh");
        assert_eq!(ready.id_token, id_token);
    }

    #[tokio::test]
    async fn ready_uses_opaque_access_without_refresh_when_id_expired() {
        let id_token = jwt_with_exp(1);
        let (ready, refresh_count) =
            ready_with_refresh_counter("opaque-access", "opaque-access", &id_token).await;
        assert_eq!(
            refresh_count, 0,
            "unreadable expiry waits for the 401 fallback"
        );
        assert_eq!(ready.access_token, "opaque-access");
        assert_eq!(ready.id_token, id_token);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restore_rejects_refresh_reply_for_restored_login_generation() {
        let dir =
            std::env::temp_dir().join(format!("aubridge-restore-refresh-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        let original = r#"{"tokens":{"access_token":"backup-access","refresh_token":"backup-refresh"},"marker":"backup"}"#;
        let live = original.replace(r#""marker":"backup""#, r#""marker":"live""#);
        fs::write(&auth, original).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut channel = ChatGptChannel::new(auth.clone(), dir.join("config.toml"))
            .with_token_endpoint(format!("{base}/oauth/token"));
        channel.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let state = Arc::new(
            GatewayState::from_manifests(BTreeMap::new(), "restore-fixture")
                .with_codex_channel(channel)
                .with_settings(dir.join("config.json"), dir.join("stash")),
        );
        state.set_share_login(true).unwrap();
        fs::write(&auth, &live).unwrap();

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let oauth = Router::new().route(
            "/oauth/token",
            post({
                let started = started.clone();
                let release = release.clone();
                move |Json(body): Json<Value>| async move {
                    assert_eq!(body["refresh_token"], "backup-refresh");
                    started.notify_one();
                    release.notified().await;
                    Json(json!({"access_token":"late-access","refresh_token":"late-refresh"}))
                }
            }),
        );
        let app = crate::app(state.clone()).merge(oauth);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let refreshing = tokio::spawn({
            let state = state.clone();
            async move { state.chatgpt_refresh("backup-access".into()).await }
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();

        let restore = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
            .post(format!("{base}/admin/settings/restore"))
            .bearer_auth("restore-fixture-admin")
            .send()
            .await;
        let restored_bytes = fs::read_to_string(&auth).unwrap();
        release.notify_one();
        let receipt: Value = restore
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(receipt, json!({"restored":true}));
        assert_eq!(restored_bytes, original);
        let returned = tokio::time::timeout(Duration::from_secs(3), refreshing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(returned.access_token, "backup-access");
        assert_eq!(
            state.chatgpt_ready().await.unwrap().access_token,
            "backup-access"
        );
        assert_eq!(fs::read_to_string(&auth).unwrap(), original);
        assert!(state.share_login());
        server.abort();
        fs::remove_dir_all(dir).unwrap();
    }
}

//! 网关共享状态:rest/亲和表 write-behind 持久化(§8.1/§8.4,
//! engineering-constraints B-8——M2 起挂 ledger 落 SQLite,重启不丢)。

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};

use manifest::schema::{Manifest, ProviderKind};
use manifest::state::{ModelCooldown, ProviderState};
use registry::plugins::{scan_dir, EnabledState};
use router::UpstreamSignal;

use crate::catalog::{self, OpenRouterCatalog};
use crate::chatgpt::{ChatGptChannel, LoginMetadata, LoginSnapshot, LoginSource};

#[path = "opencode_keys.rs"]
mod opencode_keys;

#[path = "directory.rs"]
pub(crate) mod directory;
pub(crate) mod pricing;
#[path = "quota.rs"]
pub(crate) mod quota;
#[path = "request_body.rs"]
pub(crate) mod request_body;
pub use request_body::{UpdateActivityLease, UpdateMaintenanceLease};
#[path = "source_health.rs"]
pub(crate) mod source_health;
#[path = "usage_policy.rs"]
pub(crate) mod usage_policy;
#[path = "validation_evidence.rs"]
mod validation_evidence;
#[path = "work_config_backup.rs"]
pub(crate) mod work_config_backup;

pub(crate) type ConnectionVersion = Arc<()>;

/// 快照 + 状态转移的共享宿主。registry 的完整形态(探活/轮询)M1
/// 后续迭代,当前由 gateway 在每次失败/成功时就地转移。
pub struct GatewayState {
    pub(crate) agent_projects: crate::agent_project::ProjectService,
    pub(crate) native_sessions: crate::native_sessions::Store,
    backup: work_config_backup::BackupService,
    pub(super) source_health: source_health::Observations,
    pub(super) usage_policy: RwLock<usage_policy::UsagePolicy>,
    pub(crate) agent_validation: crate::agent_validation::ValidationService,
    pub(crate) compaction: Mutex<crate::compaction::State>,
    token: String,
    admin_token: String,
    desktop_access: bool,
    pub(crate) ledger: Option<std::sync::Mutex<ledger::Ledger>>,
    /// 每源 API key 缓存(keyring 预载 + set/clear 双写,变更记录
    /// 32-①):启动时全量 keyring 读一次进缓存,运行期取 key 零钥匙
    /// 串 IO;set/clear 同步刷缓存(保存即生效,免重启)。
    keys: RwLock<BTreeMap<String, String>>,
    /// Restored custom connections never acquire an unconfirmed environment credential.
    restored_keychain_only: RwLock<BTreeSet<String>>,
    opencode_api_auth: Option<PathBuf>,
    /// Last effective key, including an observed absence. Never persisted or logged.
    opencode_api_observed: Mutex<Option<Option<String>>>,
    /// key 存储抽象(§7):None = 未接线(测试默认态,key 路由回
    /// "未配置"),生产由组装层注入 OsKeyring。
    key_store: Option<std::sync::Arc<dyn credentials::KeyStore>>,
    manifests: RwLock<BTreeMap<String, Manifest>>,
    // Connection changes acquire manifests, then versions, before changing keys or provider state.
    connection_versions: RwLock<BTreeMap<String, ConnectionVersion>>,
    providers: RwLock<BTreeMap<String, ProviderState>>,
    /// 插件目录(未配置 = None,插件页显示"未配置")。
    plugins_dir: Option<PathBuf>,
    /// 订阅源并发槽位(§8.5:每账号上限 2,防单账号被打爆触发
    /// 厂商风控;免费源不限——厂商免费档本来就会限)。
    slots: RwLock<BTreeMap<String, std::sync::Arc<tokio::sync::Semaphore>>>,
    /// 会话亲和表(§8.4):key → 粘性。write-behind 持久化(挂
    /// ledger 时落 SQLite,重启不丢"谁答过")。
    sticks: RwLock<BTreeMap<String, router::affinity::Stick>>,
    /// write-behind 脏集(§8.1/§8.4):状态变更只标脏,周期/显式
    /// flush 合并落盘——避免每请求写库放大。行 = 待写 provider id;
    /// `STICKS_DIRTY` 哨兵行标记亲和表整体待写(表 ≤512 且已裁剪,
    /// 整表 upsert 即合并写)。未挂 ledger 时标脏是空操作。
    dirty: RwLock<BTreeSet<String>>,
    /// 决策和实际用量一起排队，避免两个视图看到不同的结算结果。
    pending_requests: Mutex<Vec<ledger::CompletedRequest>>,
    /// D59: only this process can prove continuous retained coverage.
    analytics_retention_since: AtomicU64,
    /// codex wham/usage 主动查询(§8.2 第二数据源;缓存 3min/负缓存
    /// 1min)。默认不挂——未登录/测试环境零外呼,由组装层显式接线。
    wham: Option<credentials::usage::WhamFetcher>,
    /// key plan 配额查询拉取器(quota-plan-design §3.3;按端点缓存
    /// 复用——缓存/负缓存/单飞在 PlanFetcher 内)。锁内 get-or-insert、
    /// 锁外 await,不持锁跨异步。
    plan_fetchers: RwLock<BTreeMap<String, std::sync::Arc<credentials::planquota::PlanFetcher>>>,
    openrouter: Option<credentials::usage::OpenRouterKeyFetcher>,
    openrouter_catalog: Option<OpenRouterCatalog>,
    directories: directory::Directories,
    admission: request_body::Admission,
    /// chatgpt 订阅凭据通道(§7:OAuth,不走 keys/keyring 注入)。
    chatgpt: Arc<ChatGptChannel>,
    pub(crate) claude_cli: crate::claude_cli::Channel,
    /// 设置存储(§7 写回 opt-in,engineering-constraints B-7):
    /// (config.json 路径, stash 目录)。None = 未接线(测试默认态),
    /// 开关恒为只读。开关运行态在 chatgpt 通道里,持久化在这里。
    settings: Option<(PathBuf, PathBuf)>,
    /// cc-switch 数据库路径(cc-switch-import-design:None = 未接线,
    /// 导入 API 返回空列表)。
    cc_switch_db: Option<PathBuf>,
    settings_io: Mutex<()>,
    paused_providers: RwLock<BTreeSet<String>>,
    claude_cli_enabled: AtomicBool,
    /// agent 接线路径(§8.6,变更记录 30):None = 未注入(测试
    /// 默认态/smoke 自检壳不碰用户真实配置),agents admin 路由
    /// 回"未配置"。接线器按需现建(`agents::wirings`)——端口
    /// serve() 绑定后才权威,不能启动时焊死。
    agents: Option<agents::AgentsPaths>,
    agents_io: Mutex<()>,
    /// 自定义源 id 集(变更记录 32-①):config.json 装载/运行时
    /// 添加的源;删除路由只对这批放行(preset/插件不可经 UI 删)。
    custom_ids: RwLock<BTreeSet<String>>,
    /// 模型分组 store(ui-design §6.3,变更记录 32-③):config.json
    /// 装载/运行时增删;请求 model 命中分组名时按成员+规则解析候选。
    model_groups: RwLock<Vec<credentials::settings::ModelGroup>>,
    /// 分组轮流计数(GroupRule::RoundRobin 用):每请求自增,尽力
    /// 语义不持久(重启归零)。
    group_rotation: RwLock<BTreeMap<String, u64>>,
    /// 插件源 id 集(变更记录 32-①):plugins 目录并入的源,
    /// provider 行 source 字段用(preset/custom 之外即插件)。
    plugin_ids: RwLock<BTreeSet<String>>,
    /// 实际监听端口(§4):默认 3427,serve() 绑定后回写实际值;
    /// 端口回退时接线 base_url 写实际端口(变更记录 30 ②)。
    port: RwLock<u16>,
    /// 网关软开关(ui-design §6.4,变更记录 32-④):off 时推理路径
    /// 新请求 503,进程不退(admin 面照常,可再开)。不持久化——
    /// 重启回到默认开(开关是会话级操作,不是安装状态)。
    serving: tokio::sync::watch::Sender<bool>,
    /// 共享 HTTP client(变更记录 31 批 1):连接池复用,替代
    /// upstream 每请求 `Client::new()`(每请求新建 = 放弃连接复用)。
    http: reqwest::Client,
}

impl GatewayState {
    pub fn with_admin_token(mut self, token: String) -> Self {
        self.admin_token = token;
        self
    }

    pub fn with_desktop_access(mut self) -> Self {
        self.desktop_access = true;
        self
    }

    pub(crate) fn allows_origin(&self, origin: &str) -> bool {
        self.desktop_access && crate::security::DESKTOP_ORIGINS.contains(&origin)
    }

    pub fn from_manifests(manifests: BTreeMap<String, Manifest>, token: &str) -> Self {
        Self::with_keys(manifests, token, BTreeMap::new())
    }

    /// 带源 key 构造(凭据注入通道)。
    pub fn with_keys(
        manifests: BTreeMap<String, Manifest>,
        token: &str,
        keys: BTreeMap<String, String>,
    ) -> Self {
        let plugins_dir: Option<PathBuf> = None;
        Self::assemble(manifests, token, keys, plugins_dir)
    }

    /// 挂插件目录(§9):扫描 + 合并 enabled 插件进 manifests,
    /// 提供运行时开关(/admin/plugins 页的数据源)。
    pub fn with_plugins(mut self, dir: PathBuf) -> Self {
        self.plugins_dir = Some(dir.clone());
        let scan = scan_dir(&dir);
        let enabled = EnabledState::new(dir.join("enabled.json"));
        let enabled_plugins: Vec<Manifest> = scan
            .plugins
            .into_iter()
            .filter(|m| {
                enabled.is_enabled(&m.id) || (enabled.auto_enable() && !enabled.is_disabled(&m.id))
            })
            .collect();
        self.merge_plugins(enabled_plugins);
        self
    }

    /// 插件目录(插件 admin 路由用)。
    pub fn plugins_dir(&self) -> Option<&PathBuf> {
        self.plugins_dir.as_ref()
    }

    /// 注入 chatgpt 凭据通道(验收/测试;默认 `~/.codex/auth.json`)。
    pub fn with_codex_channel(mut self, channel: ChatGptChannel) -> Self {
        self.chatgpt = Arc::new(channel);
        self
    }

    pub fn with_claude_cli(mut self) -> Self {
        self.claude_cli =
            crate::claude_cli::Channel::native().with_update_gate(self.admission.activity_gate());
        self
    }

    /// Follow the selected OpenCode file read-only, independently of OAuth selection.
    pub fn with_opencode_api_auth(mut self, path: PathBuf) -> Self {
        self.opencode_api_auth = Some(path);
        self
    }

    /// cc-switch 数据库注入(导入 API 的读取目标;测试注临时 db,
    /// 生产由 Runtime 用 `~/.cc-switch/cc-switch.db` 注入)。
    pub fn with_cc_switch_db(mut self, path: PathBuf) -> Self {
        self.cc_switch_db = Some(path);
        self
    }

    /// plan 配额查询端点覆盖(quota-plan-design §3.3;测试注入:
    /// base 命中 plan_source 的源改用自定义 fetcher——本地假
    /// upstream。生产不调用)。
    pub fn with_plan_fetcher_for(
        self,
        base: &str,
        fetcher: std::sync::Arc<credentials::planquota::PlanFetcher>,
    ) -> Self {
        if let Some((endpoint, auth)) = credentials::planquota::plan_source(base) {
            self.plan_fetchers
                .write()
                .expect("plan fetchers lock")
                .insert(format!("{endpoint}|{auth:?}"), fetcher);
        }
        self
    }

    /// 导入 API 的读取目标;None = 未接线(空列表)。
    pub fn cc_switch_db(&self) -> Option<&Path> {
        self.cc_switch_db.as_deref()
    }

    /// 挂 codex wham 主动查询(§8.2 第二数据源;仪表盘触发)。
    /// 端点注入:生产用 `WhamFetcher::ENDPOINT`,测试用本地服务器。
    pub fn with_wham_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.wham = Some(credentials::usage::WhamFetcher::new(endpoint));
        self
    }

    pub fn with_openrouter_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.openrouter = Some(credentials::usage::OpenRouterKeyFetcher::new(endpoint));
        self
    }

    pub fn with_openrouter_catalog_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.openrouter_catalog = Some(OpenRouterCatalog::new(endpoint.into()));
        self
    }

    pub(crate) fn openrouter_catalog(&self) -> Option<catalog::View> {
        Some(
            self.openrouter_catalog
                .as_ref()?
                .view(crate::upstream::unix_now()),
        )
    }

    pub(crate) async fn refresh_openrouter_catalog(
        &self,
    ) -> Result<catalog::View, catalog::RefreshError> {
        let catalog = self
            .openrouter_catalog
            .as_ref()
            .filter(|_| self.manifest("openrouter").is_some())
            .ok_or(catalog::RefreshError::Unavailable)?;
        let _refresh = catalog
            .refresh
            .try_lock()
            .map_err(|_| catalog::RefreshError::Busy)?;
        let result = catalog.fetch(self.http()).await.and_then(|snapshot| {
            if let Some(ledger) = &self.ledger {
                let json = serde_json::to_string(&snapshot).map_err(|_| catalog::Issue::Storage)?;
                ledger
                    .lock()
                    .map_err(|_| catalog::Issue::Storage)?
                    .save_provider_catalog("openrouter", &json)
                    .map_err(|_| catalog::Issue::Storage)?;
            }
            self.apply_openrouter_catalog(&snapshot);
            Ok(snapshot)
        });
        catalog.complete(result);
        Ok(catalog.view(crate::upstream::unix_now()))
    }

    fn apply_openrouter_catalog(&self, catalog: &catalog::Snapshot) {
        // Routing takes the same lock order and sees one catalog generation.
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut providers = self.providers.write().expect("providers lock");
        if let (Some(manifest), Some(source)) = (
            manifests.get_mut("openrouter"),
            providers.get_mut("openrouter"),
        ) {
            manifest.free_models = catalog
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect();
            manifest.models_without_tools = catalog
                .models
                .iter()
                .filter(|model| !model.tools)
                .map(|model| model.id.clone())
                .collect();
            manifest.context_windows = catalog
                .models
                .iter()
                .filter_map(|model| model.context.map(|window| (model.id.clone(), window)))
                .collect();
            manifest.model_facts = catalog
                .models
                .iter()
                .map(|model| {
                    let mut facts = model.facts.clone();
                    facts.tools = Some(model.tools);
                    facts.context = model.context;
                    facts.protocol = Some(manifest::schema::WireProtocol::Chat);
                    facts.free_evidence = manifest::model::FreeEvidence::VendorZeroPrice;
                    facts.source = manifest::model::FactSource::VendorDirectory;
                    facts.observed_at = Some(catalog.fetched_at);
                    (model.id.clone(), facts)
                })
                .collect();
            source.models.clone_from(&manifest.free_models);
            source
                .model_cooldowns
                .retain(|model, _| source.models.contains(model));
        }
    }

    pub(crate) async fn openrouter_key_budget(
        &self,
    ) -> Option<credentials::usage::OpenRouterKeyBudget> {
        let fetcher = self.openrouter.as_ref()?;
        let manifest = self.manifest("openrouter")?;
        if !matches!(manifest.quota.source, manifest::schema::QuotaSource::Queryable(ref path)
            if path == "/api/v1/key")
        {
            return None;
        }
        let key = self
            .api_key_opt("openrouter")
            .filter(|key| !key.trim().is_empty())?;
        fetcher.fetch(self.http(), &key).await
    }

    /// 挂设置存储(§7 写回 opt-in):载入 config.json 并把开关状态
    /// 应用到 chatgpt 通道——重启后"与 CLI 共享登录"延续,不静默回读。
    /// 自定义源(变更记录 32-①)同场装载:config.json 里的 providers
    /// 经 registry::load 校验后并入候选;单条坏记录跳过(尽力语义,
    /// 不因手改坏一条拒绝整个网关启动)。
    pub fn with_settings(mut self, config: PathBuf, stash_dir: PathBuf) -> Self {
        #[cfg(unix)]
        if let Err(error) = credentials::stash::protect_existing_backup(&stash_dir) {
            eprintln!("aubridge: 收紧本机凭据备份权限失败: {error}");
        }
        let s = credentials::settings::Settings::load(&config);
        *self
            .restored_keychain_only
            .write()
            .expect("restored credentials lock") = s.restored_keychain_only.clone();
        *self.usage_policy.write().expect("cost policy lock") =
            credentials::settings::Settings::load_checked(&config)
                .map(|settings| usage_policy::UsagePolicy::from_settings(&settings))
                .unwrap_or_else(|_| usage_policy::UsagePolicy::invalid());
        self.claude_cli_enabled
            .store(s.claude_cli_enabled, Ordering::SeqCst);
        // 来源判定用 load_checked(不复用吞错默认):读失败 fail-closed,
        // 无效值 fail-closed,空串(从未选择,含文件缺失)走首启
        // 自动判定——Codex 登录可读则 codex,否则 OpenCode openai
        // 登录可读才 opencode,否则 codex(opencode-auth-design §4,
        // 2026-11-14)。启动判一次,运行期仍不自动切换。
        let login_source = match credentials::settings::Settings::load_checked(&config) {
            Ok(settings) if settings.chatgpt_login_source.is_empty() => {
                Some(self.chatgpt.detect_login_source())
            }
            Ok(settings) => LoginSource::parse(&settings.chatgpt_login_source),
            Err(_) => None,
        };
        self.chatgpt.set_login_source(login_source);
        let mut share_login = s.share_login;
        if share_login {
            if let Err(error) = credentials::stash::verify(&stash_dir) {
                share_login = false;
                eprintln!("aubridge: 共享登录以只读启动，备份不可用 · {error}");
            }
        }
        self.chatgpt.set_share_login(share_login);
        *self
            .paused_providers
            .write()
            .expect("paused providers lock") = s.paused_providers;
        let customs = s.providers.clone();
        // 分组 store(变更记录 32-③)同场装载:单条坏记录(未知
        // 规则)跳过(尽力语义,与自定义源同口径)。
        for g in &s.model_groups {
            if router::GroupRule::parse(&g.rule).is_none() {
                eprintln!("aubridge: 模型分组 {} 规则 {} 未知,跳过", g.name, g.rule);
            }
        }
        let groups: Vec<credentials::settings::ModelGroup> = s
            .model_groups
            .iter()
            .filter(|g| router::GroupRule::parse(&g.rule).is_some())
            .cloned()
            .collect();
        *self.model_groups.write().expect("model_groups lock") = groups;
        self.settings = Some((config, stash_dir));
        for c in &customs {
            match custom_manifest(c) {
                Ok(m) if self.manifest(&m.id).is_none() => self.merge_custom_provider(m),
                Ok(m) => eprintln!("aubridge: 忽略重复的自定义源 {}", m.id),
                Err(e) => eprintln!("aubridge: 自定义源 {} 装载失败,跳过: {e}", c.id),
            }
        }
        self.load_model_selections(&s.model_selections);
        self
    }

    /// 挂 key 存储(变更记录 32-①):启动全量预载 keyring 进 keys
    /// 缓存——转发路径取 key 不付钥匙串 IO;运行期 set/clear 走
    /// `set_provider_key`/`clear_provider_key` 双写。须在 manifests
    /// 定型后调用(自定义源经 with_settings 先并入)。
    pub fn with_key_store(mut self, store: std::sync::Arc<dyn credentials::KeyStore>) -> Self {
        let ids: Vec<String> = {
            let manifests = self.manifests.read().expect("manifests lock");
            manifests
                .values()
                .filter(|m| m.permissions.store_key)
                .map(|m| m.id.clone())
                .collect()
        };
        for id in ids {
            if let Some(k) = store.get(&id) {
                self.keys.write().expect("keys lock").insert(id, k);
            }
        }
        self.key_store = Some(store);
        self
    }

    /// 注入自定义 client(测试/组装层;默认见 assemble)。
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.http = client;
        self
    }

    /// 共享 client(内部克隆句柄,便宜)。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 挂 agent 接线路径(§8.6,变更记录 30 ③):组装层(aubridged)
    /// 注入用户真实路径;smoke 自检壳不注入。
    pub fn with_agents(mut self, paths: agents::AgentsPaths) -> Self {
        self.agents = Some(paths);
        self
    }

    /// agent 接线是否注入(admin 路由"未配置"口径,与设置页一致)。
    pub fn agents_configured(&self) -> bool {
        self.agents.is_some()
    }

    /// 实际监听端口(serve() 绑定后回写;默认 3427)。
    pub fn port(&self) -> u16 {
        *self.port.read().expect("port lock")
    }

    /// 端口回写(serve() 绑定后调用;§4 回退时接线 base_url 用
    /// 实际值)。
    pub fn set_port(&self, port: u16) {
        *self.port.write().expect("port lock") = port;
    }

    /// 网关软开关(变更记录 32-④):off = 推理路径 503,进程不退。
    pub fn is_serving(&self) -> bool {
        *self.serving.borrow()
    }

    /// 软开关切换(变更记录 32-④):不持久化,重启回默认开。
    pub fn set_serving(&self, on: bool) {
        self.serving.send_replace(on);
        if !on {
            self.claude_cli.cancel_all();
        }
    }

    pub fn subscribe_serving(&self) -> tokio::sync::watch::Receiver<bool> {
        self.serving.subscribe()
    }

    /// admin token(代理页接入方式展示用;调用方已通过 admin 校验,
    /// 回显非泄漏——与 provider key"只回布尔"红线不同口径)。
    pub fn admin_token(&self) -> String {
        self.admin_token.clone()
    }

    /// agents 页数据(变更记录 30):接线器状态(id/name/wired/
    /// detected/drift/hint/model/token)。OpenCode 模型列表 = 网关
    /// 聚合模型(id 即显示名)。未注入 → None(路由回空表)。
    /// model/token 编辑弹层数据源(变更记录 32-②):本 agent token
    /// = `aubridge-<id>`,可复制。
    pub fn agents_overview(&self) -> Option<Vec<serde_json::Value>> {
        let paths = self.agents.as_ref()?;
        let _guard = self.agents_io.lock().expect("agent config lock");
        let models = self
            .models()
            .into_iter()
            .map(|m| (m.clone(), m))
            .collect::<Vec<_>>();
        let token_key = self.token_key();
        Some(
            agents::wirings(paths, self.port(), models)
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id(),
                        "name": w.name(),
                        "wired": w.wired(),
                        "detected": w.detected(),
                        "discovery": crate::agent_validation::cached_discovery(self, w.id()),
                        "validation": crate::agent_validation::summary(self, w.id()),
                        "drift": w.drift().as_ref().map(drift_str),
                        "hint": w.hint(),
                        "launch_command": self.validation_launch_command(w.id()),
                        "model": w.current_model(),
                        "token": format!("{}-{}", token_key, w.id()),
                    })
                })
                .collect(),
        )
    }

    /// agent 接线开关(变更记录 30 ①)。错误统一返回给调用方 400
    /// (与插件开关同口径:未配置/未知 agent 是调用方问题,IO 失败
    /// 消息里带原因)。
    pub fn set_agent_wired(
        &self,
        id: &str,
        on: bool,
        preview: Option<&agents::DisconnectPreview>,
    ) -> Result<(), String> {
        let Some(paths) = self.agents.as_ref() else {
            return Err("agent 接线未配置".to_string());
        };
        let _guard = self.agents_io.lock().expect("agent config lock");
        if !on && id == "claude-code" {
            return self
                .claude_wiring()
                .ok_or("Claude Code 接线未配置")?
                .disconnect_reviewed(preview)
                .map_err(|error| error.to_string());
        }
        if !on && id == "opencode" {
            return self
                .opencode_wiring()
                .ok_or("OpenCode 接线未配置")?
                .disconnect_reviewed(preview)
                .map_err(|error| error.to_string());
        }
        if !on && id == "codex" {
            return self
                .codex_wiring()
                .ok_or("Codex 接线未配置")?
                .disconnect_reviewed(preview)
                .map_err(|error| error.to_string());
        }
        if !on && id == "gemini" {
            return self
                .gemini_wiring()
                .ok_or("Gemini CLI 接线未配置")?
                .disconnect_reviewed(preview)
                .map_err(|error| error.to_string());
        }
        let models = self
            .models()
            .into_iter()
            .map(|m| (m.clone(), m))
            .collect::<Vec<_>>();
        let w = agents::wirings(paths, self.port(), models)
            .into_iter()
            .find(|w| w.id() == id)
            .ok_or_else(|| format!("未知 agent: {id}"))?;
        let action = if on { "接线" } else { "断开" };
        if on {
            w.apply()
                .map_err(|e| format!("agent {id} {action}失败: {e}"))
        } else {
            w.disconnect()
                .map_err(|e| format!("agent {id} {action}失败: {e}"))
        }
    }

    fn claude_wiring(&self) -> Option<agents::ClaudeCode> {
        let paths = self.agents.as_ref()?;
        Some(agents::ClaudeCode::new(
            paths.claude_settings.clone(),
            paths.state_dir.clone(),
            self.port(),
        ))
    }

    fn codex_wiring(&self) -> Option<agents::codex::Codex> {
        let paths = self.agents.as_ref()?;
        Some(agents::codex::Codex::new(
            paths.codex_config.clone(),
            paths.state_dir.clone(),
            self.port(),
        ))
    }

    fn gemini_wiring(&self) -> Option<agents::gemini::GeminiCli> {
        let paths = self.agents.as_ref()?;
        Some(agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            self.port(),
        ))
    }

    pub(crate) fn repair_gemini_port(&self) -> std::io::Result<()> {
        let _guard = self.agents_io.lock().expect("agent config lock");
        if let Some(wiring) = self.gemini_wiring() {
            wiring.repair_port()?;
        }
        Ok(())
    }

    fn managed_agent_models(&self) -> impl Iterator<Item = (&'static str, String)> {
        let claude = self
            .claude_wiring()
            .filter(|wiring| wiring.wired())
            .and_then(|wiring| wiring.current_model());
        let codex = self
            .codex_wiring()
            .filter(|wiring| wiring.wired())
            .and_then(|wiring| wiring.current_model());
        let opencode = self.agents.as_ref().and_then(|paths| {
            agents::opencode::OpenCode::new(
                paths.opencode_config.clone(),
                paths.state_dir.clone(),
                self.port(),
            )
            .current_model()
        });
        [
            ("Claude Code (claude-code)", claude),
            ("Codex (codex)", codex),
            ("OpenCode (opencode)", opencode),
            (
                "Gemini CLI (gemini)",
                self.gemini_wiring()
                    .filter(|w| w.wired())
                    .and_then(|w| w.current_model()),
            ),
        ]
        .into_iter()
        .filter_map(|(client, model)| model.map(|model| (client, model)))
    }

    fn opencode_wiring(&self) -> Option<agents::opencode::OpenCode> {
        let paths = self.agents.as_ref()?;
        let models = self
            .models()
            .into_iter()
            .map(|id| (id.clone(), id))
            .collect();
        Some(
            agents::opencode::OpenCode::new(
                paths.opencode_config.clone(),
                paths.state_dir.clone(),
                self.port(),
            )
            .with_models(models)
            .with_model_facts(self.model_facts()),
        )
    }

    pub(crate) fn agent_disconnect_preview(
        &self,
        id: &str,
    ) -> Result<agents::DisconnectPreview, String> {
        let _guard = self.agents_io.lock().expect("agent config lock");
        match id {
            "claude-code" => self
                .claude_wiring()
                .ok_or("Claude Code 接线未配置")?
                .disconnect_preview(),
            "opencode" => self
                .opencode_wiring()
                .ok_or("OpenCode 接线未配置")?
                .disconnect_preview(),
            "codex" => self
                .codex_wiring()
                .ok_or("Codex 接线未配置")?
                .disconnect_preview(),
            "gemini" => self
                .gemini_wiring()
                .ok_or("Gemini CLI 接线未配置")?
                .disconnect_preview(),
            _ => return Err("该 Agent 暂无断开预览".into()),
        }
        .map_err(|error| error.to_string())
    }

    pub(crate) fn repair_claude_port(&self) -> std::io::Result<()> {
        let _guard = self.agents_io.lock().expect("agent config lock");
        if let Some(wiring) = self.claude_wiring() {
            wiring.repair_port()?;
        }
        Ok(())
    }

    pub(crate) fn repair_opencode(&self) -> std::io::Result<()> {
        let _guard = self.agents_io.lock().expect("agent config lock");
        if let Some(wiring) = self.opencode_wiring() {
            wiring.repair()?;
        }
        Ok(())
    }

    pub(crate) fn repair_codex_port(&self) -> std::io::Result<()> {
        let _guard = self.agents_io.lock().expect("agent config lock");
        if let Some(wiring) = self.codex_wiring() {
            wiring.repair_port()?;
        }
        Ok(())
    }

    /// 设置 agent 使用的模型(变更记录 32-②):模型合法性 = 自动
    /// 别名或网关已知模型;写该 agent 配置的目标字段。无单一模型
    /// 字段由对应接线器管理；Gemini 显式写 model.name。
    pub fn set_agent_model(&self, id: &str, model: &str) -> Result<(), String> {
        let Some(paths) = self.agents.as_ref() else {
            return Err("agent 接线未配置".to_string());
        };
        let _guard = self.agents_io.lock().expect("agent config lock");
        let known = model == crate::AUTO_MODEL || self.models().iter().any(|m| m == model);
        if !known {
            return Err(format!("未知模型: {model}"));
        }
        let models = self
            .models()
            .into_iter()
            .map(|m| (m.clone(), m))
            .collect::<Vec<_>>();
        let w = agents::wirings(paths, self.port(), models)
            .into_iter()
            .find(|w| w.id() == id)
            .ok_or_else(|| format!("未知 agent: {id}"))?;
        if let Some((members, rule)) = self
            .group_of(model)
            .filter(|_| matches!(id, "claude-code" | "codex" | "opencode" | "gemini") && w.wired())
        {
            let manifests = self.manifests.read().expect("manifests lock");
            if !group_has_supply(&manifests, &members, rule) {
                return Err(format!(
                    "agent {id} 无法选择分组 {model}：没有符合规则的模型供应，请先保留供应或选择其他模型"
                ));
            }
        }
        w.set_model(model)
            .map_err(|e| format!("agent {id} 模型设置失败: {e}"))
    }

    /// agent 高级配置读取(Claude Code settings.json / Codex 受管
    /// profile)。密钥读取时打码,返回 revision 供保存时做乐观锁。
    pub fn agent_config(&self, id: &str) -> Result<serde_json::Value, String> {
        let Some(paths) = self.agents.as_ref() else {
            return Err("agent 接线未配置".to_string());
        };
        let _guard = self.agents_io.lock().expect("agent config lock");
        let view = agents::read_config(paths, self.port(), id)?;
        serde_json::to_value(view).map_err(|error| error.to_string())
    }

    /// agent 高级配置保存:revision 不匹配/格式非法 → 400 原因。
    pub fn set_agent_config(&self, id: &str, body: &str) -> Result<serde_json::Value, String> {
        let Some(paths) = self.agents.as_ref() else {
            return Err("agent 接线未配置".to_string());
        };
        let _guard = self.agents_io.lock().expect("agent config lock");
        let view = agents::write_config(paths, self.port(), id, body)?;
        serde_json::to_value(view).map_err(|error| error.to_string())
    }

    pub(crate) fn agents_paths(&self) -> Option<&agents::AgentsPaths> {
        self.agents.as_ref()
    }

    /// Provider 页数据(变更记录 32-①):全量源行——名称/类别/模型/
    /// base/key 状态/来源(preset|plugin|custom)。key 值永不出现。
    pub fn providers_overview(&self) -> Vec<serde_json::Value> {
        let manifests = self.manifests.read().expect("manifests lock").clone();
        let snapshots: BTreeMap<_, _> = self
            .snapshot()
            .into_iter()
            .map(|state| (state.id.clone(), state))
            .collect();
        let custom = self.custom_ids.read().expect("custom_ids lock").clone();
        let plugin = self.plugin_ids.read().expect("plugin_ids lock").clone();
        let now = crate::upstream::unix_now();
        manifests
            .values()
            .map(|m| {
                let mut effective=m.clone();
                self.apply_directory(&mut effective);
                let m=&effective;
                let source = if custom.contains(&m.id) {
                    "custom"
                } else if plugin.contains(&m.id) {
                    "plugin"
                } else {
                    "preset"
                };
                let (key, key_source) = self.resolve_api_key(&m.id);
                let key_set = key.is_some();
                let ready = crate::route_plan::credential_with_key(self, m, || key).is_some();
                let paused = self.provider_paused(&m.id);
                let routing_disabled = self
                    .providers
                    .read()
                    .expect("providers lock")
                    .get(&m.id)
                    .is_some_and(|state| !state.enabled);
                // 余量即时值(quota-plan-design §3.3;曲线之外的
                // Provider 页展示口径)。
                let (util_5h, util_7d, resets_at) = {
                    match snapshots.get(&m.id) {
                        Some(state) => (state.util_5h, state.util_7d, state.resets_at),
                        None => (None, None, None),
                    }
                };
                let free_requests =
                    (m.kind == manifest::schema::ProviderKind::FreeTier).then(|| {
                        m.free_models
                            .iter()
                            .map(|model| {
                                let block = self.route_block(&m.id, model, now);
                                let status = if paused {
                                    crate::route_plan::PROVIDER_PAUSED
                                } else if !ready {
                                    "missing-credential"
                                } else {
                                    block.map_or("eligible", |block| block.reason)
                                };
                                serde_json::json!({
                                    "model": model,
                                    "status": status,
                                    "retry_at": block.and_then(|block| block.retry_at),
                                })
                            })
                            .collect::<Vec<_>>()
                    });
                let routing_status = m
                    .free_models
                    .iter()
                    .map(|model| {
                        let block = if paused {
                            Some(crate::route_plan::Block::new(
                                crate::route_plan::PROVIDER_PAUSED,
                                None,
                            ))
                        } else if !ready {
                            Some(crate::route_plan::Block::new("missing-credential", None))
                        } else {
                            self.route_block(&m.id, model, now)
                        };
                        serde_json::json!({
                            "model": model,
                            "status": block.map_or("eligible", |block| block.reason),
                            "retry_at": block.and_then(|block| block.retry_at),
                        })
                    })
                    .collect::<Vec<_>>();
                let mut row = serde_json::json!({
                    "source_health": self.source_health(m, now),
                    "id": m.id,
                    "kind": m.kind,
                    "models": m.free_models,
                    "model_facts": self.configured_model_facts(m),
                    "model_availability": m.free_models.iter().map(|model|
                        (model.clone(),self.directory_availability(m,model))).collect::<BTreeMap<_,_>>(),
                    "base": m.provider.base,
                    "store_key": m.permissions.store_key,
                    "key_set": key_set,
                    "key_source": key_source,
                    "ready": ready,
                    "paused": paused,
                    "routing_disabled": routing_disabled,
                    "util_5h": util_5h,
                    "util_7d": util_7d,
                    "resets_at": resets_at,
                    "quota": snapshots.get(&m.id).map(|state|state.quota.view(now)),
                    "free_requests": free_requests,
                    "routing_status": routing_status,
                    "source": source,
                    "model_discovery_supported": custom.contains(&m.id)
                        && m.provider.protocol == manifest::schema::WireProtocol::Chat,
                });
                if crate::claude_cli::owns(m) {
                    row["login_source"] = serde_json::json!("official_claude_cli");
                    row["transport"] = serde_json::json!("claude_cli");
                    row["subscription_type"] = serde_json::Value::Null;
                }
                if self.chatgpt.owns(m) {
                    row["login_source"] = serde_json::json!(self.chatgpt.login_source());
                }
                row
            })
            .collect()
    }

    /// 添加自定义源(变更记录 32-①):CustomProvider → manifest
    /// (registry::load 同一校验,免费拿可读错误清单)→ config.json
    /// 字段级合并持久化 → 并入候选。id 冲突(preset/插件/已有)拒绝。
    pub fn add_custom_provider(
        &self,
        c: &credentials::settings::CustomProvider,
    ) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        let m = custom_manifest(c)?;
        let _guard = self.settings_io.lock().expect("settings lock");
        if self.manifest(&c.id).is_some() {
            return Err(format!("源 {} 已存在", c.id));
        }
        let mut s = settings_for_edit(config)?;
        // Remove a retired connection's state before the new configuration can
        // be observed on restart. Its first runtime snapshot is asynchronous.
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .map_err(|_| "本机账本不可用")?
                .delete_provider_state(&c.id)
                .map_err(|error| format!("清理旧路由状态失败 · {error}"))?;
        }
        s.providers.retain(|p| p.id != c.id);
        s.providers.push(c.clone());
        s.save(config).map_err(|e| format!("持久化失败: {e}"))?;
        self.merge_custom_provider(m);
        Ok(())
    }

    pub(crate) fn edit_custom_provider(
        &self,
        id: &str,
        models: &[String],
        kind: Option<ProviderKind>,
        base: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".into());
        };
        let mut seen = BTreeSet::new();
        let models: Vec<String> = models
            .iter()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty() && seen.insert(*m))
            .map(str::to_owned)
            .collect();
        if models.is_empty() {
            return Err("模型清单不能为空".into());
        }
        let base = base
            .map(|base| parse_provider_base(base.trim()).ok_or(INVALID_PROVIDER_BASE))
            .transpose()?;

        let _agents = self.agents_io.lock().expect("agent config lock");
        let _guard = self.settings_io.lock().expect("settings lock");
        if !self
            .custom_ids
            .read()
            .expect("custom_ids lock")
            .contains(id)
        {
            return Err(format!("源 {id} 不是自定义源"));
        }
        let mut settings = settings_for_edit(config)?;
        settings.model_selections.remove(id);
        let stored = settings
            .providers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("未找到该源的本机配置")?;
        self.check_custom_provider_supply(id, Some((&models, kind)))?;
        stored.models.clone_from(&models);
        if let Some(kind) = kind {
            stored.kind = match kind {
                ProviderKind::FreeTier => "free_tier",
                ProviderKind::Subscription => "subscription",
            }
            .into();
        }
        {
            let mut manifests = self.manifests.write().expect("manifests lock");
            let mut versions = self
                .connection_versions
                .write()
                .expect("connection versions lock");
            let mut providers = self.providers.write().expect("providers lock");
            let manifest = manifests.get_mut(id).ok_or("源已移除")?;
            let state = providers.get_mut(id).ok_or("源已移除")?;
            let base = if let Some(base) = base {
                let current =
                    parse_provider_base(&manifest.provider.base).ok_or("当前上游地址无效")?;
                if base.origin() != current.origin() {
                    return Err("上游地址必须同源（协议、主机和端口相同）".into());
                }
                let base = base.as_str().trim_end_matches('/').to_owned();
                stored.base.clone_from(&base);
                Some(base)
            } else {
                None
            };
            settings
                .save(config)
                .map_err(|e| format!("持久化失败: {e}"))?;
            let connection_changed = kind.is_some_and(|kind| kind != manifest.kind)
                || base
                    .as_ref()
                    .is_some_and(|base| *base != manifest.provider.base);
            if let Some(kind) = kind {
                manifest.kind = kind;
                state.kind = kind;
            }
            if let Some(base) = base {
                manifest.provider.base = base;
            }
            if connection_changed {
                versions.insert(id.to_string(), Arc::new(()));
            }
            manifest.free_models.clone_from(&models);
            state.models.clone_from(&models);
            state
                .model_cooldowns
                .retain(|model, _| models.contains(model));
        }
        self.mark_dirty(id);
        Ok(models)
    }

    /// 删除自定义源:仅 custom 可删(preset 写死、插件走插件页)。
    /// 先确认钥匙串清理，随后提交源配置变更。
    pub fn remove_custom_provider(&self, id: &str) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        let _agents = self.agents_io.lock().expect("agent config lock");
        let _guard = self.settings_io.lock().expect("settings lock");
        if !self
            .custom_ids
            .read()
            .expect("custom_ids lock")
            .contains(id)
        {
            return Err(format!("源 {id} 不是自定义源,不能删除"));
        }
        let mut s = settings_for_edit(config)?;
        self.check_custom_provider_supply(id, None)?;
        s.providers.retain(|p| p.id != id);
        s.restored_keychain_only.remove(id);
        s.paused_providers.remove(id);
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        self.delete_stored_key(id)
            .map_err(|_| "清理系统钥匙串失败，请重试删除".to_string())?;
        if let Err(error) = s.save(config) {
            // The old connection's key is gone even though the source still exists.
            versions.insert(id.to_string(), Arc::new(()));
            return Err(format!("钥匙串已清理，源配置保存失败 · {error}"));
        }
        self.custom_ids.write().expect("custom_ids lock").remove(id);
        self.providers.write().expect("providers lock").remove(id);
        manifests.remove(id);
        self.restored_keychain_only
            .write()
            .expect("restored credentials lock")
            .remove(id);
        versions.remove(id);
        self.paused_providers
            .write()
            .expect("paused providers lock")
            .remove(id);
        Ok(())
    }

    // Call under agents_io → settings_io so validation and commit stay serialized.
    fn check_custom_provider_supply(
        &self,
        id: &str,
        replacement: Option<(&[String], Option<ProviderKind>)>,
    ) -> Result<(), String> {
        let mut affected = Vec::new();
        for (client, model) in self.managed_agent_models() {
            if model == crate::AUTO_MODEL {
                continue;
            }
            let group = self.group_of(&model).or_else(|| {
                router::parse_members(&model).map(|members| (members, router::GroupRule::FreeFirst))
            });
            let supplies = |models: &[String], kind: ProviderKind| {
                if let Some((members, rule)) = &group {
                    return supplies_group(models, kind, members, *rule);
                }
                if let Some(literal) = model.strip_prefix("free/") {
                    return kind == ProviderKind::FreeTier && models.iter().any(|m| m == literal);
                }
                models.contains(&model)
            };
            let manifests = self.manifests.read().expect("manifests lock");
            let Some(current) = manifests.get(id) else {
                continue;
            };
            if supplies(&current.free_models, current.kind)
                && !replacement
                    .is_some_and(|(models, kind)| supplies(models, kind.unwrap_or(current.kind)))
                && !manifests
                    .iter()
                    .any(|(other, m)| other != id && supplies(&m.free_models, m.kind))
            {
                affected.push(format!("{client} 的模型 {model}"));
            }
        }
        if affected.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "源 {id} 是{}的最后供应，请先切换模型、断开接线或保留供应",
                affected.join("、")
            ))
        }
    }

    pub(crate) async fn refresh_claude_cli(&self) {
        self.claude_cli
            .refresh(|| !self.provider_paused(crate::claude_cli::ID))
            .await;
    }

    pub(crate) fn provider_paused(&self, id: &str) -> bool {
        (id == crate::claude_cli::ID && !self.claude_cli_enabled.load(Ordering::SeqCst))
            || self
                .paused_providers
                .read()
                .expect("paused providers lock")
                .contains(id)
    }

    pub(crate) fn set_provider_paused(&self, id: &str, paused: bool) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".into());
        };
        let _guard = self.settings_io.lock().expect("settings lock");
        if self.manifest(id).is_none() {
            return Err(format!("未知源: {id}"));
        }
        let mut settings = settings_for_edit(config)?;
        if id == crate::claude_cli::ID {
            settings.claude_cli_enabled = !paused;
        }
        if paused {
            settings.paused_providers.insert(id.into());
        } else {
            settings.paused_providers.remove(id);
        }
        settings
            .save(config)
            .map_err(|e| format!("持久化失败: {e}"))?;
        let was_enabled = self
            .claude_cli_enabled
            .swap(settings.claude_cli_enabled, Ordering::SeqCst);
        *self
            .paused_providers
            .write()
            .expect("paused providers lock") = settings.paused_providers;
        if (was_enabled && !settings.claude_cli_enabled) || (id == crate::claude_cli::ID && paused)
        {
            self.claude_cli.cancel_all();
        }
        Ok(())
    }

    pub(crate) fn resume_provider_routing(&self, id: &str) -> Result<bool, String> {
        // Match flush_router_state's lock order so an older snapshot cannot overwrite the recovery.
        let ledger = self
            .ledger
            .as_ref()
            .map(|ledger| ledger.lock().map_err(|_| "路由状态存储不可用".to_string()))
            .transpose()?;
        let mut providers = self.providers.write().expect("providers lock");
        let Some(current) = providers.get_mut(id) else {
            return Ok(false);
        };
        if !current.enabled {
            let mut recovered = current.clone();
            recovered.enabled = true;
            if let Some(ledger) = ledger {
                ledger
                    .save_provider_state(id, &recovered)
                    .map_err(|e| format!("恢复路由保存失败 · {e}"))?;
            }
            *current = recovered;
        }
        Ok(true)
    }

    /// 分组解析(变更记录 32-③):请求 model 命中分组名 → 成员 +
    /// 规则。内联 `group/a+b` 语法优先(前缀即语法,不查表)。
    pub fn group_of(&self, model: &str) -> Option<(Vec<String>, router::GroupRule)> {
        if model.starts_with("group/") || model == crate::AUTO_MODEL {
            return None;
        }
        let groups = self.model_groups.read().expect("model_groups lock");
        groups
            .iter()
            .find(|g| g.name == model)
            .and_then(|g| router::GroupRule::parse(&g.rule).map(|r| (g.members.clone(), r)))
    }

    /// 分组轮流计数自增(RoundRobin 用):返回本轮 rotation。
    pub fn next_group_rotation(&self, name: &str) -> u64 {
        let mut rot = self.group_rotation.write().expect("group_rotation lock");
        let n = rot.entry(name.to_string()).or_insert(0);
        let cur = *n;
        *n = n.wrapping_add(1);
        cur
    }

    pub(crate) fn group_rotation(&self, name: &str) -> u64 {
        self.group_rotation
            .read()
            .expect("group_rotation lock")
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// 模型页数据(变更记录 32-③):分组表(自动虚拟模型由 UI 内置
    /// 渲染,不在表里)。未接线 → None(路由回未配置)。
    pub fn model_groups_overview(&self) -> Option<Vec<credentials::settings::ModelGroup>> {
        self.settings.as_ref()?;
        Some(self.model_groups.read().expect("model_groups lock").clone())
    }

    /// 添加/更新分组(upsert by name,变更记录 32-③):校验名字
    /// (非空/非 group 前缀/非自动别名)、成员(已知字面模型)、规则
    /// (枚举);load-改-save 持久化 → 运行态替换。
    pub fn upsert_model_group(&self, g: &credentials::settings::ModelGroup) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        if g.name.trim().is_empty() {
            return Err("分组名不能为空".to_string());
        }
        if g.name.starts_with("group/") {
            return Err("分组名不能以 group/ 开头(高级语法保留字)".to_string());
        }
        if g.name == crate::AUTO_MODEL {
            return Err("分组名不能是 aubridge(自动虚拟模型保留字)".to_string());
        }
        if g.members.is_empty() {
            return Err("分组成员不能为空".to_string());
        }
        let _agents = self.agents_io.lock().expect("agent config lock");
        let _guard = self.settings_io.lock().expect("settings lock");
        let known = self.literal_models();
        let previous = self
            .model_groups
            .read()
            .expect("model_groups lock")
            .iter()
            .find(|group| group.name == g.name)
            .cloned();
        if let Some(unknown) = g.members.iter().find(|m| {
            !known.contains(m)
                && !previous
                    .as_ref()
                    .is_some_and(|group| group.members.contains(m))
        }) {
            return Err(format!("未知成员模型: {unknown}"));
        }
        let Some(rule) = router::GroupRule::parse(&g.rule) else {
            return Err(format!(
                "未知规则: {}(free_first/free_only/sequential/round_robin/smart/pace)",
                g.rule
            ));
        };
        if let Some(previous) = previous {
            let removes_supply = {
                let manifests = self.manifests.read().expect("manifests lock");
                router::GroupRule::parse(&previous.rule).is_some_and(|previous_rule| {
                    group_has_supply(&manifests, &previous.members, previous_rule)
                        && !group_has_supply(&manifests, &g.members, rule)
                })
            };
            if removes_supply {
                let clients = self
                    .managed_agent_models()
                    .filter_map(|(client, model)| (model == g.name).then_some(client))
                    .collect::<Vec<_>>();
                if !clients.is_empty() {
                    return Err(format!(
                        "分组 {} 正被 {} 使用，请先切换模型、断开接线或保留供应",
                        g.name,
                        clients.join("、")
                    ));
                }
            }
        }
        let mut groups = self.model_groups.write().expect("model_groups lock");
        let mut s = settings_for_edit(config)?;
        s.model_groups.retain(|x| x.name != g.name);
        s.model_groups.push(g.clone());
        s.save(config).map_err(|e| format!("持久化失败: {e}"))?;
        groups.retain(|x| x.name != g.name);
        groups.push(g.clone());
        Ok(())
    }

    /// 删除分组前保护已接线客户端的当前模型。
    pub fn remove_model_group(&self, name: &str) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        // 与切换模型、接线串行，避免校验后留下已删除分组的引用。
        let _agents = self.agents_io.lock().expect("agent config lock");
        let _guard = self.settings_io.lock().expect("settings lock");
        let mut groups = self.model_groups.write().expect("model_groups lock");
        let mut s = settings_for_edit(config)?;
        let before = s.model_groups.len();
        s.model_groups.retain(|x| x.name != name);
        if s.model_groups.len() == before {
            return Err(format!("分组 {name} 不存在"));
        }
        let clients = self
            .managed_agent_models()
            .filter_map(|(client, model)| (model == name).then_some(client))
            .collect::<Vec<_>>();
        if !clients.is_empty() {
            return Err(format!(
                "分组 {name} 正被 {} 使用，请先切换模型或断开接线",
                clients.join("、")
            ));
        }
        s.save(config).map_err(|e| format!("持久化失败: {e}"))?;
        groups.retain(|x| x.name != name);
        self.group_rotation
            .write()
            .expect("group_rotation lock")
            .remove(name);
        Ok(())
    }

    /// 自定义源并入运行态(与 merge_plugins 同构:状态行 + 标脏)。
    fn merge_custom_provider(&self, m: Manifest) {
        let id = m.id.clone();
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        self.providers
            .write()
            .expect("providers lock")
            .insert(id.clone(), provider_state_of(&m));
        self.mark_dirty(&id);
        manifests.insert(id.clone(), m);
        versions.insert(id.clone(), Arc::new(()));
        self.custom_ids.write().expect("custom_ids lock").insert(id);
    }

    /// "与 CLI 共享登录"开关状态(§7;未接线 = 默认只读)。
    pub fn share_login(&self) -> bool {
        self.chatgpt.share_login()
    }

    /// 设置存储是否已接线(admin 路由用;未接线的进程开关不可用)。
    pub fn settings_wired(&self) -> bool {
        self.settings.is_some()
    }

    pub fn chatgpt_login_metadata(&self) -> LoginMetadata {
        self.chatgpt.metadata()
    }

    /// Native sealed history uses the same login-source-aware owner as formal dispatch.
    pub(crate) fn current_compaction_subject(&self, manifest: &Manifest) -> Option<String> {
        if self.chatgpt.owns(manifest) {
            self.chatgpt
                .quota_snapshot()
                .map(|login| login.compaction_subject())
        } else {
            self.current_quota_subject(manifest)
        }
    }

    pub fn set_chatgpt_login_source(&self, source: LoginSource) -> Result<(), String> {
        let Some((config, _)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        let _guard = self.settings_io.lock().expect("settings lock");
        let mut settings = settings_for_edit(config)?;
        settings.chatgpt_login_source = source.as_str().to_string();
        settings
            .save(config)
            .map_err(|_| "登录来源保存失败".to_string())?;
        self.chatgpt.set_login_source(Some(source));
        Ok(())
    }

    /// 切换"与 CLI 共享登录"(§7):开启时先完整备份 auth.json 进
    /// stash(mtime + hash),备份失败不开——不留"无备份却改用户
    /// 文件"的窗口;再持久化 config.json,最后才放行通道写回。
    pub fn set_share_login(&self, on: bool) -> Result<(), String> {
        let Some((config, stash_dir)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        let _guard = self.settings_io.lock().expect("settings lock");
        let mut s = settings_for_edit(config)?;
        if on {
            if self.chatgpt.share_login() {
                return Ok(());
            }
            credentials::stash::backup(self.chatgpt.auth_json_path(), stash_dir)
                .map_err(|e| format!("备份 auth.json 失败(需先 codex login): {e}"))?;
        }
        // load-改-save:凭空构造会把 providers 等同结构体字段抹成默认
        //(save 是字段级,不是文档级,但同结构体的字段一个不落)。
        s.share_login = on;
        s.save(config).map_err(|e| format!("设置持久化失败: {e}"))?;
        self.chatgpt.set_share_login(on);
        Ok(())
    }

    /// 一键还原(§7):stash 备份原子写回 auth.json,内存缓存失效
    /// 下次重读盘。不动开关状态——退出共享另走开关。
    pub fn restore_stash(&self) -> Result<(), String> {
        let Some((_, stash_dir)) = &self.settings else {
            return Err("设置存储未接线".to_string());
        };
        let _guard = self.settings_io.lock().expect("settings lock");
        self.chatgpt
            .restore_stash(stash_dir)
            .map_err(|e| format!("还原失败: {e}"))
    }

    /// stash 备份元信息(UI 展示"备份了什么";未接线/无备份 = None)。
    pub fn stash_meta(&self) -> Option<credentials::stash::StashMeta> {
        let (_, stash_dir) = self.settings.as_ref()?;
        credentials::stash::meta(stash_dir)
    }

    /// 发送前取 chatgpt 凭据(含主动刷新判定,§7)。
    pub async fn chatgpt_ready(&self) -> Option<credentials::oauth::CodexTokens> {
        let channel = self.chatgpt.clone();
        tokio::spawn(async move { channel.ready().await })
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn chatgpt_snapshot(&self) -> Option<LoginSnapshot> {
        let channel = self.chatgpt.clone();
        tokio::spawn(async move { channel.ready_snapshot().await })
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn chatgpt_refresh_snapshot(
        &self,
        snapshot: LoginSnapshot,
    ) -> Option<LoginSnapshot> {
        let channel = self.chatgpt.clone();
        tokio::spawn(async move { channel.refresh_snapshot(snapshot).await })
            .await
            .ok()
            .flatten()
    }

    /// 强制刷新 chatgpt 凭据(401 兜底,§7)。
    pub async fn chatgpt_refresh(
        &self,
        rejected_access: String,
    ) -> Option<credentials::oauth::CodexTokens> {
        let channel = self.chatgpt.clone();
        // 一旦刷新已发出，即使客户端取消也要收齐轮换后的 token。
        tokio::spawn(async move { channel.refresh(&rejected_access).await })
            .await
            .ok()
            .flatten()
    }

    pub(crate) fn is_chatgpt(&self, manifest: &Manifest) -> bool {
        self.chatgpt.owns(manifest)
    }

    pub(crate) fn has_chatgpt_login(&self) -> bool {
        self.chatgpt.has_login()
    }

    /// 插件并入运行态(不写 enabled.json;持久化由开关调用方负责)。
    /// 标脏以覆盖库里的旧状态行——否则重启载入会把并回的源恢复成
    /// 下架前的 stale rest/sink。
    fn merge_plugins(&self, plugins: Vec<Manifest>) -> usize {
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        let mut providers = self.providers.write().expect("providers lock");
        let mut added = 0;
        for m in plugins {
            let id = m.id.clone();
            if manifests.contains_key(&id) {
                continue;
            }
            if let Some(key) = self
                .key_store
                .as_ref()
                .filter(|_| m.permissions.store_key)
                .and_then(|store| store.get(&id))
            {
                self.keys
                    .write()
                    .expect("keys lock")
                    .insert(id.clone(), key);
            }
            providers.insert(id.clone(), provider_state_of(&m));
            self.mark_dirty(&id);
            self.plugin_ids
                .write()
                .expect("plugin_ids lock")
                .insert(id.clone());
            versions.insert(id.clone(), Arc::new(()));
            manifests.insert(id, m);
            added += 1;
        }
        added
    }

    /// 把目录里未显式关闭的合法插件并入运行态(不写 enabled.json)。
    /// auto 开启时由 admin 路由调用;返回并入数量。
    pub fn merge_scanned_plugins(&self) -> Result<usize, Vec<String>> {
        let dir = self
            .plugins_dir
            .clone()
            .ok_or_else(|| vec!["插件目录未配置".to_string()])?;
        let scan = scan_dir(&dir);
        let enabled = EnabledState::new(dir.join("enabled.json"));
        let to_merge: Vec<Manifest> = scan
            .plugins
            .into_iter()
            .filter(|m| !enabled.is_disabled(&m.id))
            .collect();
        Ok(self.merge_plugins(to_merge))
    }

    /// 运行时开关插件:on = 校验后并入候选;off = 移出。
    /// 校验失败返回错误清单(与扫描口径一致)。
    pub fn set_plugin_enabled(&self, id: &str, on: bool) -> Result<(), Vec<String>> {
        let dir = self
            .plugins_dir
            .clone()
            .ok_or_else(|| vec!["插件目录未配置".to_string()])?;
        let is_plugin = self
            .plugin_ids
            .read()
            .expect("plugin_ids lock")
            .contains(id);
        if self.manifest(id).is_some() && !is_plugin {
            return Err(vec![format!("插件 {id} 与已有源重名")]);
        }
        if on {
            let scan = scan_dir(&dir);
            let m = scan
                .plugins
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| vec![format!("插件 {id} 不存在或未通过校验")])?;
            EnabledState::new(dir.join("enabled.json"))
                .set(id, true)
                .map_err(|e| vec![format!("开关持久化失败: {e}")])?;
            self.merge_plugins(vec![m]);
        } else {
            EnabledState::new(dir.join("enabled.json"))
                .set(id, false)
                .map_err(|e| vec![format!("开关持久化失败: {e}")])?;
            let mut manifests = self.manifests.write().expect("manifests lock");
            let mut versions = self
                .connection_versions
                .write()
                .expect("connection versions lock");
            self.providers.write().expect("providers lock").remove(id);
            manifests.remove(id);
            versions.remove(id);
            self.plugin_ids.write().expect("plugin_ids lock").remove(id);
            self.keys.write().expect("keys lock").remove(id);
        }
        Ok(())
    }

    fn assemble(
        manifests: BTreeMap<String, Manifest>,
        token: &str,
        keys: BTreeMap<String, String>,
        plugins_dir: Option<PathBuf>,
    ) -> Self {
        let providers = manifests
            .values()
            .map(|m| (m.id.clone(), provider_state_of(m)))
            .collect::<BTreeMap<_, _>>();
        let connection_versions = providers
            .keys()
            .map(|id| (id.clone(), Arc::new(())))
            .collect();
        // codex 凭据默认路径(§7);HOME 缺失的极端环境留空路径,
        // 读取失败 → 凭据不可用 → 该源换源,不影响其他 preset。
        let codex_dir = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".codex"))
            .unwrap_or_else(|_| PathBuf::from(".codex"));
        let chatgpt = Arc::new(ChatGptChannel::new(
            codex_dir.join("auth.json"),
            codex_dir.join("config.toml"),
        ));
        // 默认 client:连接池保活(§11 预算:转发路径不付连接重建
        // 成本);构造失败兜底裸 client(仅丢失池化,不影响正确性)。
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(POOL_IDLE_PER_HOST)
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build gateway HTTP client");
        let admission = request_body::Admission::default();
        let claude_cli =
            crate::claude_cli::Channel::default().with_update_gate(admission.activity_gate());
        Self {
            agent_validation: Default::default(),
            usage_policy: Default::default(),
            source_health: Default::default(),
            backup: Default::default(),
            native_sessions: Default::default(),
            agent_projects: Default::default(),
            compaction: Mutex::new(crate::compaction::State::default()),
            token: token.to_string(),
            admin_token: format!("{token}-admin"),
            desktop_access: false,
            ledger: None,
            keys: RwLock::new(keys),
            restored_keychain_only: Default::default(),
            opencode_api_auth: None,
            opencode_api_observed: Mutex::new(None),
            key_store: None,
            manifests: RwLock::new(manifests),
            connection_versions: RwLock::new(connection_versions),
            providers: RwLock::new(providers),
            plugins_dir,
            slots: RwLock::new(BTreeMap::new()),
            sticks: RwLock::new(BTreeMap::new()),
            dirty: RwLock::new(BTreeSet::new()),
            pending_requests: Mutex::new(Vec::new()),
            analytics_retention_since: AtomicU64::new(crate::upstream::unix_now()),
            wham: None,
            plan_fetchers: RwLock::new(BTreeMap::new()),
            openrouter: None,
            openrouter_catalog: None,
            directories: directory::Directories::default(),
            admission,
            chatgpt,
            claude_cli,
            settings: None,
            cc_switch_db: None,
            settings_io: Mutex::new(()),
            paused_providers: RwLock::new(BTreeSet::new()),
            claude_cli_enabled: AtomicBool::new(false),
            agents: None,
            agents_io: Mutex::new(()),
            custom_ids: RwLock::new(BTreeSet::new()),
            model_groups: RwLock::new(Vec::new()),
            group_rotation: RwLock::new(BTreeMap::new()),
            plugin_ids: RwLock::new(BTreeSet::new()),
            port: RwLock::new(DEFAULT_PORT),
            serving: tokio::sync::watch::channel(true).0,
            http,
        }
    }

    /// 合法 key:`aubridge` 或 `aubridge-<agent>`(§6)。
    pub fn token_key(&self) -> String {
        self.token.clone()
    }

    pub fn is_valid_key(&self, key: Option<&str>) -> bool {
        !self.is_valid_admin_key(key)
            && key.is_some_and(|k| {
                k == self.token
                    || k.strip_prefix(&format!("{}-", self.token))
                        .is_some_and(|suffix| !suffix.is_empty() && suffix != "admin")
            })
    }

    /// 挂账本(仪表盘数据源 + router 运行态持久化宿主):挂载即
    /// 载入上次落盘的 rest/sink/亲和(§8.1/§8.4,重启不丢)。
    pub fn with_ledger(mut self, l: ledger::Ledger) -> Self {
        if let Some((config, _)) = &self.settings {
            if let Ok(settings) = credentials::settings::Settings::load_checked(config) {
                self.load_model_selections(&settings.model_selections);
            }
        }
        self.load_directories(&l);
        if let Some(catalog) = &self.openrouter_catalog {
            let cached = l.load_provider_catalog("openrouter");
            match cached {
                Ok(Some(json)) => match catalog::Snapshot::from_cache(&json) {
                    Some(snapshot) => {
                        self.apply_openrouter_catalog(&snapshot);
                        catalog.complete(Ok(snapshot));
                    }
                    None => catalog.complete(Err(catalog::Issue::Storage)),
                },
                Ok(None) => {}
                Err(_) => catalog.complete(Err(catalog::Issue::Storage)),
            }
        }
        self.load_router_state(&l);
        self.ledger = Some(std::sync::Mutex::new(l));
        self
    }

    /// 载入持久化的 router 运行态。只恢复路由决策态(rest/sink/熔断/
    /// 利用率/亲和);kind/models 以当前 manifest 为权威(重启间隙
    /// 改配置不回退)。manifest 里已删除的源 ledger 侧过滤,不复活。
    fn load_router_state(&self, l: &ledger::Ledger) {
        let manifests = self.manifests.read().expect("manifests lock").clone();
        let known: BTreeSet<String> = manifests.keys().cloned().collect();
        let (providers, sticks) = l.load_router_state(&known);
        if !providers.is_empty() {
            let mut cur = self.providers.write().expect("providers lock");
            for (id, mut p) in providers {
                if let Some(m) = manifests.get(&id) {
                    p.kind = m.kind;
                    p.models = m.free_models.clone();
                    p.model_cooldowns
                        .retain(|model, _| p.models.contains(model));
                }
                cur.insert(id, p);
            }
        }
        if !sticks.is_empty() {
            self.sticks.write().expect("sticks lock").extend(sticks);
        }
    }

    /// write-behind 落盘(§8.1/§8.4):把脏集里的源状态与亲和表合并
    /// 写进 ledger;决策队列(变更记录 31 批 2 ⑤)一并批量插入。
    /// 尽力语义——未挂 ledger 或写失败都不影响转发路径;
    /// 周期任务(serve 内 5s)与显式调用(测试/停机前)双通道。
    pub fn flush_router_state(&self) {
        let Some(l) = self.ledger.as_ref() else {
            return;
        };
        // 串行 flush，避免旧快照晚于新快照写盘。
        let Ok(ledger) = l.lock() else {
            return;
        };
        let dirty: BTreeSet<String> = std::mem::take(&mut *self.dirty.write().expect("dirty lock"));
        if !dirty.is_empty() {
            let providers: Vec<(String, ProviderState)> = self
                .providers
                .read()
                .expect("providers lock")
                .values()
                .filter(|p| dirty.contains(&p.id))
                .cloned()
                .map(|p| (p.id.clone(), p))
                .collect();
            let sticks: Vec<(String, router::affinity::Stick)> = if dirty.contains(STICKS_DIRTY) {
                self.sticks
                    .read()
                    .expect("sticks lock")
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            } else {
                Vec::new()
            };
            if let Err(error) = ledger.save_router_state(&providers, &sticks) {
                self.dirty.write().expect("dirty lock").extend(dirty);
                eprintln!("aubridge: 路由状态暂未写入 SQLite: {error}");
            }
        }
        let mut pending = self.pending_requests.lock().expect("pending requests lock");
        if !pending.is_empty() {
            match ledger.record_requests(&pending) {
                Ok(()) => pending.clear(),
                Err(e) => eprintln!("aubridge: 请求结算暂未写入 SQLite: {e}"),
            }
        }
    }

    pub(crate) fn record_request(&self, request: ledger::CompletedRequest) {
        if self.ledger.is_none() {
            return;
        }
        let mut pending = self.pending_requests.lock().expect("pending requests lock");
        if pending.len() >= PENDING_REQUESTS_CAP {
            self.analytics_retention_since
                .fetch_max(crate::upstream::unix_now(), Ordering::SeqCst);
            pending.remove(0);
        }
        pending.push(request);
    }

    pub(crate) fn clear_request_history(&self) -> Result<usize, String> {
        let Some(ledger) = &self.ledger else {
            return Ok(0);
        };
        // Keep the lock order shared with flush_router_state so the cleared
        // queue cannot be written back as old history by a concurrent flush.
        let ledger = ledger.lock().expect("ledger lock");
        let mut pending = self.pending_requests.lock().expect("pending requests lock");
        let deleted = ledger
            .clear_request_history(&pending)
            .map_err(|error| format!("清空请求历史失败 · {error}"))?;
        pending.clear();
        self.analytics_retention_since
            .fetch_max(crate::upstream::unix_now(), Ordering::SeqCst);
        Ok(deleted)
    }

    /// D59: no flush, provider sampling, or mutation while reading analytics.
    pub(crate) fn analytics_report(
        &self,
        start: &str,
        end: &str,
        compare_previous: bool,
    ) -> Result<ledger::AnalyticsReport, ledger::AnalyticsError> {
        let ledger = self
            .ledger
            .as_ref()
            .ok_or(ledger::AnalyticsError::Storage)?
            .lock()
            .map_err(|_| ledger::AnalyticsError::Storage)?;
        // Freeze the as-of context briefly; aggregation must not block request settlement.
        let (now, retention_since, timestamps, active_inference) = {
            let pending = self
                .pending_requests
                .lock()
                .map_err(|_| ledger::AnalyticsError::Storage)?;
            let timestamps: Vec<u64> = pending
                .iter()
                .flat_map(|request| {
                    std::iter::once(request.decision.ts)
                        .chain(request.usage.iter().map(|usage| usage.ts))
                })
                .collect();
            (
                crate::upstream::unix_now(),
                self.analytics_retention_since.load(Ordering::SeqCst),
                timestamps,
                self.admission.has_active_inference(),
            )
        };
        ledger.analytics(
            start,
            end,
            compare_previous,
            ledger::AnalyticsContext {
                now,
                retention_since,
                pending_timestamps: &timestamps,
                active_inference,
            },
        )
    }

    pub(crate) fn analytics_decision_page(
        &self,
        start: &str,
        end: &str,
        filter: ledger::DecisionFilter<'_>,
    ) -> Result<ledger::DecisionPage, ledger::AnalyticsError> {
        self.ledger
            .as_ref()
            .ok_or(ledger::AnalyticsError::Storage)?
            .lock()
            .map_err(|_| ledger::AnalyticsError::Storage)?
            .decision_page_in_range(filter, 20, start, end, crate::upstream::unix_now())
    }

    /// 仪表盘最近 20 条请求；账本未配置或读取失败时不可用。
    pub fn recent_decisions(&self) -> Option<Vec<ledger::DecisionRecord>> {
        self.decision_page(ledger::DecisionFilter::default())
            .map(|page| page.rows)
    }

    /// 先写待落盘的请求，再查询本地账本。
    pub(crate) fn decision_page(
        &self,
        filter: ledger::DecisionFilter<'_>,
    ) -> Option<ledger::DecisionPage> {
        let ledger = self.ledger.as_ref()?;
        self.flush_router_state();
        ledger
            .lock()
            .expect("ledger lock")
            .decision_page(filter, 20)
            .ok()
    }

    /// 标脏(内部:所有 router 状态变更方法收尾调用)。
    fn mark_dirty(&self, id: &str) {
        if self.ledger.is_some() {
            self.dirty
                .write()
                .expect("dirty lock")
                .insert(id.to_string());
        }
    }

    /// admin key(§6:与推理 key 空间隔离)。
    pub fn is_valid_admin_key(&self, key: Option<&str>) -> bool {
        key == Some(self.admin_token.as_str())
    }

    /// 月度省 $X(本自然月);账本未配置或查询失败时不可用。
    pub async fn monthly_savings(&self) -> Option<(f64, Vec<(String, f64)>)> {
        self.flush_router_state();
        self.ledger
            .as_ref()
            .and_then(|l| l.lock().expect("ledger lock").monthly_savings_opt().ok())
    }

    pub(crate) fn session_page(
        &self,
        agent: Option<&str>,
        session: Option<&str>,
        before: Option<(u64, i64)>,
    ) -> Option<ledger::SessionPage> {
        self.flush_router_state();
        self.ledger
            .as_ref()?
            .lock()
            .ok()?
            .session_page(agent, session, before, 20)
            .ok()
    }

    pub(crate) fn monthly_value(&self) -> Option<ledger::ValueSummary> {
        self.flush_router_state();
        self.ledger.as_ref()?.lock().ok()?.monthly_value().ok()
    }

    /// 源 key:钥匙串缓存优先，环境变量作为开发注入路径。
    pub fn api_key_opt(&self, id: &str) -> Option<String> {
        self.resolve_api_key(id).0
    }

    fn resolve_api_key(&self, id: &str) -> (Option<String>, &'static str) {
        let manifests = self.manifests.read().expect("manifests lock");
        self.resolve_api_key_for(id, manifests.get(id))
    }

    // Read-only: callers may already hold connection_versions. Never upgrade it here.
    fn resolve_api_key_for(
        &self,
        id: &str,
        manifest: Option<&Manifest>,
    ) -> (Option<String>, &'static str) {
        if let Some(k) = self.keys.read().expect("keys lock").get(id) {
            return (Some(k.clone()), "saved");
        }
        if self
            .restored_keychain_only
            .read()
            .expect("restored credentials lock")
            .contains(id)
        {
            return (None, "none");
        }
        let env = format!("KEY_{}", id.to_uppercase().replace('-', "_"));
        if let Ok(key) = std::env::var(env) {
            return (Some(key), "environment");
        }
        let imported = manifest
            .filter(|m| self.opencode_api_target(m))
            .and(self.opencode_api_auth.as_deref())
            .and_then(|path| credentials::opencode::read_opencode_openrouter_api_key(path).ok())
            .flatten();
        if imported.is_some() {
            (imported, "opencode")
        } else {
            (None, "none")
        }
    }

    fn opencode_api_target(&self, manifest: &Manifest) -> bool {
        opencode_keys::official_target(manifest)
            && !self.is_custom_provider(&manifest.id)
            && !self
                .plugin_ids
                .read()
                .expect("plugin_ids lock")
                .contains(&manifest.id)
    }

    /// Must run before taking the routing snapshot, so a rotated key can escape
    /// only its previous key cooldown. The first observation preserves persisted state.
    pub(crate) fn reconcile_external_api_key(&self) {
        if self.opencode_api_auth.is_none() {
            return;
        }
        let manifests = self.manifests.read().expect("manifests lock");
        let Some(manifest) = manifests
            .get("openrouter")
            .filter(|m| self.opencode_api_target(m))
        else {
            return;
        };
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        let (key, _) = self.resolve_api_key_for(&manifest.id, Some(manifest));
        self.observe_external_api_key(manifest, &key, &mut versions);
    }

    // Lock order: manifests -> versions; key reads finish before observation,
    // and observation may then update providers. No resolve path writes versions.
    // Resolve once under versions, then return that exact credential/version pair.
    fn observe_external_api_key(
        &self,
        manifest: &Manifest,
        key: &Option<String>,
        versions: &mut BTreeMap<String, ConnectionVersion>,
    ) {
        if self.opencode_api_auth.is_none() || !self.opencode_api_target(manifest) {
            return;
        }
        let mut observed = self
            .opencode_api_observed
            .lock()
            .expect("opencode key observation lock");
        if observed.as_ref().is_some_and(|previous| previous != key) {
            versions.insert(manifest.id.clone(), Arc::new(()));
            self.set_key_retry_at(&manifest.id, None);
            self.mark_dirty(&manifest.id);
        }
        *observed = Some(key.clone());
    }

    fn baseline_external_api_key(&self, manifest: &Manifest) {
        if self.opencode_api_auth.is_some() && self.opencode_api_target(manifest) {
            let key = self.resolve_api_key_for(&manifest.id, Some(manifest)).0;
            *self
                .opencode_api_observed
                .lock()
                .expect("opencode key observation lock") = Some(key);
        }
    }

    pub(crate) fn is_custom_provider(&self, id: &str) -> bool {
        self.custom_ids
            .read()
            .expect("custom_ids lock")
            .contains(id)
    }

    pub(crate) fn dispatch_credential(
        &self,
        planned: &Manifest,
    ) -> Result<(crate::route_plan::Credential, ConnectionVersion), &'static str> {
        let manifests = self.manifests.read().expect("manifests lock");
        let current = manifests
            .get(&planned.id)
            .filter(|m| {
                m.provider == planned.provider
                    && m.kind == planned.kind
                    && m.permissions.store_key == planned.permissions.store_key
            })
            .ok_or(crate::route_plan::PROVIDER_CHANGED)?;
        parse_provider_base(&current.provider.base)
            .filter(|url| {
                url.host_str().is_some_and(|host| {
                    manifest::schema::allows_host(host, &current.permissions.net)
                })
            })
            .ok_or(crate::route_plan::INVALID_PROVIDER_URL)?;
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        let (key, _) = self.resolve_api_key_for(&current.id, Some(current));
        self.observe_external_api_key(current, &key, &mut versions);
        let version = versions
            .get(&planned.id)
            .ok_or(crate::route_plan::PROVIDER_CHANGED)?;
        let credential = crate::route_plan::credential_with_key(self, current, || key)
            .ok_or("missing-credential")?;
        Ok((credential, version.clone()))
    }

    pub(crate) fn update_current_connection(
        &self,
        id: &str,
        version: Option<&ConnectionVersion>,
        update: impl FnOnce(),
    ) {
        let Some(version) = version else {
            return;
        };
        let versions = self
            .connection_versions
            .read()
            .expect("connection versions lock");
        if versions
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, version))
        {
            update();
        }
    }

    pub(crate) fn update_bound_connection(
        &self,
        id: &str,
        version: Option<&ConnectionVersion>,
        login: Option<&LoginSnapshot>,
        cli_epoch: Option<&Arc<()>>,
        update: impl FnOnce(),
    ) {
        self.update_current_connection(id, version, || {
            if id == crate::claude_cli::ID {
                self.claude_cli.update_bound(cli_epoch, update);
            } else if let Some(login) = login {
                self.chatgpt.update_current_quota_login(login, update);
            } else {
                update();
            }
        });
    }

    /// key 是否可用(Provider 行"已设置"口径:缓存或 env 有值即
    /// 已设置——对用户语义是"这个源有凭据",来源不区分)。
    pub fn key_set(&self, id: &str) -> bool {
        self.api_key_opt(id).is_some()
    }

    /// 保存源 key(变更记录 32-①):keyring + 缓存双写,保存即生效
    /// 免重启。仅 store_key 源可写(chatgpt 等走 OAuth 通道)。
    pub fn set_provider_key(&self, id: &str, key: &str) -> Result<(), String> {
        if key.trim().is_empty() {
            return Err("key 不能为空".to_string());
        }
        // keyring IO(OS 钥匙串可能弹授权框,等待时间不可控)必须发生在
        // 所有状态锁之外——2026-11-15 实测:持有 manifests 读锁跨
        // keychain 调用,一次授权等待会连锁阻塞 manifests 写方
        // (provider 增删),网关整面停摆。锁序不变(manifests →
        // versions → keys),仅把 IO 移出临界区。
        let (manifest, store) = {
            let manifests = self.manifests.read().expect("manifests lock");
            let Some(m) = manifests.get(id) else {
                return Err(format!("未知源: {id}"));
            };
            if !m.permissions.store_key {
                return Err(format!("源 {id} 的凭据不走 aubridge key 通道"));
            }
            let Some(store) = &self.key_store else {
                return Err("key 存储未接线".to_string());
            };
            (m.clone(), store.clone())
        };
        store.set(id, key)?;
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        let mut keys = self.keys.write().expect("keys lock");
        keys.insert(id.to_string(), key.to_string());
        drop(keys);
        self.set_key_retry_at(id, None);
        versions.insert(id.to_string(), Arc::new(()));
        self.baseline_external_api_key(&manifest);
        Ok(())
    }

    /// 清除源 key:keyring 删条目(幂等)+ 缓存移除。env 来源的 key
    /// 清不掉(每请求重读 env),重载 Provider 行会如实回到"已设置"。
    pub fn clear_provider_key(&self, id: &str) -> Result<(), String> {
        // 同 set_provider_key:keyring IO 移出所有状态锁之外。
        let manifest = {
            let manifests = self.manifests.read().expect("manifests lock");
            let Some(m) = manifests.get(id) else {
                return Err(format!("未知源: {id}"));
            };
            if !m.permissions.store_key {
                return Err(format!("源 {id} 的凭据不走 aubridge key 通道"));
            }
            m.clone()
        };
        self.delete_stored_key(id)?;
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        versions.insert(id.to_string(), Arc::new(()));
        self.baseline_external_api_key(&manifest);
        Ok(())
    }

    fn delete_stored_key(&self, id: &str) -> Result<(), String> {
        if let Some(store) = &self.key_store {
            store.delete(id)?;
        }
        self.keys.write().expect("keys lock").remove(id);
        self.set_key_retry_at(id, None);
        Ok(())
    }

    pub(crate) fn set_key_retry_at(&self, id: &str, retry_at: Option<u64>) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(source) = providers.get_mut(id) {
            source.key_retry_at = retry_at;
        }
    }

    pub fn manifest(&self, id: &str) -> Option<Manifest> {
        let mut manifest = self
            .manifests
            .read()
            .expect("manifests lock")
            .get(id)
            .cloned()?;
        self.apply_directory(&mut manifest);
        Some(manifest)
    }

    pub fn snapshot(&self) -> Vec<ProviderState> {
        self.routing_sources()
            .into_iter()
            .map(|(source, _)| source)
            .collect()
    }

    pub(crate) fn routing_sources(&self) -> Vec<(ProviderState, Manifest)> {
        let mut sources: Vec<_> = {
            let manifests = self.manifests.read().expect("manifests lock");
            self.providers
                .read()
                .expect("providers lock")
                .values()
                .filter_map(|source| Some((source.clone(), manifests.get(&source.id)?.clone())))
                .collect()
        };
        for (source, manifest) in &mut sources {
            self.bind_quota_identity(source, manifest);
            self.apply_directory(manifest);
        }
        sources
    }

    /// 网关聚合模型清单(OpenCode with_models 与 agents 页下拉共用)。
    /// 自动虚拟模型置首(变更记录 32-②:编辑弹层"自动(aubridge)"
    /// 第一项);分组名并入(变更记录 32-③:agent 可直接请求分组名)。
    pub fn models(&self) -> Vec<String> {
        let mut models = vec![crate::AUTO_MODEL.to_string()];
        models.extend(self.literal_models());
        for g in self.model_groups.read().expect("model_groups lock").iter() {
            if !models.contains(&g.name) {
                models.push(g.name.clone());
            }
        }
        models
    }

    /// manifests 聚合的字面模型(sort + dedup,不含自动别名/分组名)。
    fn literal_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .routing_sources()
            .into_iter()
            .flat_map(|(_, manifest)| {
                manifest
                    .free_models
                    .iter()
                    .filter(|model| self.directory_availability(&manifest, model) != Some(false))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        models.sort();
        models.dedup();
        models
    }

    /// 候选循环(router 纯函数)。
    pub fn candidates(&self, model: &str, now: u64) -> Vec<String> {
        router::candidates(model, &self.snapshot(), now)
    }

    /// 失败转移回写(§8.1;fuzz 由调用方注入)。
    pub fn apply_failure(&self, id: &str, signal: &UpstreamSignal, now: u64, fuzz: u64) {
        let decision = router::classify::classify(signal, now);
        self.apply_provider_failure(id, &decision, now, fuzz);
    }

    /// 成功转移回写。
    pub fn apply_success(&self, id: &str, now: u64) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(state) = providers.get_mut(id) {
            // A request already in flight must not clear a newer account cooldown.
            if !state.is_resting(now) {
                *state = router::rest::apply_success(state, now);
            }
            state
                .model_cooldowns
                .retain(|_, cooldown| cooldown.until > now);
        }
        drop(providers);
        self.mark_dirty(id);
    }

    /// 被动采样决策落地(§8.2:成功响应上采到窗口耗尽,按失败语义
    /// rest 到窗口重置)。
    pub fn apply_decision(&self, id: &str, decision: &router::classify::FailureDecision, now: u64) {
        self.apply_provider_failure(id, decision, now, 0);
    }

    fn apply_provider_failure(
        &self,
        id: &str,
        decision: &router::FailureDecision,
        now: u64,
        fuzz: u64,
    ) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(state) = providers.get_mut(id) {
            if state.rest_until.is_some_and(|until| until <= now) {
                state.rest_until = None;
                state.rest_reason = None;
                state.sunk = false;
            }
            if state.resets_at.is_some_and(|reset| reset <= now) {
                state.resets_at = None;
                state.remaining = None;
            }
            let mut next = router::rest::apply_failure(state, decision, now, fuzz);
            if next.rest_until < state.rest_until {
                next.rest_until = state.rest_until;
                next.rest_reason = state.rest_reason;
            }
            *state = next;
        }
        drop(providers);
        self.mark_dirty(id);
    }

    pub(crate) fn cool_model(&self, id: &str, model: &str, cooldown: ModelCooldown, now: u64) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(source) = providers
            .get_mut(id)
            .filter(|source| source.models.iter().any(|m| m == model))
        {
            source
                .model_cooldowns
                .retain(|_, current| current.until > now);
            if source
                .model_cooldowns
                .get(model)
                .is_none_or(|current| current.until <= cooldown.until)
            {
                source.model_cooldowns.insert(model.into(), cooldown);
            }
        }
        drop(providers);
        self.mark_dirty(id);
    }

    pub(crate) fn route_block(
        &self,
        id: &str,
        model: &str,
        now: u64,
    ) -> Option<crate::route_plan::Block> {
        if self.provider_paused(id) {
            return Some(crate::route_plan::Block::new(
                crate::route_plan::PROVIDER_PAUSED,
                None,
            ));
        }
        let manifest = {
            self.manifests
                .read()
                .expect("manifests lock")
                .get(id)
                .cloned()
        };
        if manifest
            .as_ref()
            .is_some_and(|manifest| self.directory_availability(manifest, model) == Some(false))
        {
            return Some(crate::route_plan::Block::new("model-not-listed", None));
        }
        let providers = self.providers.read().expect("providers lock");
        match providers.get(id) {
            Some(source) if source.models.iter().any(|id| id == model) => {
                crate::route_plan::blocked(source, model, now)
            }
            Some(_) => Some(crate::route_plan::Block::new("model-unlisted", None)),
            None => Some(crate::route_plan::Block::new("provider-disabled", None)),
        }
    }

    /// 成功响应被动采样:记录窗口重置时刻(§8.2,只前移)。
    pub fn apply_reset_sample(&self, id: &str, reset_at: u64) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(state) = providers.get_mut(id) {
            *state = router::rest::apply_sample_reset(state, reset_at);
        }
        drop(providers);
        self.mark_dirty(id);
    }

    /// 熔断失败计数(§8.1:5xx/408/网络失败;调用方已按状态码过滤)。
    pub fn apply_breaker_failure(&self, id: &str, now: u64) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(state) = providers.get_mut(id) {
            state.breaker = router::breaker::record_failure(&state.breaker, now);
        }
        drop(providers);
        self.mark_dirty(id);
    }

    /// 熔断成功恢复计数(半开 2 成关闭,closed 清计数)。
    pub fn apply_breaker_success(&self, id: &str) {
        let mut providers = self.providers.write().expect("providers lock");
        if let Some(state) = providers.get_mut(id) {
            state.breaker = router::breaker::record_success(&state.breaker);
        }
        drop(providers);
        self.mark_dirty(id);
    }

    /// 利用率回写(§8.2 被动采样/主动查询):合并式——None 不覆盖
    /// 已知值(头缺失 = 未知,不抹掉上一窗口的采样)。顺带把读数
    /// 采样进配额窗口历史(尽力语义,quota-plan-design §3.3;reset
    /// 时刻取 ProviderState 当前快照——同周期内恒有值,周期切换由
    /// 历史侧周期判定处理)。
    pub fn record_utilization(&self, id: &str, w5h: Option<f64>, w7d: Option<f64>) {
        if w5h.is_none() && w7d.is_none() {
            return;
        }
        let resets_at = {
            let mut providers = self.providers.write().expect("providers lock");
            let state = providers.get_mut(id);
            match state {
                Some(state) => {
                    if w5h.is_some() {
                        state.util_5h = w5h;
                    }
                    if w7d.is_some() {
                        state.util_7d = w7d;
                    }
                    state.resets_at
                }
                None => return,
            }
        };
        self.note_quota_history(id, w5h, w7d, resets_at);
        self.mark_dirty(id);
    }

    /// 读数入历史(ledger Mutex 独立于状态锁;写失败不影响转发,
    /// 与 router_state write-behind 同口径)。
    fn note_quota_history(
        &self,
        id: &str,
        w5h: Option<f64>,
        w7d: Option<f64>,
        resets_at: Option<u64>,
    ) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let Ok(ledger) = ledger.lock() else {
            return;
        };
        let now = crate::upstream::unix_now();
        for (window, used) in [("5h", w5h), ("7d", w7d)] {
            if let Some(used) = used {
                let _ = ledger.note_quota_point(id, window, 100.0 - used, resets_at, now);
            }
        }
    }

    /// 会话亲和条目查询(§8.4;调用方负责冷窗与重排决策)。
    pub fn stick(&self, key: &str) -> Option<router::affinity::Stick> {
        self.sticks.read().expect("sticks lock").get(key).cloned()
    }

    /// 回复后的粘性回写(§8.4,流末由输出侧护栏调用):cache 量
    /// 不足不粘(fold 内判);替补答话保留 main(backToMain);容量
    /// 上限 512,满时淘汰最旧。write-behind 持久化(脏标记 +
    /// 周期/显式 flush 落 ledger)。
    pub fn record_affinity(&self, key: &str, provider: &str, cache: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut sticks = self.sticks.write().expect("sticks lock");
        let existing = sticks.get(key).cloned();
        let Some(entry) = router::affinity::fold(existing, provider, cache, now) else {
            return;
        };
        sticks.insert(key.to_string(), entry);
        if sticks.len() > router::affinity::STICKS_KEPT {
            if let Some(oldest) = sticks
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            {
                sticks.remove(&oldest);
            }
        }
        drop(sticks);
        if self.ledger.is_some() {
            self.dirty
                .write()
                .expect("dirty lock")
                .insert(STICKS_DIRTY.to_string());
        }
    }

    /// 订阅源并发槽位(§8.5:上限 2;免费源返回 None 不限流)。
    /// 槽位表惰性建,泄漏无虞(源数有限,进程生命周期)。
    pub fn subscription_slot(
        &self,
        id: &str,
        kind: manifest::schema::ProviderKind,
    ) -> Option<std::sync::Arc<tokio::sync::Semaphore>> {
        if kind != manifest::schema::ProviderKind::Subscription {
            return None;
        }
        let mut slots = self.slots.write().expect("slots lock");
        Some(
            slots
                .entry(id.to_string())
                .or_insert_with(|| {
                    std::sync::Arc::new(tokio::sync::Semaphore::new(SUBSCRIPTION_CONCURRENCY))
                })
                .clone(),
        )
    }
}

/// 每订阅账号并发上限(§8.5 定死默认 2,不做配置——sub2api 教训)。
const SUBSCRIPTION_CONCURRENCY: usize = 2;

/// 连接池每主机保活连接数(变更记录 31 批 1;上游源有限,8 足够)。
const POOL_IDLE_PER_HOST: usize = 8;

/// 默认端口(§4;serve() 绑定后按实际值回写)。
const DEFAULT_PORT: u16 = 3427;

/// drift → 字符串(仅 UI 展示;replaced = 接线字段被第三方改写)。
fn settings_for_edit(path: &Path) -> Result<credentials::settings::Settings, String> {
    credentials::settings::Settings::load_checked(path)
        .map_err(|error| format!("读取本机配置失败，请检查 config.json · {error}"))
}

fn drift_str(d: &agents::Drift) -> &'static str {
    match d {
        agents::Drift::Clean => "clean",
        agents::Drift::PortChanged => "port-changed",
        agents::Drift::Replaced => "replaced",
        agents::Drift::Unwired => "unwired",
    }
}

/// 脏集里的亲和表哨兵行(真实 provider id 不会带 @ 前缀)。
const STICKS_DIRTY: &str = "@sticks";

const PENDING_REQUESTS_CAP: usize = 1024;

/// manifest → 运行态快照(kind 取自 manifest,不臆断)。
fn supplies_group(
    models: &[String],
    kind: ProviderKind,
    members: &[String],
    rule: router::GroupRule,
) -> bool {
    rule.allows_provider(kind) && models.iter().any(|model| members.contains(model))
}

fn group_has_supply(
    manifests: &BTreeMap<String, Manifest>,
    members: &[String],
    rule: router::GroupRule,
) -> bool {
    manifests
        .values()
        .any(|manifest| supplies_group(&manifest.free_models, manifest.kind, members, rule))
}

fn provider_state_of(m: &Manifest) -> ProviderState {
    ProviderState::new(&m.id, m.kind, m.free_models.clone())
}

/// CustomProvider → manifest(变更记录 32-①):经 registry::load 同一
/// 装载校验路径(id kebab-case/base 合法 URL/models 非空),坏值
/// 返回可读错误。net 白名单 = base host 本身(自定义源只许访问
/// 自己声明的域);quota 写死 inferred(429/头推断,零配置);
/// store_key 恒 true(自定义源 key 走 keyring)。
fn custom_manifest(c: &credentials::settings::CustomProvider) -> Result<Manifest, String> {
    let base = parse_provider_base(c.base.trim()).ok_or(INVALID_PROVIDER_BASE)?;
    let host = base.host_str().ok_or("上游地址缺少主机")?;
    let json = serde_json::json!({
        "id": c.id,
        "version": "1",
        "kind": c.kind,
        "provider": { "base": base.as_str().trim_end_matches('/'), "auth": c.auth, "protocol": c.protocol },
        "free_models": c.models,
        "quota": { "source": "inferred" },
        "permissions": { "net": [host], "store_key": true },
    });
    let text = serde_json::to_string(&json).map_err(|e| e.to_string())?;
    registry::load(&text).map_err(|e| e.to_string())
}

const INVALID_PROVIDER_BASE: &str = "上游地址必须是合法 HTTP(S) URL，不能含用户信息、查询串或片段";

fn parse_provider_base(raw: &str) -> Option<reqwest::Url> {
    manifest::schema::base_host(raw)?;
    let url = reqwest::Url::parse(raw).ok()?;
    (url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归(2026-11-15 实测发现):keyring IO 不得持有任何状态锁——
    /// OS 钥匙串可能弹授权框,等待时间不可控;旧实现持有 manifests
    /// 读锁跨 `store.set`,一次授权等待连锁阻塞 manifests 写方,
    /// 网关整面停摆。用可阻塞的 store 验证:set 期间 manifests 写锁
    /// 必须可得。
    #[test]
    fn keyring_io_holds_no_state_locks() {
        struct GatedStore {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl credentials::KeyStore for GatedStore {
            fn set(&self, _provider: &str, _key: &str) -> Result<(), String> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(())
            }
            fn get(&self, _provider: &str) -> Option<String> {
                None
            }
            fn delete(&self, _provider: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "id": "gated", "version": "1",
            "provider": { "base": "https://relay.example/v1", "auth": "bearer" },
            "free_models": ["m"],
            "quota": { "source": "inferred" },
            "permissions": { "net": ["relay.example"], "store_key": true }
        }))
        .unwrap();
        let state = GatewayState::from_manifests(
            std::collections::BTreeMap::from([("gated".to_string(), manifest)]),
            "aubridge",
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let state = std::sync::Arc::new(state.with_key_store(std::sync::Arc::new(GatedStore {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        })));

        let writer = std::thread::spawn({
            let state = state.clone();
            move || state.set_provider_key("gated", "fixture-key").unwrap()
        });
        // store.set 已进入:此刻 manifests 写锁必须可得(旧实现持有
        // 读锁跨 IO,轮询 2 秒拿不到即失败;新实现立即可得)。
        entered_rx.recv().unwrap();
        let mut writable = false;
        for _ in 0..200 {
            if state.manifests.try_write().is_ok() {
                writable = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            writable,
            "store.set 期间 manifests 写锁不可得:keyring IO 不得持有状态锁"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(
            state.keys.read().unwrap().get("gated").map(String::as_str),
            Some("fixture-key")
        );
    }
}

#[cfg(test)]
mod analytics_context_tests {
    use super::*;

    fn state() -> GatewayState {
        GatewayState::from_manifests(BTreeMap::new(), "analytics-context")
            .with_ledger(ledger::Ledger::open(":memory:").unwrap())
    }

    fn completed(ts: u64) -> ledger::CompletedRequest {
        ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                ts,
                outcome: "ok".into(),
                attempts: "[]".into(),
                ..Default::default()
            },
            usage: vec![ledger::UsageRecord {
                ts,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn analytics_read_keeps_pending_unsettled_and_does_not_flush() {
        let state = state();
        let window = state
            .ledger
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .analytics_window("2020-01-01", "2020-01-02", crate::upstream::unix_now())
            .unwrap();
        state.analytics_retention_since.store(0, Ordering::SeqCst);
        state.record_request(completed(window.start_ts));
        let report = state
            .analytics_report("2020-01-01", "2020-01-02", true)
            .unwrap();
        let report = serde_json::to_value(report).unwrap();
        assert_eq!(report["summary"]["requests"]["total"], 0);
        assert_eq!(
            report["comparison"]["requests_change"]["reason"],
            "pending_settlement"
        );
        assert_eq!(state.pending_requests.lock().unwrap().len(), 1);
        assert!(state
            .ledger
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .recent_decisions(20)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn loss_and_successful_clear_advance_observation_boundary() {
        let state = state();
        state.analytics_retention_since.store(0, Ordering::SeqCst);
        for _ in 0..=PENDING_REQUESTS_CAP {
            state.record_request(completed(1));
        }
        assert_eq!(
            state.pending_requests.lock().unwrap().len(),
            PENDING_REQUESTS_CAP
        );
        assert!(state.analytics_retention_since.load(Ordering::SeqCst) > 0);
        state.analytics_retention_since.store(0, Ordering::SeqCst);
        state.clear_request_history().unwrap();
        assert!(state.analytics_retention_since.load(Ordering::SeqCst) > 0);
        assert!(state.pending_requests.lock().unwrap().is_empty());
    }
}

//! Production gateway lifecycle, shared by the desktop and headless entry points.

use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::chatgpt::ChatGptChannel;
use crate::state::GatewayState;
use credentials::settings::Settings;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct Runtime {
    state: Arc<GatewayState>,
    listener: tokio::net::TcpListener,
    data_dir: PathBuf,
    _instance: Arc<File>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct GatewayAccess {
    pub endpoint: String,
    pub admin_token: String,
}

impl GatewayAccess {
    pub fn from_state(state: &GatewayState) -> Self {
        Self {
            endpoint: format!("http://127.0.0.1:{}", state.port()),
            admin_token: state.admin_token(),
        }
    }
}

impl Runtime {
    pub async fn start(desktop: bool) -> io::Result<Self> {
        let (home, plugins, data_dir) = resolve_dirs();
        fs::create_dir_all(&data_dir)?;
        let instance = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join("gateway.lock"))?;
        instance
            .try_lock()
            .map_err(|e| io::Error::other(format!("无法锁定 aubridge 数据目录: {e}")))?;

        let agents_dir = data_dir.join("agents");
        for (name, result) in agents::protect_wiring_backups(&agents_dir) {
            if let Err(error) = result {
                eprintln!("aubridge: 收紧接线备份 {name} 的权限失败: {error}");
            }
        }
        let config = data_dir.join("config.json");
        let ledger =
            ledger::Ledger::open(data_dir.join("ledger.sqlite")).map_err(io::Error::other)?;
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map_or_else(|| home.join(".claude"), PathBuf::from);
        let codex_dir =
            std::env::var_os("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
        let opencode_config = std::env::var_os("OPENCODE_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let config_home = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .unwrap_or_else(|| home.join(".config"));
                agents::opencode::config_path(&config_home.join("opencode"))
            });
        let opencode_auth = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"))
            .join("opencode/auth.json");
        let mut state = GatewayState::from_manifests(
            registry::presets()
                .into_iter()
                .map(|m| (m.id.clone(), m))
                .collect(),
            "aubridge",
        )
        .with_admin_token(new_admin_token()?)
        .with_opencode_api_auth(opencode_auth.clone())
        .with_codex_channel(
            ChatGptChannel::new(codex_dir.join("auth.json"), codex_dir.join("config.toml"))
                .with_opencode_auth(opencode_auth),
        )
        .with_settings(config.clone(), data_dir.join("stash"))
        .with_cc_switch_db(home.join(".cc-switch").join("cc-switch.db"))
        .with_plugins(plugins)
        .with_openrouter_catalog_endpoint(crate::catalog::OPENROUTER_MODELS)
        .with_ledger(ledger)
        .with_wham_endpoint(credentials::usage::WhamFetcher::ENDPOINT)
        .with_openrouter_endpoint(credentials::usage::OpenRouterKeyFetcher::ENDPOINT)
        .with_agents(agents::AgentsPaths {
            claude_settings: claude_dir.join("settings.json"),
            codex_config: codex_dir.join("config.toml"),
            opencode_config,
            gemini_settings: home.join(".gemini/settings.json"),
            gemini_env: home.join(".gemini/.env"),
            state_dir: agents_dir,
        })
        .with_key_store(Arc::new(credentials::OsKeyring));
        if desktop {
            state = state.with_desktop_access();
        }
        let state = Arc::new(state);
        let preferred_port = Settings::load_port(&config).unwrap_or_else(|| state.port());
        let listener = crate::bind_listener(&state, preferred_port).await?;
        match Settings::save_port(&config, state.port()) {
            Ok(()) => {
                if let Err(error) = state.repair_claude_port() {
                    eprintln!("aubridge: Claude Code 旧端口修复失败: {error}");
                }
                if let Err(error) = state.repair_opencode() {
                    eprintln!("aubridge: OpenCode 接线同步失败: {error}");
                }
                if let Err(error) = state.repair_codex_port() {
                    eprintln!("aubridge: Codex profile 端口修复失败: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                eprintln!("aubridge: 配置无效，本次使用临时端口状态，暂缓自动接线 · {error}");
            }
            Err(error) => return Err(error),
        }
        Ok(Self {
            state,
            listener,
            data_dir,
            _instance: Arc::new(instance),
        })
    }

    pub fn state(&self) -> Arc<GatewayState> {
        self.state.clone()
    }

    pub async fn serve(self) -> io::Result<()> {
        // Keep the data-directory lock for the listener's entire lifetime.
        let _instance = self._instance;
        crate::serve_listener(self.state, self.listener).await
    }

    /// Stop accepting connections, then give ongoing responses time to finish.
    pub async fn serve_until_shutdown(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let _instance = self._instance;
        let (begin_drain, drain) = tokio::sync::oneshot::channel();
        let server = axum::serve(self.listener, crate::app(self.state.clone()))
            .with_graceful_shutdown(async move {
                let _ = drain.await;
            });
        let result = {
            let serving = crate::serve_with_ledger_flush(&self.state, server);
            tokio::pin!(serving);
            tokio::select! {
                result = &mut serving => result,
                () = shutdown => {
                    let _ = begin_drain.send(());
                    match tokio::time::timeout(SHUTDOWN_GRACE, &mut serving).await {
                        Ok(result) => result,
                        Err(_) => {
                            eprintln!("aubridge: 停机等待达到 5 秒，关闭剩余连接");
                            Ok(())
                        }
                    }
                }
            }
        };
        self.state.flush_router_state();
        result
    }

    pub fn publish_session(&self) -> io::Result<SessionFile> {
        let path = self.data_dir.join("gateway-session.json");
        let temp = self.data_dir.join("gateway-session.json.tmp");
        if let Err(error) = fs::remove_file(&temp) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let access = serde_json::to_vec(&GatewayAccess::from_state(&self.state))?;
        let result = file.write_all(&access);
        drop(file);
        if let Err(error) = result.and_then(|()| fs::rename(&temp, &path)) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(SessionFile {
            path,
            _instance: self._instance.clone(),
        })
    }
}

pub struct SessionFile {
    path: PathBuf,
    _instance: Arc<File>,
}

impl SessionFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn new_admin_token() -> io::Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// 数据目录(B-7 `~/.aubridge`,PLUGINS_DIR 重定向测试环境)。诊断
/// CLI 与网关进程共用同一解析,路径不双写(变更记录 34)。
pub fn data_dir() -> PathBuf {
    resolve_dirs().2
}

/// home / plugins / 数据目录三元组:Runtime::start 与 data_dir 的
/// 单一解析来源。
fn resolve_dirs() -> (PathBuf, PathBuf, PathBuf) {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let plugins = std::env::var_os("PLUGINS_DIR")
        .map_or_else(|| home.join(".aubridge/plugins"), PathBuf::from);
    let data = plugins
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    (home, plugins, data)
}

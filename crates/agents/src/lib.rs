//! agents —— agent wiring 三件套(设计文档 §8.6,magpie 语义)。
//!
//! - `stash`:改写前备份原值,断开时原样还原;
//! - `applied`:记录 aubridge 写过什么,检测第三方改动(drift);
//! - 原子写(temp + rename),保留用户已有字段(整体读 → 改目标字段 →
//!   落盘,绝不基于快照覆盖——sub2api 凭据回滚事故同款纪律)。
//!
//! 本 crate 纯文件 IO,零重依赖;路径由调用方注入(可测)。
//!
//! M2:Codex(gemini/opencode 模块)与 Gemini CLI、OpenCode 接线
//(字段已按 magpie internal/agent 核实)。

mod advanced;
use crate::codex::Codex;
use crate::gemini::GeminiCli;
use crate::opencode::OpenCode;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub use advanced::{read_config, test_command, write_config};

use serde::{Deserialize, Serialize};

use serde_json::{json, Value};

/// Agent 接线行为合同(变更记录 30 评审修正):抽象契约,
/// **不抽象改写算法骨架**——各 agent 文件形态差异太大(codex 是
/// 整文件字节 stash),硬抽模板方法 = 伪抽象。真共享的原子写/JSON
/// 读写保持各模块 free helper。新 agent = 一个 impl 模块 +
/// `wirings()` 注册表一行,gateway 无 match 分支。
pub trait AgentWiring {
    /// 稳定 id(admin 路由/UI key;本机 token `aubridge-<id>`,§6)。
    fn id(&self) -> &'static str;
    /// 展示名(UI)。
    fn name(&self) -> &'static str;
    /// 接线(幂等):stash 先行,写 base_url + 本机 token。
    fn apply(&self) -> std::io::Result<()>;
    /// 断开:按 stash 原样还原(无 stash = 未接线,不动)。
    fn disconnect(&self) -> std::io::Result<()>;
    /// 是否在位(接线值匹配)。
    fn wired(&self) -> bool;
    /// 安装检测:只看配置文件存在,不探 PATH(变更记录 30)。
    fn detected(&self) -> bool;
    /// drift:接线字段被第三方改写。仅 Claude Code 有此语义。
    fn drift(&self) -> Option<Drift> {
        None
    }
    /// 接线后残余用户动作提示(仅 codex:导出环境变量)。
    fn hint(&self) -> Option<&'static str> {
        None
    }
    /// 当前使用的模型(读该 agent 自己的配置文件;None = 未设置或
    /// 该 agent 无单一模型字段)。变更记录 32-②:编辑弹层数据源。
    fn current_model(&self) -> Option<String> {
        None
    }
    /// 设置使用的模型(写该 agent 配置的目标字段;agent 之后请求
    /// 带该模型名,网关按模型/自动别名解析候选)。无单一模型字段的
    /// agent(如 gemini 经 -m 传参)走默认空实现。OpenCode 仅在接线
    /// 仍受管时显式选择，并保留首次默认值供断开恢复。
    fn set_model(&self, _model: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// 各 agent 配置文件路径(组装层注入,可测;变更记录 30 ③)。
pub struct AgentsPaths {
    /// Claude Code:`~/.claude/settings.json`($CLAUDE_CONFIG_DIR 可变)。
    pub claude_settings: PathBuf,
    /// Codex:`~/.codex/config.toml`($CODEX_HOME 可变)。
    pub codex_config: PathBuf,
    /// OpenCode:`~/.config/opencode/opencode.json`。
    pub opencode_config: PathBuf,
    /// Gemini CLI:`~/.gemini/settings.json`。
    pub gemini_settings: PathBuf,
    /// Gemini CLI:`~/.gemini/.env`。
    pub gemini_env: PathBuf,
    /// stash/applied 存放目录(~/.aubridge/agents/)。
    pub state_dir: PathBuf,
}

/// 注册表(变更记录 30):新 agent = 一个 impl 模块 + 这里一行。
/// 每次调用现建(admin 面低频,构造四个小结构零成本)——端口在
/// serve() 绑定后才权威(§4 回退),不能进程启动时焊死。
pub fn wirings(
    paths: &AgentsPaths,
    port: u16,
    models: Vec<(String, String)>,
) -> Vec<Box<dyn AgentWiring>> {
    vec![
        Box::new(ClaudeCode::new(
            paths.claude_settings.clone(),
            paths.state_dir.clone(),
            port,
        )),
        Box::new(Codex::new(
            paths.codex_config.clone(),
            paths.state_dir.clone(),
            port,
        )),
        Box::new(
            OpenCode::new(paths.opencode_config.clone(), paths.state_dir.clone(), port)
                .with_models(models),
        ),
        Box::new(GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            port,
        )),
    ]
}

/// Claude Code 接线:`~/.claude/settings.json` 的 env 段
/// (magpie `agents/claude_code.go` 语义)。
pub struct ClaudeCode {
    /// settings.json 路径(调用方注入,$CLAUDE_CONFIG_DIR 可变)。
    pub settings: PathBuf,
    /// stash/applied 存放目录(~/.aubridge/agents/)。
    pub state_dir: PathBuf,
    pub port: u16,
}

/// 接线结果。
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub fields: Vec<String>,
    pub stashed: Value,
}

/// drift 检测结论。
#[derive(Debug, PartialEq)]
pub enum Drift {
    /// 在位且未被第三方改动。
    Clean,
    /// 配置仍指向 aubridge 写入的旧端口。
    PortChanged,
    /// 接线字段被第三方改写(警告用户)。
    Replaced,
    /// 从未接线或已断开。
    Unwired,
}

const BASE_URL_FIELD: &str = "ANTHROPIC_BASE_URL";
const AUTH_FIELD: &str = "ANTHROPIC_AUTH_TOKEN";
const AUBRIDGE_TOKEN: &str = "aubridge-claude-code";
const MANAGED_FIELDS: [&str; 2] = [BASE_URL_FIELD, AUTH_FIELD];

#[derive(Clone, Copy, Deserialize, Serialize)]
struct WiringMarker {
    port: u16,
    // Either port is ours if an interrupted rewire left the old settings in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_port: Option<u16>,
}

impl WiringMarker {
    fn port_for_base(self, settings: &Value) -> Option<u16> {
        let base = env_field(settings, BASE_URL_FIELD)?.as_str()?;
        [Some(self.port), self.previous_port]
            .into_iter()
            .flatten()
            .find(|&port| base == base_url(port))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DisconnectPreview {
    pub changes: Vec<DisconnectChange>,
    pub blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DisconnectChange {
    pub field: String,
    pub action: RestoreAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreAction {
    Restore,
    Remove,
    Keep,
}

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn env_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    value.get("env")?.get(field)
}

fn managed_token(settings: &Value) -> bool {
    env_field(settings, AUTH_FIELD).and_then(Value::as_str) == Some(AUBRIDGE_TOKEN)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

impl ClaudeCode {
    pub fn new(settings: PathBuf, state_dir: PathBuf, port: u16) -> Self {
        Self {
            settings,
            state_dir,
            port,
        }
    }

    /// 首次接线先保存原值。修复端口时沿用首次备份。
    pub fn apply(&self) -> io::Result<Applied> {
        let original = self.read_settings()?;
        if self.matches_port(&original, self.port) {
            return Ok(Applied {
                fields: Vec::new(),
                stashed: json!({}),
            });
        }
        let marker = self.marker()?;
        let previous_port = marker
            .and_then(|marker| marker.port_for_base(&original))
            .filter(|_| managed_token(&original));
        let stashed = match self.backup()? {
            Some(backup) => {
                let still_original = MANAGED_FIELDS
                    .iter()
                    .all(|field| env_field(&original, field) == env_field(&backup, field));
                if previous_port.is_none() && !still_original {
                    return Err(io::Error::other(
                        "Claude Code 接线字段已被修改，请先查看断开预览",
                    ));
                }
                backup
            }
            None if marker.is_some() => {
                return Err(io::Error::other("Claude Code 接线备份缺失"));
            }
            None => {
                let env: serde_json::Map<String, Value> = MANAGED_FIELDS
                    .iter()
                    .filter_map(|&field| {
                        env_field(&original, field).map(|value| (field.into(), value.clone()))
                    })
                    .collect();
                let backup = json!({ "env": env, "env_present": original.get("env").is_some() });
                atomic_write_json(&self.stash_path(), &backup)?;
                backup
            }
        };

        let mut next = original.clone();
        let env = next
            .as_object_mut()
            .expect("validated settings")
            .entry("env")
            .or_insert_with(|| json!({}));
        env[BASE_URL_FIELD] = json!(base_url(self.port));
        if previous_port.is_none() {
            env[AUTH_FIELD] = json!(AUBRIDGE_TOKEN);
        }
        let mut marker = WiringMarker {
            port: self.port,
            previous_port,
        };
        // Record the pending port before changing the client so a restart
        // can recognize either exact value if one of the writes fails.
        atomic_write_json(&self.applied_path(), &json!(marker))?;
        if self.read_settings()? != original {
            return Err(io::Error::other("Claude Code 配置已变化，请重新接线"));
        }
        atomic_write_json(&self.settings, &next)?;
        if marker.previous_port.take().is_some() {
            let _ = atomic_write_json(&self.applied_path(), &json!(marker));
        }

        Ok(Applied {
            fields: if previous_port.is_some() {
                vec![BASE_URL_FIELD.into()]
            } else {
                MANAGED_FIELDS.iter().map(|field| (*field).into()).collect()
            },
            stashed,
        })
    }

    pub fn repair_port(&self) -> io::Result<()> {
        let Some(marker) = self.marker()? else {
            return Ok(());
        };
        let settings = self.read_settings()?;
        if managed_token(&settings)
            && marker
                .port_for_base(&settings)
                .is_some_and(|port| port != self.port)
        {
            self.apply()?;
        }
        Ok(())
    }

    pub fn disconnect_preview(&self) -> io::Result<DisconnectPreview> {
        let current = self.read_settings()?;
        self.disconnect_plan(&current).map(|(preview, _)| preview)
    }

    pub fn disconnect(&self) -> io::Result<()> {
        self.disconnect_reviewed(None)
    }

    pub fn disconnect_reviewed(&self, expected: Option<&DisconnectPreview>) -> io::Result<()> {
        let current = self.read_settings()?;
        let (preview, next) = self.disconnect_plan(&current)?;
        if expected.is_some_and(|expected| expected != &preview) {
            return Err(io::Error::other("Claude Code 配置已变化，请重新预览"));
        }
        if preview.blocked {
            return Err(io::Error::other(
                "Claude Code 地址和凭据已被部分改写，请先核对这两个字段",
            ));
        }
        if next != current {
            if self.read_settings()? != current {
                return Err(io::Error::other("Claude Code 配置已变化，请重新预览"));
            }
            atomic_write_json(&self.settings, &next)?;
        }
        remove_if_present(&self.applied_path())?;
        remove_if_present(&self.stash_path())
    }

    fn disconnect_plan(&self, current: &Value) -> io::Result<(DisconnectPreview, Value)> {
        let mut next = current.clone();
        let marker = self.marker()?;
        let backup = self.backup()?;
        let mut preview = DisconnectPreview {
            changes: Vec::new(),
            blocked: false,
        };
        let Some(backup) = backup else {
            if marker.is_some() {
                return Err(io::Error::other("Claude Code 接线备份缺失"));
            }
            return Ok((preview, next));
        };
        let base_owned = marker
            .and_then(|marker| marker.port_for_base(current))
            .is_some();
        let auth_owned = marker.is_some() && managed_token(current);
        // Do not pair a credential with a different endpoint after an outside
        // edit. If the other field is already restored, completing the pair is safe.
        preview.blocked = (base_owned
            && !auth_owned
            && env_field(current, AUTH_FIELD) != env_field(&backup, AUTH_FIELD))
            || (auth_owned
                && !base_owned
                && env_field(current, BASE_URL_FIELD) != env_field(&backup, BASE_URL_FIELD));
        for (field, owned) in [(BASE_URL_FIELD, base_owned), (AUTH_FIELD, auth_owned)] {
            let original = env_field(&backup, field);
            let action = if !owned || preview.blocked || env_field(current, field) == original {
                RestoreAction::Keep
            } else if let Some(original) = original {
                next["env"][field] = original.clone();
                RestoreAction::Restore
            } else {
                next["env"]
                    .as_object_mut()
                    .expect("owned env field")
                    .remove(field);
                RestoreAction::Remove
            };
            preview.changes.push(DisconnectChange {
                field: field.into(),
                action,
            });
        }
        let had_env = backup["env_present"]
            .as_bool()
            .unwrap_or_else(|| backup["env"].as_object().is_some_and(|env| !env.is_empty()));
        if next != *current && !had_env && next["env"].as_object().is_some_and(|env| env.is_empty())
        {
            next.as_object_mut()
                .expect("validated settings")
                .remove("env");
        }
        Ok((preview, next))
    }

    /// 匹配完整回环地址和 token，避免把第三方同端口地址识别为本机接线。
    pub fn wired(&self) -> bool {
        self.read_settings()
            .is_ok_and(|settings| self.matches_port(&settings, self.port))
    }

    fn matches_port(&self, settings: &Value, port: u16) -> bool {
        env_field(settings, BASE_URL_FIELD).and_then(Value::as_str) == Some(base_url(port).as_str())
            && managed_token(settings)
    }

    pub fn drift(&self) -> Drift {
        let marker = match self.marker() {
            Ok(Some(marker)) => marker,
            Ok(None) => return Drift::Unwired,
            Err(_) => return Drift::Replaced,
        };
        let Ok(settings) = self.read_settings() else {
            return Drift::Replaced;
        };
        if !managed_token(&settings) {
            return Drift::Replaced;
        }
        match marker.port_for_base(&settings) {
            Some(port) if port == self.port => Drift::Clean,
            Some(_) => Drift::PortChanged,
            None => Drift::Replaced,
        }
    }

    /// 当前模型:settings.json 顶层 `model` 字段(未设置 = None)。
    pub fn current_model(&self) -> Option<String> {
        let s = self.read_settings().ok()?;
        s.get("model").and_then(|m| m.as_str()).map(str::to_string)
    }

    /// 设置模型:整体读 → 改顶层 `model` 与已有模型环境变量 →
    /// 原子写(用户其余字段保留;文件不存在则建——设模型先于接线是
    /// 合法顺序)。Claude Code 的环境变量优先级高于 settings 的
    /// `model`,只改顶层会让 UI 选择看起来"不生效";因此对已存在的
    /// ANTHROPIC_MODEL、角色映射和子 Agent 模型一并同步。
    pub fn set_model(&self, model: &str) -> std::io::Result<()> {
        let mut v = self.read_settings()?;
        v.as_object_mut()
            .expect("settings root is object")
            .insert("model".into(), json!(model));
        if let Some(env) = v.get_mut("env").and_then(Value::as_object_mut) {
            sync_model_env(env, model);
        }
        atomic_write_json(&self.settings, &v)
    }

    fn stash_path(&self) -> PathBuf {
        self.state_dir.join("claude-code.stash.json")
    }
    fn applied_path(&self) -> PathBuf {
        self.state_dir.join("claude-code.applied.json")
    }
    fn read_settings(&self) -> io::Result<Value> {
        let settings = read_optional_json(&self.settings)?.unwrap_or_else(|| json!({}));
        if !settings.is_object() || settings.get("env").is_some_and(|env| !env.is_object()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude Code 配置格式无效",
            ));
        }
        Ok(settings)
    }

    fn marker(&self) -> io::Result<Option<WiringMarker>> {
        read_optional_json(&self.applied_path())?
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|_| io::Error::other("Claude Code 接线记录格式无效"))
            })
            .transpose()
    }

    fn backup(&self) -> io::Result<Option<Value>> {
        let backup = read_optional_json(&self.stash_path())?;
        if backup
            .as_ref()
            .is_some_and(|value| !value["env"].is_object())
        {
            return Err(io::Error::other("Claude Code 接线备份格式无效"));
        }
        Ok(backup)
    }
}

impl AgentWiring for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn name(&self) -> &'static str {
        "Claude Code"
    }
    fn apply(&self) -> std::io::Result<()> {
        // 显式 inherent 调用:trait 内 self.apply() 会解析到本 trait。
        ClaudeCode::apply(self).map(|_| ())
    }
    fn disconnect(&self) -> std::io::Result<()> {
        ClaudeCode::disconnect(self)
    }
    fn wired(&self) -> bool {
        ClaudeCode::wired(self)
    }
    fn detected(&self) -> bool {
        self.settings.exists()
    }
    fn drift(&self) -> Option<Drift> {
        Some(ClaudeCode::drift(self))
    }
    fn current_model(&self) -> Option<String> {
        ClaudeCode::current_model(self)
    }
    fn set_model(&self, model: &str) -> std::io::Result<()> {
        ClaudeCode::set_model(self, model)
    }
}

fn read_json(path: &Path) -> std::io::Result<Value> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn read_optional_json(path: &Path) -> io::Result<Option<Value>> {
    match read_json(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Stable within this process and across restarts of the same build;
/// the UI only needs to detect that the file changed between load and
/// save.
pub(crate) fn content_revision(text: Option<&str>) -> String {
    let Some(text) = text else {
        return "missing".into();
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 把"使用模型"同步到 Claude Code 已存在的模型环境变量。只更新
/// 已存在的键,不凭空新增角色映射;未配置角色的用户仍由顶层
/// `model` 控制。
fn sync_model_env(env: &mut serde_json::Map<String, Value>, model: &str) {
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        if env.contains_key(key) {
            env.insert(key.into(), json!(model));
        }
    }
    for key in [
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    ] {
        if env.contains_key(key) {
            env.insert(key.into(), json!(model));
        }
    }
}

/// Tighten permissions on imported wiring backups without reading their contents.
pub fn protect_wiring_backups(state_dir: &Path) -> [(&'static str, io::Result<()>); 2] {
    ["claude-code.stash.json", "opencode.stash.json"]
        .map(|name| (name, protect_wiring_backup(state_dir, name)))
}

#[cfg(unix)]
fn protect_wiring_backup(state_dir: &Path, name: &str) -> io::Result<()> {
    use std::fs::{self, File, Metadata, Permissions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = match fs::symlink_metadata(state_dir) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !directory.is_dir() {
        return Err(io::Error::other("接线备份路径不是独立目录"));
    }
    let path = state_dir.join(name);
    let entry = match fs::symlink_metadata(&path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let independent_file = |metadata: &Metadata| metadata.is_file() && metadata.nlink() == 1;
    if !independent_file(&entry) {
        return Err(io::Error::other("接线备份不是独立的普通文件"));
    }
    if entry.mode() & 0o077 == 0 {
        return Ok(());
    }
    let file = File::open(&path)?;
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(&path)?;
    let current_directory = fs::symlink_metadata(state_dir)?;
    let identity = |metadata: &Metadata| (metadata.dev(), metadata.ino());
    if !independent_file(&opened)
        || !independent_file(&current)
        || identity(&opened) != identity(&entry)
        || identity(&current) != identity(&opened)
        || !current_directory.is_dir()
        || identity(&current_directory) != identity(&directory)
    {
        return Err(io::Error::other("接线备份路径或文件已变化"));
    }
    if current.mode() & 0o077 == 0 {
        return Ok(());
    }
    file.set_permissions(Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn protect_wiring_backup(_state_dir: &Path, _name: &str) -> io::Result<()> {
    Ok(())
}

/// 原子写，临时文件在目标目录并对其他用户保密。
fn atomic_write_json(path: &Path, value: &Value) -> io::Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_install(path, bytes, false)
}

fn atomic_create(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_install(path, bytes, true)
}

fn atomic_install(path: &Path, bytes: &[u8], create: bool) -> io::Result<()> {
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let seq = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".aubridge-{}-{seq}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    let result = result.and_then(|()| {
        if create {
            std::fs::hard_link(&temp, path)
        } else {
            std::fs::rename(&temp, path)
        }
    });
    if create || result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aubridge-agents-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("settings.json"), dir.join("state"))
    }

    #[cfg(unix)]
    #[test]
    fn protect_wiring_backups_tightens_only_existing_known_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let (settings, state) = tmp("protect-wiring-backups");
        std::fs::create_dir(&state).unwrap();
        let claude = state.join("claude-code.stash.json");
        let opencode = state.join("opencode.stash.json");
        let unrelated = state.join("unrelated.json");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        for (name, result) in protect_wiring_backups(&state) {
            assert!(result.is_ok(), "{name}: {result:?}");
            assert!(!state.join(name).exists());
        }

        for (path, bytes) in [
            (&claude, b"claude fixture".as_slice()),
            (&opencode, b"opencode fixture".as_slice()),
            (&unrelated, b"unrelated".as_slice()),
            (&settings, b"original configuration".as_slice()),
        ] {
            std::fs::write(path, bytes).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let claude_inode = std::fs::metadata(&claude).unwrap().ino();
        let opencode_inode = std::fs::metadata(&opencode).unwrap().ino();
        for _ in 0..2 {
            let [(claude_name, claude_result), (opencode_name, opencode_result)] =
                protect_wiring_backups(&state);
            assert_eq!(claude_name, "claude-code.stash.json");
            assert_eq!(opencode_name, "opencode.stash.json");
            claude_result.unwrap();
            opencode_result.unwrap();
            assert_eq!(mode(&claude), 0o600);
            assert_eq!(mode(&opencode), 0o600);
            assert_eq!(std::fs::read(&claude).unwrap(), b"claude fixture");
            assert_eq!(std::fs::read(&opencode).unwrap(), b"opencode fixture");
            assert_eq!(std::fs::metadata(&claude).unwrap().ino(), claude_inode);
            assert_eq!(std::fs::metadata(&opencode).unwrap().ino(), opencode_inode);
        }
        assert_eq!(mode(&unrelated), 0o644);
        assert_eq!(mode(&settings), 0o644);
        assert_eq!(std::fs::read(&settings).unwrap(), b"original configuration");
        std::fs::remove_dir_all(settings.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn protect_wiring_backups_rejects_unsafe_paths_without_blocking_other_backup() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        for kind in ["symlink", "hardlink", "directory"] {
            let (target, state) = tmp(&format!("protect-wiring-backups-{kind}"));
            std::fs::create_dir(&state).unwrap();
            let unsafe_backup = state.join("claude-code.stash.json");
            let safe_backup = state.join("opencode.stash.json");
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            std::fs::write(&target, b"external fixture").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(&safe_backup, b"safe fixture").unwrap();
            std::fs::set_permissions(&safe_backup, std::fs::Permissions::from_mode(0o644)).unwrap();
            match kind {
                "symlink" => symlink(&target, &unsafe_backup).unwrap(),
                "hardlink" => std::fs::hard_link(&target, &unsafe_backup).unwrap(),
                _ => {
                    std::fs::create_dir(&unsafe_backup).unwrap();
                    std::fs::set_permissions(
                        &unsafe_backup,
                        std::fs::Permissions::from_mode(0o755),
                    )
                    .unwrap();
                    std::fs::write(unsafe_backup.join("marker"), b"inside directory").unwrap();
                }
            }
            let [(_, rejected), (_, protected)] = protect_wiring_backups(&state);
            assert!(rejected.is_err(), "{kind}");
            protected.unwrap();
            assert_eq!(mode(&target), 0o644, "{kind}");
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"external fixture",
                "{kind}"
            );
            assert_eq!(mode(&safe_backup), 0o600, "{kind}");
            assert_eq!(
                std::fs::read(&safe_backup).unwrap(),
                b"safe fixture",
                "{kind}"
            );
            if kind == "directory" {
                assert_eq!(mode(&unsafe_backup), 0o755);
                assert_eq!(
                    std::fs::read(unsafe_backup.join("marker")).unwrap(),
                    b"inside directory"
                );
            }
            std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn apply_then_wired_with_user_fields_preserved() {
        let (settings, state) = tmp("apply");
        std::fs::write(
            &settings,
            r#"{"model": "sonnet", "env": {"CUSTOM": "keep"}}"#,
        )
        .unwrap();
        let cc = ClaudeCode::new(settings.clone(), state.clone(), 3427);
        let applied = cc.apply().expect("apply");
        assert_eq!(applied.fields.len(), 2);

        assert!(cc.wired());
        // 用户字段保留(整体读 → 只改目标字段)。
        let v: Value = read_json(&settings).unwrap();
        assert_eq!(v["model"], "sonnet");
        assert_eq!(v["env"]["CUSTOM"], "keep");
        assert!(v["env"][BASE_URL_FIELD].as_str().unwrap().ends_with("3427"));
    }

    #[test]
    fn disconnect_restores_original() {
        let (settings, state) = tmp("disconnect");
        std::fs::write(&settings, r#"{"env": {"CUSTOM": "keep"}}"#).unwrap();
        let cc = ClaudeCode::new(settings.clone(), state, 3427);
        cc.apply().unwrap();
        cc.disconnect().unwrap();

        assert!(!cc.wired());
        assert_eq!(cc.drift(), Drift::Unwired);
        let v: Value = read_json(&settings).unwrap();
        assert_eq!(v["env"]["CUSTOM"], "keep");
        assert!(v["env"].get(BASE_URL_FIELD).is_none());
    }

    #[test]
    fn port_repair_preserves_a_legacy_backup_and_later_user_edits() {
        let (settings, state) = tmp("port-repair");
        std::fs::create_dir_all(&state).unwrap();
        let cc = ClaudeCode::new(settings.clone(), state.clone(), 6543);
        let backup = json!({
            "env": {BASE_URL_FIELD: "https://origin.example", AUTH_FIELD: "original-test-token", "CUSTOM": "old"},
            "mtime": 1,
        });
        atomic_write_json(&cc.stash_path(), &backup).unwrap();
        atomic_write_json(
            &cc.applied_path(),
            &json!({ "fields": MANAGED_FIELDS, "port": 3427 }),
        )
        .unwrap();
        atomic_write_json(&settings, &json!({
            "model": "user-choice",
            "env": {BASE_URL_FIELD: base_url(3427), AUTH_FIELD: AUBRIDGE_TOKEN, "CUSTOM": "edited"},
        })).unwrap();
        assert_eq!(cc.drift(), Drift::PortChanged);
        cc.repair_port().unwrap();
        assert!(cc.wired());
        assert_eq!(read_json(&cc.stash_path()).unwrap(), backup);
        let preview = cc.disconnect_preview().unwrap();
        assert!(preview
            .changes
            .iter()
            .all(|change| change.action == RestoreAction::Restore));
        assert!(!serde_json::to_string(&preview)
            .unwrap()
            .contains("original-test-token"));
        cc.disconnect_reviewed(Some(&preview)).unwrap();
        let restored = read_json(&settings).unwrap();
        assert_eq!(restored["env"][BASE_URL_FIELD], "https://origin.example");
        assert_eq!(restored["env"][AUTH_FIELD], "original-test-token");
        assert_eq!(restored["env"]["CUSTOM"], "edited");
        assert_eq!(restored["model"], "user-choice");
    }

    #[test]
    fn drift_detects_third_party_change() {
        let (settings, state) = tmp("drift");
        let cc = ClaudeCode::new(settings.clone(), state, 3427);
        cc.apply().unwrap();
        assert_eq!(cc.drift(), Drift::Clean);

        // 第三方改写接线字段。
        let mut v: Value = read_json(&settings).unwrap();
        v["env"][BASE_URL_FIELD] = json!("http://elsewhere:9999");
        atomic_write_json(&settings, &v).unwrap();
        assert_eq!(cc.drift(), Drift::Replaced);
        assert!(cc.disconnect_preview().unwrap().blocked);
        assert!(cc.apply().is_err());
        assert!(cc.disconnect().is_err());
        assert_eq!(read_json(&settings).unwrap(), v);
    }

    #[test]
    fn set_and_current_model_preserve_user_fields() {
        let (settings, state) = tmp("model");
        std::fs::write(
            &settings,
            r#"{"model":"sonnet","env":{"CUSTOM":"keep","ANTHROPIC_MODEL":"sonnet","ANTHROPIC_DEFAULT_SONNET_MODEL":"sonnet","ANTHROPIC_DEFAULT_SONNET_MODEL_NAME":"Sonnet","CLAUDE_CODE_SUBAGENT_MODEL":"haiku","ANTHROPIC_DEFAULT_OPUS_MODEL":"opus"}}"#,
        )
        .unwrap();
        let cc = ClaudeCode::new(settings.clone(), state, 3427);

        assert_eq!(cc.current_model().as_deref(), Some("sonnet"));
        cc.set_model("glm-4.5-flash").unwrap();
        assert_eq!(cc.current_model().as_deref(), Some("glm-4.5-flash"));

        let v: Value = read_json(&settings).unwrap();
        assert_eq!(v["env"]["CUSTOM"], "keep", "用户字段保留");
        assert_eq!(v["env"]["ANTHROPIC_MODEL"], "glm-4.5-flash");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-4.5-flash");
        assert_eq!(
            v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"],
            "glm-4.5-flash"
        );
        assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "glm-4.5-flash");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-4.5-flash");
        // 文件不存在:建文档只带 model(设模型先于接线是合法顺序)。
        let (fresh, state2) = tmp("model-fresh");
        let cc2 = ClaudeCode::new(fresh.clone(), state2, 3427);
        assert_eq!(cc2.current_model(), None);
        cc2.set_model("aubridge").unwrap();
        assert_eq!(cc2.current_model().as_deref(), Some("aubridge"));
    }
}

pub mod codex;
pub mod gemini;
pub mod opencode;

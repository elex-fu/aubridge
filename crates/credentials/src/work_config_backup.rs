//! Decision 55: logical, secret-free same-machine work configuration archives.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub format: String,
    pub version: u32,
    pub payload: Payload,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub sources: Vec<Source>,
    pub groups: Vec<Group>,
    pub model_selections: BTreeMap<String, Vec<String>>,
    pub paused_providers: Vec<String>,
    pub chatgpt_login_source: String,
    pub client_wiring: Vec<WiringIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_intent: Option<crate::usage_policy::CostIntent>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub source: SourceKind,
    pub base: String,
    pub auth: String,
    pub protocol: String,
    pub kind: String,
    pub models: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Custom,
    Preset,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    pub members: Vec<String>,
    pub rule: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WiringIntent {
    pub agent: String,
    pub enabled: bool,
    pub model: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRestore {
    pub payload: Payload,
    pub archive_digest: String,
    pub bound_sources: Vec<String>,
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bytes<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(invalid_json)
}
fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid(error.to_string())
}

impl Archive {
    pub fn encode(mut payload: Payload) -> io::Result<Vec<u8>> {
        payload.validate()?;
        // Source ordering has no preference meaning; group member order does.
        payload.sources.sort_by(|a, b| a.id.cmp(&b.id));
        payload.paused_providers.sort();
        payload.client_wiring.sort_by(|a, b| a.agent.cmp(&b.agent));
        let sha256 = digest(&bytes(&payload)?);
        bytes(&Self {
            format: "aubridge-work-config".into(),
            version: 1,
            payload,
            sha256,
        })
    }
    pub fn decode(input: &[u8]) -> io::Result<Self> {
        if input.len() > 4 * 1024 * 1024 {
            return Err(invalid("归档过大"));
        }
        let value = strict_json(input)?;
        let archive: Self = serde_json::from_value(value).map_err(invalid_json)?;
        if archive.format != "aubridge-work-config" || archive.version != 1 {
            return Err(invalid("不支持的工作配置归档版本"));
        }
        archive.payload.validate()?;
        if archive.sha256 != digest(&bytes(&archive.payload)?) {
            return Err(invalid("归档 checksum 不匹配"));
        }
        Ok(archive)
    }
}
fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains(char::is_control)
}
fn unique(values: &[String]) -> bool {
    values.iter().all(|s| identifier(s))
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}
pub fn canonical_base(value: &str) -> io::Result<String> {
    if value.trim() != value
        || value.contains(char::is_control)
        || value.split_once("://").is_some_and(|(_, rest)| {
            rest.split('/')
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
    {
        return Err(invalid("来源地址包含用户信息或空白"));
    }
    let url = reqwest::Url::parse(value).map_err(|_| invalid("来源地址无效"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("来源地址不能包含用户信息、查询或片段"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}
impl Source {
    pub fn identity(&self) -> io::Result<String> {
        Ok(digest(&bytes(&(
            canonical_base(&self.base)?,
            &self.auth,
            &self.protocol,
            &self.kind,
        ))?))
    }
    pub fn same_connection(&self, other: &Self) -> io::Result<bool> {
        Ok(self.id == other.id && self.identity()? == other.identity()?)
    }
}
impl Payload {
    pub fn validate(&self) -> io::Result<()> {
        if self.sources.len() > 256 || self.groups.len() > 256 || self.client_wiring.len() > 4 {
            return Err(invalid("归档条目过多"));
        }
        let mut ids = BTreeSet::new();
        let mut models = BTreeSet::new();
        for source in &self.sources {
            if !identifier(&source.id)
                || !source
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                || !ids.insert(source.id.clone())
            {
                return Err(invalid("重复或无效来源 ID"));
            }
            canonical_base(&source.base)?;
            if !matches!(source.auth.as_str(), "bearer" | "api-key" | "claude_cli")
                || !matches!(source.protocol.as_str(), "chat" | "anthropic" | "responses")
                || !matches!(source.kind.as_str(), "free_tier" | "subscription")
                || !unique(&source.models)
                || source.models.len() > 1000
            {
                return Err(invalid("来源 schema 无效"));
            }
            if source.source == SourceKind::Custom
                && (source.models.is_empty()
                    || source.auth == "claude_cli"
                    || source.protocol == "responses")
            {
                return Err(invalid("不支持的自定义来源协议"));
            }
            models.extend(source.models.iter().cloned());
        }
        for (source, selected) in &self.model_selections {
            let definition = self
                .sources
                .iter()
                .find(|s| &s.id == source)
                .ok_or_else(|| invalid("模型选择来源不存在"))?;
            if !unique(selected)
                || selected.is_empty()
                || selected.iter().any(|m| !definition.models.contains(m))
            {
                return Err(invalid("模型选择引用无效"));
            }
        }
        if !unique(&self.paused_providers)
            || self.paused_providers.iter().any(|id| !ids.contains(id))
        {
            return Err(invalid("暂停来源引用无效"));
        }
        let mut names = BTreeSet::new();
        for group in &self.groups {
            if !identifier(&group.name)
                || group.name.starts_with("group/")
                || group.name == "aubridge"
                || !names.insert(group.name.clone())
                || group.members.is_empty()
                || !unique(&group.members)
                || group.members.iter().any(|model| !models.contains(model))
                || !matches!(
                    group.rule.as_str(),
                    "free_first" | "free_only" | "sequential" | "round_robin" | "smart" | "pace"
                )
            {
                return Err(invalid("分组重复或引用无效"));
            }
        }
        if !matches!(
            self.chatgpt_login_source.as_str(),
            "" | "codex" | "opencode"
        ) {
            return Err(invalid("登录来源偏好不支持"));
        }
        let mut agents = BTreeSet::new();
        for intent in &self.client_wiring {
            if !matches!(
                intent.agent.as_str(),
                "codex" | "claude-code" | "opencode" | "gemini"
            ) || !agents.insert(&intent.agent)
                || (!intent.model.is_empty()
                    && intent.model != "aubridge"
                    && !models.contains(&intent.model)
                    && !names.contains(&intent.model))
            {
                return Err(invalid("客户端意图引用无效"));
            }
        }
        Ok(())
    }
}

// Value's normal deserializer accepts duplicate keys. Archives must reject them at every depth.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut output = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueJson>()? {
                    if output.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueJson(Value::Object(output)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(value) = seq.next_element::<UniqueJson>()? {
                    out.push(value.0);
                }
                Ok(UniqueJson(Value::Array(out)))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(v.into())))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueJson(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
fn strict_json(input: &[u8]) -> io::Result<Value> {
    serde_json::from_slice::<UniqueJson>(input)
        .map(|v| v.0)
        .map_err(invalid_json)
}
pub fn read_document(path: &Path) -> io::Result<Value> {
    read_snapshot(path).map(|(value, _)| value)
}
fn read_snapshot(path: &Path) -> io::Result<(Value, Option<Vec<u8>>)> {
    match std::fs::read(path) {
        Ok(input) => {
            let value = strict_json(&input)?;
            if !value.is_object() {
                return Err(invalid("配置需要对象"));
            }
            Ok((value, Some(input)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((serde_json::json!({}), None)),
        Err(e) => Err(e),
    }
}
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub pending: PendingRestore,
    pub target_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Undo {
    pub before: BTreeMap<String, Option<Value>>,
    pub after: BTreeMap<String, Option<Value>>,
}
impl RestorePlan {
    pub fn preview(archive: &Archive, target: &Value, presets: &[Source]) -> io::Result<Self> {
        Archive::decode(&bytes(archive)?)?;
        if !target.is_object() {
            return Err(invalid("目标配置需要对象"));
        }
        for (field, identity) in [("providers", "id"), ("model_groups", "name")] {
            if let Some(value) = target.get(field) {
                let items = value
                    .as_array()
                    .ok_or_else(|| invalid("目标配置集合格式无效"))?;
                let mut seen = BTreeSet::new();
                for item in items {
                    let id = item[identity]
                        .as_str()
                        .ok_or_else(|| invalid("目标配置条目标识无效"))?;
                    if !seen.insert(id) {
                        return Err(invalid("目标配置条目标识重复"));
                    }
                }
            }
        }
        for source in &archive.payload.sources {
            if source.source == SourceKind::Preset {
                let trusted = presets
                    .iter()
                    .find(|p| p.id == source.id)
                    .ok_or_else(|| invalid("本版本不含该内置来源"))?;
                if !source.same_connection(trusted)? {
                    return Err(invalid("内置来源身份不匹配"));
                }
            } else if presets.iter().any(|p| p.id == source.id) {
                return Err(invalid("自定义来源与内置 ID 冲突"));
            }
            if let Some(current) = target
                .get("providers")
                .and_then(Value::as_array)
                .and_then(|ps| ps.iter().find(|p| p["id"].as_str() == Some(&source.id)))
            {
                let current: crate::settings::CustomProvider =
                    serde_json::from_value(current.clone()).map_err(invalid_json)?;
                let identity = Source {
                    id: current.id,
                    source: SourceKind::Custom,
                    base: current.base,
                    auth: current.auth,
                    protocol: current.protocol,
                    kind: current.kind,
                    models: current.models,
                };
                if source.source != SourceKind::Custom || !source.same_connection(&identity)? {
                    return Err(invalid("现有同 ID 来源的端点、鉴权、协议或费用类别冲突"));
                }
            }
        }
        if target
            .get("pending_work_config_restore")
            .is_some_and(|v| !v.is_null())
        {
            return Err(invalid("已有待完成恢复，请先取消或撤销"));
        }
        Ok(Self {
            pending: PendingRestore {
                payload: archive.payload.clone(),
                archive_digest: archive.sha256.clone(),
                bound_sources: Vec::new(),
            },
            target_revision: digest(&bytes(target)?),
        })
    }
    pub fn commit(&self, config: &Path) -> io::Result<Undo> {
        patch_if_revision(
            config,
            &BTreeMap::from([(
                "pending_work_config_restore".into(),
                Some(serde_json::to_value(&self.pending).map_err(invalid_json)?),
            )]),
            &self.target_revision,
        )
    }
}
/// Caller serializes all configuration edits using agents_io → settings_io.
pub fn patch_if_revision(
    config: &Path,
    changes: &BTreeMap<String, Option<Value>>,
    revision: &str,
) -> io::Result<Undo> {
    patch_guarded(config, changes, Some(revision), None)
}
fn patch_guarded(
    config: &Path,
    changes: &BTreeMap<String, Option<Value>>,
    revision: Option<&str>,
    affected: Option<&BTreeMap<String, Option<Value>>>,
) -> io::Result<Undo> {
    reject_symlink(config)?;
    let (mut current, original_bytes) = read_snapshot(config)?;
    let actual_revision = digest(&bytes(&current)?);
    if revision.is_some_and(|expected| actual_revision != expected) {
        return Err(invalid("目标配置已改变，请重新预览"));
    }
    if affected.is_some_and(|fields| {
        fields
            .iter()
            .any(|(key, value)| current.get(key) != value.as_ref())
    }) {
        return Err(invalid("恢复后受影响配置已被修改，拒绝覆盖"));
    }
    let original = current.clone();
    let before = changes
        .keys()
        .map(|key| (key.clone(), current.get(key).cloned()))
        .collect();
    for (key, value) in changes {
        match value {
            Some(value) => {
                current[key] = value.clone();
            }
            None => {
                current
                    .as_object_mut()
                    .ok_or_else(|| invalid("配置需要对象"))?
                    .remove(key);
            }
        }
    }
    let undo = Undo {
        before,
        after: changes.clone(),
    };
    // Journal first: an interrupted or failed config rename must leave active state intact.
    let journal = config.with_file_name("work-config-restore-before.json");
    reject_symlink(&journal)?;
    reject_symlink(config)?;
    let mut history: Vec<Undo> = match load_private(&journal) {
        Ok(history) => history,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let previous = history.drain(..).rev().find(|receipt| {
        receipt
            .after
            .iter()
            .all(|(key, value)| original.get(key) == value.as_ref())
    });
    let new_stage = changes.len() == 1
        && changes
            .get("pending_work_config_restore")
            .is_some_and(|v| v.as_ref().is_some_and(Value::is_object))
        && undo
            .before
            .get("pending_work_config_restore")
            .is_none_or(|v| v.as_ref().is_none_or(Value::is_null));
    let mut next = if new_stage {
        undo.clone()
    } else {
        previous.clone().unwrap_or_else(|| undo.clone())
    };
    if !new_stage {
        for (key, value) in &undo.before {
            next.before.entry(key.clone()).or_insert(value.clone());
        }
        next.after.extend(undo.after.clone());
    }
    // Keep the committed and proposed receipt: a crash before rename can select the former.
    let records: Vec<_> = previous.into_iter().chain(std::iter::once(next)).collect();
    let encoded = bytes(&records)?;
    if encoded.len() > 32 * 1024 * 1024 {
        return Err(invalid("恢复回执过大"));
    }
    crate::private_file::replace(&journal, &encoded)?;
    crate::private_file::replace_validated(config, &bytes(&current)?, || {
        reject_symlink(config)?;
        let (_, latest_bytes) = read_snapshot(config)?;
        if latest_bytes != original_bytes {
            return Err(invalid("提交期间配置已改变，未覆盖外部修改"));
        }
        Ok(())
    })?;
    Ok(undo)
}
impl Undo {
    pub fn apply(&self, config: &Path) -> io::Result<()> {
        patch_guarded(config, &self.before, None, Some(&self.after)).map(|_| ())
    }
}
fn reject_symlink(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(invalid("不接受符号链接文件")),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub fn export(destination: &Path, app_dir: &Path, content: &[u8]) -> io::Result<()> {
    Archive::decode(content)?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("备份路径缺少父目录"))?
        .canonicalize()?;
    let app = app_dir.canonicalize()?;
    if parent.starts_with(&app) {
        return Err(invalid("请选择应用数据目录外的位置"));
    }
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("备份路径缺少文件名"))?;
    let destination = parent.join(name);
    reject_symlink(&destination)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".aubridge-export-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = file
        .write_all(content)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(&temporary, &destination));
    drop(file);
    let _ = std::fs::remove_file(&temporary);
    result
}

/// Private local recovery receipts; never part of the exported archive.
pub fn save_private(path: &Path, value: &impl Serialize) -> io::Result<()> {
    reject_symlink(path)?;
    crate::private_file::replace(path, &bytes(value)?)
}
pub fn load_private<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    reject_symlink(path)?;
    serde_json::from_value(strict_json(&read_limited_file(path, 32 * 1024 * 1024)?)?)
        .map_err(invalid_json)
}

/// Bound memory before reading an untrusted user-selected archive or local receipt.
pub fn read_archive_file(path: &Path) -> io::Result<Vec<u8>> {
    read_limited_file(path, 4 * 1024 * 1024)
}
fn read_limited_file(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid("归档需要大小受限的普通文件"));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("归档需要普通文件"));
    }
    if file.metadata()?.len() > limit {
        return Err(invalid("归档过大"));
    }
    let mut content = Vec::new();
    file.take(limit + 1).read_to_end(&mut content)?;
    if content.len() as u64 > limit {
        return Err(invalid("归档过大"));
    }
    Ok(content)
}

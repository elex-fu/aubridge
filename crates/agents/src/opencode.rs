//! OpenCode connects through one reversible provider block in JSON or JSONC.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use jsonc_parser::ast::{Object, ObjectProp};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstRootNode};
use jsonc_parser::{parse_to_ast, parse_to_serde_value, ParseOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    atomic_write, atomic_write_json, read_optional_json, remove_if_present, AgentWiring,
    DisconnectChange, DisconnectPreview, Drift, RestoreAction,
};

const PROVIDER_ID: &str = "aubridge";
const CONFIG_BLOCK: &str = "provider.aubridge";
const TOKEN: &str = "aubridge-opencode";
const SDK: &str = "@ai-sdk/openai-compatible";

pub struct OpenCode {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub port: u16,
    pub models: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct Wiring {
    path: PathBuf,
    original: Option<String>,
    managed: String,
    previous: Option<String>,
    had_provider: bool,
    had_file: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<ModelWiring>,
}

#[derive(Serialize, Deserialize)]
struct ModelWiring {
    original: Option<String>,
    managed: String,
    previous: Option<String>,
    #[serde(default)]
    pending: bool,
}

impl ModelWiring {
    fn owns(&self, value: Option<&str>) -> bool {
        value.is_some_and(|value| value == self.managed || self.previous.as_deref() == Some(value))
    }

    fn accepts(&self, value: Option<&str>) -> bool {
        self.owns(value)
            || (self.pending && self.previous.is_none() && value == self.original.as_deref())
    }
}

impl Wiring {
    fn owns(&self, block: Option<&str>) -> bool {
        block.is_some_and(|block| self.managed == block || self.previous.as_deref() == Some(block))
    }
}

/// OpenCode loads these global files in reverse precedence.
pub fn config_path(directory: &Path) -> PathBuf {
    ["opencode.jsonc", "opencode.json", "config.json"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| fs::symlink_metadata(path).is_ok())
        .unwrap_or_else(|| directory.join("opencode.json"))
}

impl OpenCode {
    pub fn new(config: PathBuf, state_dir: PathBuf, port: u16) -> Self {
        Self {
            config,
            state_dir,
            port,
            models: Vec::new(),
        }
    }

    pub fn with_models(mut self, models: Vec<(String, String)>) -> Self {
        self.models = models;
        self
    }

    pub fn apply(&self) -> io::Result<()> {
        let path = std::path::absolute(&self.config)?;
        let document = Document::read(&path)?;
        let old = document.block();
        let saved = self.wiring()?;
        if let Some(saved) = &saved {
            if saved.path != path {
                return Err(io::Error::other("OpenCode 配置路径已变化，请先断开原接线"));
            }
            if !saved.owns(old) && old != saved.original.as_deref() {
                return Err(io::Error::other("OpenCode 的受管 Provider 已被修改"));
            }
            if saved
                .model
                .as_ref()
                .is_some_and(|model| !model.accepts(document.model()))
            {
                return Err(io::Error::other("OpenCode 的受管模型已被修改"));
            }
        }
        let managed = self.provider();
        if document.block_value()? == Some(managed.clone()) {
            return Ok(());
        }
        let next = document.set(&managed)?;
        let next = Document::parse(Some(next))?;
        let next_block = next
            .block()
            .ok_or_else(|| io::Error::other("OpenCode 接线校验失败"))?;
        let mut saved = saved.unwrap_or_else(|| Wiring {
            path,
            original: old.map(str::to_owned),
            managed: String::new(),
            previous: None,
            had_provider: document.has_provider,
            had_file: document.text.is_some(),
            model: None,
        });
        // Record both values before writing the config so a retry can recover
        // the original backup after interruption on either side of the write.
        saved.previous = old.filter(|_| saved.owns(old)).map(str::to_owned);
        saved.managed = next_block.into();
        atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        document.write_if_current(&saved.path, next.text.as_deref())?;
        if saved.previous.take().is_some() {
            atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        }
        Ok(())
    }

    pub fn repair(&self) -> io::Result<()> {
        if let Some(saved) = self.wiring()? {
            if saved.path == std::path::absolute(&self.config)?
                && saved.owns(Document::read(&saved.path)?.block())
            {
                self.apply()?;
            }
        }
        Ok(())
    }

    pub fn disconnect_preview(&self) -> io::Result<DisconnectPreview> {
        let Some(saved) = self.wiring()? else {
            return Ok(DisconnectPreview {
                changes: Vec::new(),
                blocked: false,
            });
        };
        let document = Document::read(&saved.path)?;
        Ok(self.preview(&saved, &document))
    }

    pub fn disconnect(&self) -> io::Result<()> {
        self.disconnect_reviewed(None)
    }

    pub fn disconnect_reviewed(&self, expected: Option<&DisconnectPreview>) -> io::Result<()> {
        let Some(saved) = self.wiring()? else {
            return if expected.is_some_and(|preview| !preview.changes.is_empty()) {
                Err(io::Error::other("OpenCode 配置已变化，请重新预览"))
            } else {
                Ok(())
            };
        };
        let document = Document::read(&saved.path)?;
        let preview = self.preview(&saved, &document);
        if expected.is_some_and(|expected| expected != &preview) || preview.blocked {
            return Err(io::Error::other("OpenCode 配置已变化，请重新预览"));
        }
        let next = document.restore(&saved)?;
        document.write_if_current(&saved.path, next.as_deref())?;
        remove_if_present(&self.stash_path())
    }

    fn preview(&self, saved: &Wiring, document: &Document) -> DisconnectPreview {
        let block = document.block();
        let owned = saved.owns(block);
        let action = if !owned || block == saved.original.as_deref() {
            RestoreAction::Keep
        } else if saved.original.is_some() {
            RestoreAction::Restore
        } else {
            RestoreAction::Remove
        };
        let mut changes = vec![DisconnectChange {
            field: CONFIG_BLOCK.into(),
            action,
        }];
        let mut blocked = !owned && block != saved.original.as_deref()
            || std::path::absolute(&self.config).ok().as_deref() != Some(&saved.path);
        if let Some(model) = &saved.model {
            let current = document.model();
            let action = if current == model.original.as_deref() || !model.owns(current) {
                RestoreAction::Keep
            } else if model.original.is_some() {
                RestoreAction::Restore
            } else {
                RestoreAction::Remove
            };
            let restored =
                block == saved.original.as_deref() && current == model.original.as_deref();
            blocked |= !model.accepts(current) && !restored;
            changes.push(DisconnectChange {
                field: "model".into(),
                action,
            });
        }
        DisconnectPreview { changes, blocked }
    }

    pub fn current_model(&self) -> Option<String> {
        let saved = self.wiring().ok()??;
        if saved.path != std::path::absolute(&self.config).ok()? {
            return None;
        }
        let document = Document::read(&saved.path).ok()?;
        if !saved.owns(document.block())
            || document.block_value().ok()?.as_ref().and_then(bridge_port) != Some(self.port)
        {
            return None;
        }
        let model = saved.model?;
        if !model.owns(document.model()) {
            return None;
        }
        let value: String = serde_json::from_str(document.model()?).ok()?;
        value
            .strip_prefix("aubridge/")
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    pub fn set_model(&self, model: &str) -> io::Result<()> {
        if model.is_empty() {
            return Err(io::Error::other("OpenCode 模型不能为空"));
        }
        let mut saved = self
            .wiring()?
            .ok_or_else(|| io::Error::other("OpenCode 尚未接线"))?;
        if saved.path != std::path::absolute(&self.config)? {
            return Err(io::Error::other("OpenCode 配置路径已变化"));
        }
        let document = Document::read(&saved.path)?;
        if !saved.owns(document.block())
            || document.block_value()?.as_ref().and_then(bridge_port) != Some(self.port)
        {
            return Err(io::Error::other(
                "OpenCode 的受管 Provider 已被修改或未接线",
            ));
        }
        if let Some(raw) = document.model() {
            serde_json::from_str::<String>(raw).map_err(|_| invalid_config())?;
        }
        if saved
            .model
            .as_ref()
            .is_some_and(|model| !model.accepts(document.model()))
        {
            return Err(io::Error::other("OpenCode 的受管模型已被修改"));
        }
        let next = document.set_model(&format!("aubridge/{model}"))?;
        let next = Document::parse(Some(next))?;
        let managed = next.model().ok_or_else(invalid_config)?.to_owned();
        let model = saved.model.get_or_insert_with(|| ModelWiring {
            original: document.model().map(str::to_owned),
            managed: String::new(),
            previous: None,
            pending: true,
        });
        model.previous = document
            .model()
            .filter(|raw| model.owns(Some(raw)))
            .map(str::to_owned);
        model.managed = managed;
        model.pending = true;
        atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        document.write_if_current(&saved.path, next.text.as_deref())?;
        if let Some(model) = saved.model.as_mut() {
            model.previous = None;
            model.pending = false;
            atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        }
        Ok(())
    }

    pub fn wired(&self) -> bool {
        Document::read(&self.config)
            .and_then(|document| document.block_value())
            .ok()
            .flatten()
            .as_ref()
            .and_then(bridge_port)
            == Some(self.port)
    }

    fn drift(&self) -> Drift {
        let saved = match self.wiring() {
            Ok(Some(saved)) => saved,
            Ok(None) => return Drift::Unwired,
            Err(_) => return Drift::Replaced,
        };
        let Ok(document) = Document::read(&saved.path) else {
            return Drift::Replaced;
        };
        if std::path::absolute(&self.config).ok().as_deref() != Some(&saved.path) {
            return Drift::Replaced;
        }
        if saved
            .model
            .as_ref()
            .is_some_and(|model| !model.accepts(document.model()))
        {
            return Drift::Replaced;
        }
        if saved.owns(document.block()) {
            if self.wired() {
                Drift::Clean
            } else {
                Drift::PortChanged
            }
        } else if document.block() == saved.original.as_deref() {
            Drift::Unwired
        } else {
            Drift::Replaced
        }
    }

    fn provider(&self) -> Value {
        let models: serde_json::Map<String, Value> = self
            .models
            .iter()
            .map(|(id, name)| (id.clone(), json!({"name": name})))
            .collect();
        json!({
            "npm": SDK, "name": "aubridge",
            "options": {"baseURL": format!("http://127.0.0.1:{}/v1", self.port), "apiKey": TOKEN},
            "models": models,
        })
    }

    fn stash_path(&self) -> PathBuf {
        self.state_dir.join("opencode.stash.json")
    }

    fn wiring(&self) -> io::Result<Option<Wiring>> {
        let Some(value) = read_optional_json(&self.stash_path())? else {
            return Ok(None);
        };
        if value.get("path").is_some() {
            return serde_json::from_value(value)
                .map(Some)
                .map_err(|_| io::Error::other("OpenCode 接线备份格式无效"));
        }
        // Earlier versions kept the original provider but no applied record.
        // Adopt only the exact block layout that those versions wrote.
        let original = value
            .get(CONFIG_BLOCK)
            .ok_or_else(|| io::Error::other("OpenCode 接线备份格式无效"))?;
        let path = std::path::absolute(&self.config)?;
        let document = Document::read(&path)?;
        let actual = document
            .block_value()?
            .filter(legacy_provider)
            .ok_or_else(|| io::Error::other("无法确认旧版 OpenCode 受管 Provider"))?;
        let raw = document
            .block()
            .ok_or_else(|| io::Error::other("OpenCode 受管 Provider 缺失"))?;
        let original = (!original.is_null()).then(|| original.to_string());
        // A legacy block with comments or manual additions must be reviewed as a conflict.
        if serde_json::from_str::<Value>(raw).ok().as_ref() != Some(&actual) {
            return Err(io::Error::other("旧版 OpenCode 受管 Provider 已被修改"));
        }
        Ok(Some(Wiring {
            path,
            original,
            managed: raw.into(),
            previous: None,
            had_provider: true,
            had_file: true,
            model: None,
        }))
    }
}

impl AgentWiring for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn name(&self) -> &'static str {
        "OpenCode"
    }
    fn apply(&self) -> io::Result<()> {
        OpenCode::apply(self)
    }
    fn disconnect(&self) -> io::Result<()> {
        OpenCode::disconnect(self)
    }
    fn wired(&self) -> bool {
        OpenCode::wired(self)
    }
    fn detected(&self) -> bool {
        self.config.exists()
    }
    fn drift(&self) -> Option<Drift> {
        Some(OpenCode::drift(self))
    }
    fn current_model(&self) -> Option<String> {
        OpenCode::current_model(self)
    }
    fn set_model(&self, model: &str) -> io::Result<()> {
        OpenCode::set_model(self, model)
    }
}

fn bridge_port(value: &Value) -> Option<u16> {
    if value["npm"] != SDK || value["options"]["apiKey"] != TOKEN {
        return None;
    }
    value["options"]["baseURL"]
        .as_str()?
        .strip_prefix("http://127.0.0.1:")?
        .strip_suffix("/v1")?
        .parse::<u16>()
        .ok()
}

fn legacy_provider(value: &Value) -> bool {
    bridge_port(value).is_some()
        && value["name"] == "aubridge"
        && value.as_object().is_some_and(|object| object.len() == 4)
        && value["options"]
            .as_object()
            .is_some_and(|object| object.len() == 2)
        && value["models"].as_object().is_some_and(|models| {
            models.values().all(|model| {
                model.as_object().is_some_and(|object| object.len() == 1)
                    && model["name"].is_string()
            })
        })
}

struct Document {
    text: Option<String>,
    has_provider: bool,
    managed: Option<Range<usize>>,
    model: Option<Range<usize>>,
}

impl Document {
    fn read(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::other("OpenCode 配置必须是普通文件"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::parse(None),
            Err(error) => return Err(error),
            _ => {}
        }
        Self::parse(Some(fs::read_to_string(path)?))
    }

    fn parse(text: Option<String>) -> io::Result<Self> {
        let parsed = parse_to_ast(
            text.as_deref().unwrap_or("{}"),
            &Default::default(),
            &parse_options(),
        )
        .map_err(|_| invalid_config())?;
        let root = parsed
            .value
            .as_ref()
            .and_then(|value| value.as_object())
            .ok_or_else(invalid_config)?;
        let provider = unique_property(root, "provider")?;
        let model = unique_property(root, "model")?
            .map(|property| property.value.start()..property.value.end());
        let managed = match provider {
            Some(provider) => unique_property(
                provider.value.as_object().ok_or_else(invalid_config)?,
                PROVIDER_ID,
            )?
            .map(|property| property.value.start()..property.value.end()),
            None => None,
        };
        Ok(Self {
            has_provider: provider.is_some(),
            text,
            managed,
            model,
        })
    }

    fn block(&self) -> Option<&str> {
        self.managed
            .clone()
            .map(|range| &self.text.as_deref().expect("parsed document")[range])
    }

    fn model(&self) -> Option<&str> {
        self.model
            .clone()
            .map(|range| &self.text.as_deref().expect("parsed document")[range])
    }

    fn set_model(&self, value: &str) -> io::Result<String> {
        if let Some(range) = &self.model {
            let mut text = self.text.clone().ok_or_else(invalid_config)?;
            text.replace_range(range.clone(), &serde_json::to_string(value)?);
            return Ok(text);
        }
        let tree = self.tree()?;
        tree.object_value()
            .ok_or_else(invalid_config)?
            .append("model", CstInputValue::String(value.into()));
        Ok(tree.to_string())
    }

    fn block_value(&self) -> io::Result<Option<Value>> {
        self.block()
            .map(|raw| parse_to_serde_value(raw, &parse_options()).map_err(|_| invalid_config()))
            .transpose()
    }

    fn set(&self, value: &Value) -> io::Result<String> {
        if let Some(range) = &self.managed {
            let mut text = self.text.clone().expect("parsed document");
            text.replace_range(range.clone(), &serde_json::to_string(value)?);
            return Ok(text);
        }
        let tree = self.tree()?;
        let root = tree.object_value().ok_or_else(invalid_config)?;
        if let Some(providers) = root.object_value("provider") {
            providers.append(PROVIDER_ID, cst_value(value));
        } else {
            root.append(
                "provider",
                CstInputValue::Object(vec![(PROVIDER_ID.into(), cst_value(value))]),
            );
        }
        Ok(tree.to_string())
    }

    fn restore(&self, saved: &Wiring) -> io::Result<Option<String>> {
        let text = if let Some(model) = &saved.model {
            if model.owns(self.model()) && self.model() != model.original.as_deref() {
                if let Some(original) = &model.original {
                    let mut text = self.text.clone().ok_or_else(invalid_config)?;
                    text.replace_range(self.model.clone().ok_or_else(invalid_config)?, original);
                    Some(text)
                } else {
                    let tree = self.tree()?;
                    tree.object_value()
                        .ok_or_else(invalid_config)?
                        .get("model")
                        .ok_or_else(invalid_config)?
                        .remove();
                    Some(tree.to_string())
                }
            } else {
                self.text.clone()
            }
        } else {
            self.text.clone()
        };
        let document = Self::parse(text)?;
        if saved.owns(document.block()) {
            document.restore_provider(saved)
        } else {
            Ok(document.text.filter(|text| {
                saved.had_file || text.split_whitespace().collect::<String>() != "{}"
            }))
        }
    }

    fn restore_provider(&self, saved: &Wiring) -> io::Result<Option<String>> {
        if let Some(original) = &saved.original {
            let mut text = self.text.clone().ok_or_else(invalid_config)?;
            let range = self.managed.clone().ok_or_else(invalid_config)?;
            text.replace_range(range, original);
            Self::parse(Some(text.clone()))?;
            return Ok(Some(text));
        }
        let tree = self.tree()?;
        let root = tree.object_value().ok_or_else(invalid_config)?;
        let provider = root.object_value("provider").ok_or_else(invalid_config)?;
        if !saved.had_provider
            && provider.properties().len() == 1
            && !provider.children().iter().any(CstNode::is_comment)
        {
            root.get("provider").ok_or_else(invalid_config)?.remove();
        } else {
            provider
                .get(PROVIDER_ID)
                .ok_or_else(invalid_config)?
                .remove();
        }
        let text = tree.to_string();
        Ok((saved.had_file || text.split_whitespace().collect::<String>() != "{}").then_some(text))
    }

    fn tree(&self) -> io::Result<CstRootNode> {
        CstRootNode::parse(
            self.text.as_deref().unwrap_or(
                "{}
",
            ),
            &parse_options(),
        )
        .map_err(|_| invalid_config())
    }

    fn write_if_current(&self, path: &Path, new: Option<&str>) -> io::Result<()> {
        if Self::read(path)?.text != self.text {
            return Err(io::Error::other("OpenCode 配置已变化，请重新预览"));
        }
        match new {
            Some(text) => atomic_write(path, text.as_bytes()),
            None => remove_if_present(path),
        }
    }
}

fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

fn unique_property<'a, 'text>(
    object: &'a Object<'text>,
    key: &str,
) -> io::Result<Option<&'a ObjectProp<'text>>> {
    let mut properties = object
        .properties
        .iter()
        .filter(|property| property.name.as_str() == key);
    let first = properties.next();
    if properties.next().is_some() {
        return Err(invalid_config());
    }
    Ok(first)
}

fn invalid_config() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "OpenCode 配置必须是有效的 JSON 或 JSONC",
    )
}

fn cst_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(values) => CstInputValue::Array(values.iter().map(cst_value).collect()),
        Value::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), cst_value(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aubridge-opencode-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("opencode.json"), dir.join("state"))
    }

    #[test]
    fn explicit_model_selection_restores_first_default() {
        let (config, state) = tmp("explicit-model");
        let original = r#"{
  // keep this setting
  "model": "foreign/original",
  "small_model": "foreign/small",
  "theme": "dark"
}"#;
        fs::write(&config, original).unwrap();
        let client = OpenCode::new(config.clone(), state, 3427);
        client.apply().unwrap();
        assert_eq!(AgentWiring::current_model(&client), None);
        AgentWiring::set_model(&client, "vendor/first").unwrap();
        assert_eq!(
            AgentWiring::current_model(&client).as_deref(),
            Some("vendor/first")
        );
        AgentWiring::set_model(&client, "aubridge").unwrap();
        assert_eq!(
            AgentWiring::current_model(&client).as_deref(),
            Some("aubridge")
        );
        assert_eq!(
            parse_to_serde_value::<Value>(&fs::read_to_string(&config).unwrap(), &parse_options())
                .unwrap()["model"],
            "aubridge/aubridge"
        );
        let preview = client.disconnect_preview().unwrap();
        assert!(preview.changes.iter().any(|change| change.field == "model"));
        client.disconnect_reviewed(Some(&preview)).unwrap();
        let restored = fs::read_to_string(&config).unwrap();
        assert!(restored.contains("// keep this setting"));
        let value: Value = parse_to_serde_value(&restored, &parse_options()).unwrap();
        assert_eq!(value["model"], "foreign/original");
        assert_eq!(value["small_model"], "foreign/small");
        assert_eq!(value["theme"], "dark");
    }

    #[test]
    fn model_selection_requires_wiring_and_restores_absence_after_sync() {
        let (config, state) = tmp("model-absent");
        let client = OpenCode::new(config.clone(), state.clone(), 3427);
        assert!(client.set_model("m").is_err());
        assert!(!config.exists());
        client.apply().unwrap();
        client.set_model("vendor/m").unwrap();
        let updated = OpenCode::new(config.clone(), state, 6543)
            .with_models(vec![("vendor/m".into(), "M".into())]);
        updated.repair().unwrap();
        assert_eq!(updated.current_model().as_deref(), Some("vendor/m"));
        let before = fs::read(&config).unwrap();
        let stash = fs::read(updated.stash_path()).unwrap();
        let preview = updated.disconnect_preview().unwrap();
        assert!(!preview.blocked);
        assert_eq!(fs::read(&config).unwrap(), before);
        assert_eq!(fs::read(updated.stash_path()).unwrap(), stash);
        updated.disconnect_reviewed(Some(&preview)).unwrap();
        assert!(!config.exists());
    }

    #[test]
    fn external_model_change_blocks_selection_sync_and_entire_disconnect() {
        let (config, state) = tmp("model-drift");
        fs::write(&config, r#"{"model":"foreign/original","theme":"dark"}"#).unwrap();
        let client = OpenCode::new(config.clone(), state, 3427);
        client.apply().unwrap();
        client.set_model("m").unwrap();
        let preview = client.disconnect_preview().unwrap();
        let before = fs::read_to_string(&config).unwrap();
        // Returning to the original value externally is still a user edit.
        let modified = before.replace("aubridge/m", "foreign/original");
        fs::write(&config, &modified).unwrap();
        let stash = fs::read(client.stash_path()).unwrap();
        assert_eq!(client.current_model(), None);
        assert!(client.set_model("next").is_err());
        assert!(client.apply().is_err());
        assert!(client.disconnect_preview().unwrap().blocked);
        assert!(client.disconnect_reviewed(Some(&preview)).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), modified);
        assert_eq!(fs::read(client.stash_path()).unwrap(), stash);
    }

    #[test]
    fn interrupted_model_write_and_legacy_stash_preserve_first_original() {
        for legacy in [false, true] {
            let (config, state) = tmp(if legacy {
                "model-legacy"
            } else {
                "model-interrupted"
            });
            fs::write(&config, r#"{"model":"foreign/original"}"#).unwrap();
            let client = OpenCode::new(config.clone(), state, 3427);
            client.apply().unwrap();
            if legacy {
                let old = json!({(CONFIG_BLOCK): null});
                atomic_write_json(&client.stash_path(), &old).unwrap();
            } else {
                let mut old = serde_json::to_value(client.wiring().unwrap().unwrap()).unwrap();
                old.as_object_mut().unwrap().remove("model");
                atomic_write_json(&client.stash_path(), &old).unwrap();
            }
            assert_eq!(client.current_model(), None);
            let mut saved = client.wiring().unwrap().unwrap();
            saved.model = Some(ModelWiring {
                original: Some(r#""foreign/original""#.into()),
                managed: r#""aubridge/m""#.into(),
                previous: None,
                pending: true,
            });
            atomic_write_json(&client.stash_path(), &serde_json::to_value(&saved).unwrap())
                .unwrap();
            client.set_model("m").unwrap();
            let previous = Document::read(&config).unwrap().model().unwrap().to_owned();
            let mut saved = client.wiring().unwrap().unwrap();
            let model = saved.model.as_mut().unwrap();
            model.previous = Some(previous);
            model.managed = r#""aubridge/next""#.into();
            model.pending = true;
            atomic_write_json(&client.stash_path(), &serde_json::to_value(&saved).unwrap())
                .unwrap();
            assert_eq!(client.current_model().as_deref(), Some("m"));
            let before_retry = fs::read_to_string(&config).unwrap();
            fs::write(
                &config,
                before_retry.replace("aubridge/m", "foreign/original"),
            )
            .unwrap();
            assert!(client.set_model("next").is_err());
            assert!(client.disconnect_preview().unwrap().blocked);
            fs::write(&config, before_retry).unwrap();
            client.set_model("next").unwrap();
            let completed_stash = fs::read(client.stash_path()).unwrap();
            client.disconnect().unwrap();
            assert_eq!(
                parse_to_serde_value::<Value>(
                    &fs::read_to_string(&config).unwrap(),
                    &parse_options()
                )
                .unwrap()["model"],
                "foreign/original"
            );
            // Simulate interruption after atomic restore but before stash removal.
            atomic_write(&client.stash_path(), &completed_stash).unwrap();
            assert!(!client.disconnect_preview().unwrap().blocked);
            client.disconnect().unwrap();
            assert!(!client.stash_path().exists());
        }
    }

    #[test]
    fn apply_writes_provider_block_preserving_user_config() {
        let (config, state) = tmp("apply");
        std::fs::write(&config, r#"{"theme":"dark","provider":{"custom":{}}}"#).unwrap();
        OpenCode::new(config.clone(), state, 3427)
            .with_models(vec![("glm-4.5-flash".into(), "GLM 4.5 Flash".into())])
            .apply()
            .unwrap();
        let v: Value = crate::read_json(&config).unwrap();
        assert_eq!(v["theme"], "dark");
        assert!(v["provider"]["custom"].is_object());
        assert_eq!(
            v.pointer("/provider/aubridge/options/baseURL").unwrap(),
            "http://127.0.0.1:3427/v1"
        );
        assert_eq!(
            v.pointer("/provider/aubridge/models/glm-4.5-flash/name")
                .unwrap(),
            "GLM 4.5 Flash"
        );
    }

    #[test]
    fn disconnect_restores_or_removes_block() {
        let (config, state) = tmp("disconnect");
        std::fs::write(&config, r#"{"provider":{"custom":{}}}"#).unwrap();
        let oc =
            OpenCode::new(config.clone(), state, 3427).with_models(vec![("m".into(), "M".into())]);
        oc.apply().unwrap();
        assert!(oc.wired());
        oc.disconnect().unwrap();
        assert!(!oc.wired());
        let v: Value = crate::read_json(&config).unwrap();
        assert!(v["provider"]["custom"].is_object());
        assert!(v.pointer("/provider/aubridge").is_none());
    }

    #[test]
    fn jsonc_repair_and_disconnect_preserve_first_block_and_later_settings() {
        let (config, state) = tmp("jsonc-repair");
        let config = config.with_extension("jsonc");
        let original = r#"{
  // user settings
  "autoupdate": false,
  "provider": {
    "aubridge": {
      // retain this comment in the original provider
      "name": "original",
    },
    "private": {},
  },
}
"#;
        fs::write(&config, original).unwrap();
        let first = OpenCode::new(config.clone(), state.clone(), 3427)
            .with_models(vec![("old".into(), "Old".into())]);
        first.apply().unwrap();
        let saved = first.wiring().unwrap().unwrap().original;
        let updated = fs::read_to_string(&config)
            .unwrap()
            .replace("\"autoupdate\": false", "\"autoupdate\": true");
        fs::write(&config, updated).unwrap();
        let second = OpenCode::new(config.clone(), state, 6543)
            .with_models(vec![("new".into(), "New".into())]);
        assert_eq!(second.drift(), Drift::PortChanged);
        second.repair().unwrap();
        assert!(second.wired());
        assert_eq!(second.wiring().unwrap().unwrap().original, saved);
        let managed = Document::read(&config)
            .unwrap()
            .block_value()
            .unwrap()
            .unwrap();
        assert_eq!(managed["models"]["new"]["name"], "New");
        let preview = second.disconnect_preview().unwrap();
        assert_eq!(preview.changes[0].action, RestoreAction::Restore);
        second.disconnect_reviewed(Some(&preview)).unwrap();
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            original.replace("\"autoupdate\": false", "\"autoupdate\": true")
        );
    }

    #[test]
    fn invalid_jsonc_is_rejected_without_replacing_it() {
        let (config, state) = tmp("invalid-config");
        let original = r#"{"autoupdate": false "provider": {}}"#;
        fs::write(&config, original).unwrap();
        let client = OpenCode::new(config.clone(), state, 3427);
        assert!(client.apply().is_err());
        assert!(!client.stash_path().exists());
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
    }

    #[test]
    fn a_new_comment_in_the_managed_block_invalidates_a_disconnect_preview() {
        let (config, state) = tmp("preview-conflict");
        let client = OpenCode::new(config.clone(), state, 3427);
        client.apply().unwrap();
        let preview = client.disconnect_preview().unwrap();
        let modified = fs::read_to_string(&config)
            .unwrap()
            .replace("\"aubridge\": {", "\"aubridge\": { /* keep me */");
        fs::write(&config, &modified).unwrap();
        assert!(client.disconnect_preview().unwrap().blocked);
        assert!(client.disconnect_reviewed(Some(&preview)).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), modified);
    }

    #[test]
    fn global_jsonc_takes_precedence_and_new_config_is_removed_on_disconnect() {
        let (config, state) = tmp("config-path");
        let directory = config.parent().unwrap();
        let fallback = directory.join("config.json");
        fs::write(&fallback, "{}").unwrap();
        assert_eq!(config_path(directory), fallback);
        fs::write(&config, "{}").unwrap();
        assert_eq!(config_path(directory), config);
        let jsonc = config.with_extension("jsonc");
        fs::write(&jsonc, "{ /* current */ }").unwrap();
        assert_eq!(config_path(directory), jsonc);
        let new = directory.join("new.json");
        let client = OpenCode::new(new.clone(), state, 3427);
        client.apply().unwrap();
        client.disconnect().unwrap();
        assert!(!new.exists());
    }
}

//! Reversible Gemini CLI settings and dotenv wiring (decision 50).
use crate::{
    atomic_write, atomic_write_json, AgentWiring, DisconnectChange, DisconnectPreview, Drift,
    RestoreAction,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};

const AUTO: &str = "aubridge";
const TOKEN: &str = "aubridge-gemini";
const AUTH: &str = "gemini-api-key";
const ENV_KEYS: [&str; 2] = ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"];
const PATHS: [&[&str]; 2] = [&["security", "auth", "selectedType"], &["model", "name"]];

const AUTO_ALIAS_PATH: &[&str] = &["modelConfigs", "customAliases", "aubridge"];
const SDK_ALIAS_PATH: &[&str] = &["modelConfigs", "aliases", "aubridge"];
fn default_alias_name() -> String {
    AUTO.into()
}
fn alias_path(name: &str) -> [&str; 3] {
    ["modelConfigs", "customAliases", name]
}
fn alias_value(name: &str) -> Value {
    json!({"modelConfig":{"model":name}})
}
fn auto_alias_value() -> Value {
    json!({"modelConfig":{"model":AUTO}})
}

pub struct GeminiCli {
    pub settings: PathBuf,
    pub env_file: PathBuf,
    pub state_dir: PathBuf,
    pub port: u16,
}

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    settings: [Value; 2],
    env: [Vec<String>; 2],
    #[serde(default)]
    alias: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
struct AutoAlias {
    #[serde(default = "default_alias_name")]
    name: String,
    had_model_configs: bool,
    had_custom_aliases: bool,
}
#[derive(Serialize, Deserialize)]
struct Stash {
    version: u8,
    settings_path: PathBuf,
    env_path: PathBuf,
    original_settings: Option<String>,
    original_env: Option<String>,
    managed: Snapshot,
    previous: Option<Snapshot>,
    pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_alias: Option<AutoAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_alias: Option<AutoAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_alias: Option<AutoAlias>,
}
struct Document {
    settings_text: Option<String>,
    settings: Value,
    env_text: Option<String>,
}
impl Document {
    fn read(settings: &Path, env: &Path) -> io::Result<Self> {
        let settings_text = optional_text(settings)?;
        let settings = match &settings_text {
            Some(text) => serde_json::from_str::<Value>(text).map_err(io::Error::other)?,
            None => json!({}),
        };
        if !settings.is_object() {
            return Err(io::Error::other("Gemini settings 必须是 JSON 对象"));
        }
        for path in PATHS.into_iter().chain([AUTO_ALIAS_PATH, SDK_ALIAS_PATH]) {
            let mut value = &settings;
            for key in &path[..path.len() - 1] {
                match value.get(*key) {
                    None => break,
                    Some(next) if next.is_object() => value = next,
                    Some(_) => return Err(io::Error::other("Gemini 受管字段父级必须是对象")),
                }
            }
        }
        Ok(Self {
            settings_text,
            settings,
            env_text: optional_text(env)?,
        })
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            settings: PATHS.map(|path| get_json(&self.settings, path)),
            env: ENV_KEYS.map(|key| env_lines(self.env_text.as_deref().unwrap_or(""), key)),
            alias: find_json(&self.settings, AUTO_ALIAS_PATH).cloned(),
        }
    }
}
impl GeminiCli {
    pub fn new(settings: PathBuf, env_file: PathBuf, state_dir: PathBuf, port: u16) -> Self {
        Self {
            settings,
            env_file,
            state_dir,
            port,
        }
    }
    fn stash_path(&self) -> PathBuf {
        self.state_dir.join("gemini.stash.json")
    }
    fn legacy(&self) -> io::Result<Option<Value>> {
        crate::protect_wiring_backup(&self.state_dir, "gemini.stash.json")?;
        let Some(text) = optional_text(&self.stash_path())? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
        Ok(value.get("version").is_none().then_some(value))
    }
    fn load(&self) -> io::Result<Option<Stash>> {
        crate::protect_wiring_backup(&self.state_dir, "gemini.stash.json")?;
        let mut saved: Option<Stash> = optional_text(&self.stash_path())?
            .map(|s| serde_json::from_str(&s).map_err(io::Error::other))
            .transpose()?;
        if let Some(saved) = &mut saved {
            if let Some(mut legacy) = saved.auto_alias.take() {
                legacy.name = AUTO.into();
                if saved.selected_alias.is_some() {
                    return Err(io::Error::other("Gemini 接线备份含冲突的别名所有权"));
                }
                if saved.pending
                    && saved.previous.as_ref().is_some_and(|previous| {
                        previous.alias.as_ref() == Some(&auto_alias_value())
                    })
                {
                    saved.previous_alias = Some(legacy.clone());
                }
                saved.selected_alias = Some(legacy);
            }
        }
        Ok(saved)
    }

    fn original(saved: &Stash) -> io::Result<Snapshot> {
        let settings = saved
            .original_settings
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_else(|| json!({}));
        Ok(Snapshot {
            settings: PATHS.map(|path| get_json(&settings, path)),
            env: ENV_KEYS.map(|key| env_lines(saved.original_env.as_deref().unwrap_or(""), key)),
            alias: find_json(&settings, AUTO_ALIAS_PATH).cloned(),
        })
    }
    fn accepts(saved: &Stash, current: &Snapshot, settings: &Value) -> io::Result<bool> {
        let original = Self::original(saved)?;
        Ok((0..2).all(|i| {
            let setting = &current.settings[i];
            let env = &current.env[i];
            (setting == &saved.managed.settings[i]
                || saved
                    .previous
                    .as_ref()
                    .is_some_and(|p| setting == &p.settings[i])
                || saved.pending && setting == &original.settings[i])
                && (env == &saved.managed.env[i]
                    || saved.previous.as_ref().is_some_and(|p| env == &p.env[i])
                    || saved.pending && env == &original.env[i])
        }) && if saved.pending {
            Self::after_alias_state(saved, current, settings)
                || current.settings[1] == saved.previous.as_ref().unwrap_or(&original).settings[1]
                    && alias_state(
                        settings,
                        saved.previous_alias.as_ref(),
                        saved.selected_alias.as_ref(),
                    )
        } else {
            alias_state(settings, saved.selected_alias.as_ref(), None)
        })
    }
    fn after_alias_state(saved: &Stash, current: &Snapshot, settings: &Value) -> bool {
        current.settings[1] == saved.managed.settings[1]
            && alias_state(
                settings,
                saved.selected_alias.as_ref(),
                saved.previous_alias.as_ref(),
            )
    }
    fn current_owner(saved: &Stash, current: &Snapshot, settings: &Value) -> Option<AutoAlias> {
        if !saved.pending || Self::after_alias_state(saved, current, settings) {
            saved.selected_alias.clone()
        } else {
            saved.previous_alias.clone()
        }
    }

    fn validate_paths(&self, saved: &Stash) -> io::Result<()> {
        if saved.settings_path != std::path::absolute(&self.settings)?
            || saved.env_path != std::path::absolute(&self.env_file)?
        {
            return Err(io::Error::other("Gemini 配置路径已变化，请先断开原接线"));
        }
        Ok(())
    }
    pub fn apply(&self) -> io::Result<()> {
        self.apply_model(None)
    }
    fn apply_model(&self, model: Option<&str>) -> io::Result<()> {
        let mut document = Document::read(&self.settings, &self.env_file)?;
        let current = document.snapshot();
        let existing = self.load()?;
        if let Some(saved) = &existing {
            self.validate_paths(saved)?;
            if !Self::accepts(saved, &current, &document.settings)? {
                return Err(io::Error::other("Gemini 受管字段已被修改，请先核对配置"));
            }
        }
        let model = model
            .map(str::to_owned)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|s| s.managed.settings[1].as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| AUTO.into());
        let previous_alias = existing
            .as_ref()
            .and_then(|saved| Self::current_owner(saved, &current, &document.settings));
        if let Some(alias) = &previous_alias {
            remove_alias(&mut document.settings, alias);
        }
        let path = alias_path(&model);
        let selected_alias = match find_json(&document.settings, &path)
            .or_else(|| find_json(&document.settings, &["modelConfigs", "aliases", &model]))
        {
            None => {
                let alias = AutoAlias {
                    name: model.clone(),
                    had_model_configs: document.settings.get("modelConfigs").is_some(),
                    had_custom_aliases: find_json(
                        &document.settings,
                        &["modelConfigs", "customAliases"],
                    )
                    .is_some(),
                };
                set_path(&mut document.settings, &path, alias_value(&model));
                Some(alias)
            }
            Some(alias)
                if alias
                    .get("modelConfig")
                    .and_then(|config| config.get("model"))
                    .and_then(Value::as_str)
                    == Some(model.as_str()) =>
            {
                None
            }
            Some(_) => {
                return Err(io::Error::other(format!(
                    "Gemini 的 {model} 模型别名已由用户配置，无法安全选用；请先核对该别名"
                )))
            }
        };
        set_path(&mut document.settings, PATHS[0], json!(AUTH));
        set_path(&mut document.settings, PATHS[1], json!(model));
        let next_env = merge_env(
            document.env_text.as_deref().unwrap_or(""),
            &[
                vec![format!("{}=http://127.0.0.1:{}\n", ENV_KEYS[0], self.port)],
                vec![format!("{}={TOKEN}\n", ENV_KEYS[1])],
            ],
        );
        let managed = Snapshot {
            settings: PATHS.map(|p| get_json(&document.settings, p)),
            env: ENV_KEYS.map(|key| env_lines(&next_env, key)),
            alias: find_json(&document.settings, AUTO_ALIAS_PATH).cloned(),
        };
        let mut saved = existing.unwrap_or(Stash {
            version: 2,
            settings_path: std::path::absolute(&self.settings)?,
            env_path: std::path::absolute(&self.env_file)?,
            original_settings: document.settings_text.clone(),
            original_env: document.env_text.clone(),
            managed: current.clone(),
            previous: None,
            pending: true,
            auto_alias: None,
            selected_alias: None,
            previous_alias: None,
        });
        saved.selected_alias = selected_alias;
        saved.previous_alias = previous_alias;
        saved.previous = Some(current);
        saved.managed = managed;
        saved.pending = true;
        atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        atomic_write_json(&self.settings, &document.settings)?;
        atomic_write(&self.env_file, next_env.as_bytes())?;
        saved.previous = None;
        saved.previous_alias = None;
        saved.pending = false;
        atomic_write_json(&self.stash_path(), &serde_json::to_value(saved)?)
    }
    pub fn current_model(&self) -> Option<String> {
        Document::read(&self.settings, &self.env_file)
            .ok()
            .and_then(|d| get_json(&d.settings, PATHS[1]).as_str().map(str::to_owned))
    }
    pub fn set_model(&self, model: &str) -> io::Result<()> {
        if !self.wired() {
            return Err(io::Error::other("Gemini 尚未接线或受管字段已变化"));
        }
        self.apply_model(Some(model))
    }
    pub fn wired(&self) -> bool {
        let Ok(Some(saved)) = self.load() else {
            return false;
        };
        let Ok(document) = Document::read(&self.settings, &self.env_file) else {
            return false;
        };
        let current = document.snapshot();
        self.validate_paths(&saved).is_ok()
            && Self::accepts(&saved, &current, &document.settings).unwrap_or(false)
            && current.settings[0] == AUTH
            && current.settings[1].as_str().is_some_and(|s| !s.is_empty())
            && env_value(&current.env[0]) == Some(format!("http://127.0.0.1:{}", self.port))
            && env_value(&current.env[1]).as_deref() == Some(TOKEN)
    }
    pub fn drift(&self) -> Drift {
        match self.load() {
            Ok(None) => Drift::Unwired,
            Ok(Some(saved)) => {
                let valid = self.validate_paths(&saved).is_ok()
                    && Document::read(&self.settings, &self.env_file)
                        .ok()
                        .is_some_and(|d| {
                            Self::accepts(&saved, &d.snapshot(), &d.settings).unwrap_or(false)
                        });
                if !valid {
                    Drift::Replaced
                } else if self.wired() {
                    Drift::Clean
                } else {
                    Drift::PortChanged
                }
            }
            Err(_) => Drift::Replaced,
        }
    }
    pub fn repair_port(&self) -> io::Result<()> {
        if self.drift() == Drift::PortChanged {
            self.apply()?;
        }
        Ok(())
    }
    pub fn disconnect_preview(&self) -> io::Result<DisconnectPreview> {
        if let Some(saved) = self.legacy()? {
            let document = Document::read(&self.settings, &self.env_file)?;
            let current = document.snapshot();
            let blocked = current.settings[0] != AUTH
                || env_value(&current.env[0]) != Some(format!("http://127.0.0.1:{}", self.port))
                || env_value(&current.env[1]).as_deref() != Some(TOKEN);
            return Ok(DisconnectPreview {
                changes: ["selectedType", ENV_KEYS[0], ENV_KEYS[1]]
                    .iter()
                    .map(|key| DisconnectChange {
                        field: (*key).into(),
                        action: if saved.get(*key).is_none_or(Value::is_null) {
                            RestoreAction::Remove
                        } else {
                            RestoreAction::Restore
                        },
                    })
                    .collect(),
                blocked,
            });
        }
        let Some(saved) = self.load()? else {
            return Ok(DisconnectPreview {
                changes: vec![],
                blocked: false,
            });
        };
        self.validate_paths(&saved)?;
        let document = Document::read(&self.settings, &self.env_file)?;
        let current = document.snapshot();
        let original = Self::original(&saved)?;
        let original_settings = saved
            .original_settings
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_else(|| json!({}));
        let mut fields = [
            "security.auth.selectedType",
            "model.name",
            ENV_KEYS[0],
            ENV_KEYS[1],
        ]
        .map(str::to_owned)
        .to_vec();
        if let Some(alias) = Self::current_owner(&saved, &current, &document.settings) {
            fields.push(format!("modelConfigs.customAliases[{}]", json!(alias.name)));
        }
        Ok(DisconnectPreview {
            changes: fields
                .iter()
                .enumerate()
                .map(|(i, field)| DisconnectChange {
                    field: field.clone(),
                    action: if i == 4
                        || i < 2 && find_json(&original_settings, PATHS[i]).is_none()
                        || i >= 2 && original.env[i - 2].is_empty()
                    {
                        RestoreAction::Remove
                    } else {
                        RestoreAction::Restore
                    },
                })
                .collect(),
            blocked: !Self::accepts(&saved, &current, &document.settings)?,
        })
    }
    pub fn disconnect(&self) -> io::Result<()> {
        self.disconnect_reviewed(None)
    }
    pub fn disconnect_reviewed(&self, expected: Option<&DisconnectPreview>) -> io::Result<()> {
        let preview = self.disconnect_preview()?;
        if preview.blocked || expected.is_some_and(|p| p != &preview) {
            return Err(io::Error::other("Gemini 配置已变化，请重新预览"));
        }
        if let Some(saved) = self.legacy()? {
            let mut document = Document::read(&self.settings, &self.env_file)?;
            match saved.get("selectedType").filter(|v| !v.is_null()) {
                Some(value) => set_path(&mut document.settings, PATHS[0], value.clone()),
                None => del_path(&mut document.settings, PATHS[0]),
            }
            let replacements = ENV_KEYS.map(|key| {
                saved
                    .get(key)
                    .and_then(Value::as_str)
                    .map(|value| vec![format!("{key}={value}\n")])
                    .unwrap_or_default()
            });
            atomic_write_json(&self.settings, &document.settings)?;
            atomic_write(
                &self.env_file,
                merge_env(document.env_text.as_deref().unwrap_or(""), &replacements).as_bytes(),
            )?;
            return std::fs::remove_file(self.stash_path());
        }
        let Some(mut saved) = self.load()? else {
            return Ok(());
        };
        let mut document = Document::read(&self.settings, &self.env_file)?;
        let original = Self::original(&saved)?;
        let current = document.snapshot();
        if !Self::accepts(&saved, &current, &document.settings)? {
            return Err(io::Error::other("Gemini 配置已变化，请重新预览"));
        }
        let current_owner = Self::current_owner(&saved, &current, &document.settings);
        // Before/after alias states remain recoverable if either file write is interrupted.
        saved.selected_alias = current_owner.clone();
        saved.previous_alias = None;
        saved.managed = current;
        saved.previous = Some(original.clone());
        saved.pending = true;
        atomic_write_json(&self.stash_path(), &serde_json::to_value(&saved)?)?;
        let original_json = saved
            .original_settings
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_else(|| json!({}));
        for path in PATHS {
            match find_json(&original_json, path) {
                Some(value) => set_path(&mut document.settings, path, value.clone()),
                None => {
                    del_path(&mut document.settings, path);
                    for length in 1..path.len() {
                        let parent = &path[..length];
                        if find_json(&document.settings, parent).is_none() {
                            if let Some(value) = find_json(&original_json, parent) {
                                set_path(&mut document.settings, parent, value.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Some(alias) = &current_owner {
            remove_alias(&mut document.settings, alias);
        }
        if document.settings == original_json {
            restore_file(&self.settings, saved.original_settings.as_deref())?;
        } else {
            atomic_write_json(&self.settings, &document.settings)?;
        }
        let env = merge_env(document.env_text.as_deref().unwrap_or(""), &original.env);
        // Restore exact original ordering and final-newline bytes if the only edits were ours.
        let original_text = saved.original_env.as_deref().unwrap_or("");
        let applied_original = merge_env(original_text, &saved.managed.env);
        if document.env_text.as_deref() == Some(applied_original.as_str())
            || document.env_text == saved.original_env
        {
            restore_file(&self.env_file, saved.original_env.as_deref())?;
        } else {
            atomic_write(&self.env_file, env.as_bytes())?;
        }
        std::fs::remove_file(self.stash_path())
    }
}
impl AgentWiring for GeminiCli {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn name(&self) -> &'static str {
        "Gemini CLI"
    }
    fn apply(&self) -> io::Result<()> {
        GeminiCli::apply(self)
    }
    fn disconnect(&self) -> io::Result<()> {
        GeminiCli::disconnect(self)
    }
    fn wired(&self) -> bool {
        GeminiCli::wired(self)
    }
    fn detected(&self) -> bool {
        self.settings.exists() || self.env_file.exists()
    }
    fn drift(&self) -> Option<Drift> {
        Some(GeminiCli::drift(self))
    }
    fn current_model(&self) -> Option<String> {
        GeminiCli::current_model(self)
    }
    fn set_model(&self, model: &str) -> io::Result<()> {
        GeminiCli::set_model(self, model)
    }
    fn hint(&self) -> Option<&'static str> {
        Some("Gemini CLI 在启动时读取配置；请重启已打开的 Gemini 会话。")
    }
}
fn alias_state(settings: &Value, present: Option<&AutoAlias>, absent: Option<&AutoAlias>) -> bool {
    present.is_none_or(|alias| {
        find_json(settings, &alias_path(&alias.name)) == Some(&alias_value(&alias.name))
    }) && absent.is_none_or(|alias| {
        present.is_some_and(|present| present.name == alias.name)
            || find_json(settings, &alias_path(&alias.name)).is_none()
    })
}
fn remove_alias(settings: &mut Value, alias: &AutoAlias) {
    del_path(settings, &alias_path(&alias.name));
    if alias.had_model_configs && settings.get("modelConfigs").is_none() {
        set_path(settings, &["modelConfigs"], json!({}));
    }
    if alias.had_custom_aliases && find_json(settings, &["modelConfigs", "customAliases"]).is_none()
    {
        set_path(settings, &["modelConfigs", "customAliases"], json!({}));
    }
}
fn optional_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn restore_file(path: &Path, text: Option<&str>) -> io::Result<()> {
    match text {
        Some(text) => atomic_write(path, text.as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}
fn find_json<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}
fn get_json(v: &Value, path: &[&str]) -> Value {
    path.iter()
        .fold(v, |v, key| v.get(*key).unwrap_or(&Value::Null))
        .clone()
}
fn set_path(v: &mut Value, path: &[&str], value: Value) {
    if path.len() == 1 {
        v[path[0]] = value;
    } else {
        if v.get(path[0]).is_none() {
            v[path[0]] = json!({});
        }
        set_path(&mut v[path[0]], &path[1..], value);
    }
}
fn del_path(v: &mut Value, path: &[&str]) {
    if path.len() == 1 {
        if let Some(o) = v.as_object_mut() {
            o.remove(path[0]);
        }
    } else if let Some(child) = v.get_mut(path[0]) {
        del_path(child, &path[1..]);
        if child.as_object().is_some_and(|o| o.is_empty()) {
            v.as_object_mut().unwrap().remove(path[0]);
        }
    }
}
fn env_key(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, _) = line.split_once('=')?;
    let key = key.trim();
    ENV_KEYS.contains(&key).then_some(key)
}
fn env_lines(text: &str, key: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .filter(|line| env_key(line) == Some(key))
        .map(str::to_owned)
        .collect()
}
fn env_value(lines: &[String]) -> Option<String> {
    lines
        .last()?
        .split_once('=')
        .map(|(_, v)| v.trim().trim_matches(['\'', '"']).to_string())
}
fn merge_env(text: &str, replacements: &[Vec<String>; 2]) -> String {
    let mut output = String::new();
    let mut seen = [false; 2];
    for line in text.split_inclusive('\n') {
        if let Some(i) = env_key(line).and_then(|key| ENV_KEYS.iter().position(|k| *k == key)) {
            if !seen[i] {
                for replacement in &replacements[i] {
                    output.push_str(replacement);
                }
                seen[i] = true;
            }
        } else {
            output.push_str(line);
        }
    }
    for i in 0..2 {
        if !seen[i] && !replacements[i].is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            for line in &replacements[i] {
                output.push_str(line);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_read_env(path: &Path) -> std::collections::BTreeMap<String, String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| {
                line.split_once('=')
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
            })
            .collect()
    }
    fn tmp(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aubridge-gemini-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            dir.join("settings.json"),
            dir.join(".env"),
            dir.join("state"),
        )
    }

    #[test]
    fn apply_sets_env_and_selected_type() {
        let (settings, env_file, state) = tmp("apply");
        std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
        std::fs::write(&env_file, "CUSTOM=1\n").unwrap();

        GeminiCli::new(settings.clone(), env_file.clone(), state, 3427)
            .apply()
            .unwrap();
        let env = test_read_env(&env_file);
        assert_eq!(
            env.get("GOOGLE_GEMINI_BASE_URL").unwrap(),
            "http://127.0.0.1:3427"
        );
        assert_eq!(env.get("GEMINI_API_KEY").unwrap(), "aubridge-gemini");
        assert_eq!(env.get("CUSTOM").unwrap(), "1");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
    }

    #[test]
    fn disconnect_restores_original() {
        let (settings, env_file, state) = tmp("disconnect");
        std::fs::write(
            &settings,
            r#"{"security":{"auth":{"selectedType":"oauth"}}}"#,
        )
        .unwrap();
        std::fs::write(&env_file, "GOOGLE_GEMINI_BASE_URL=https://orig\n").unwrap();

        let g = GeminiCli::new(settings.clone(), env_file.clone(), state, 3427);
        g.apply().unwrap();
        assert!(g.wired());
        g.disconnect().unwrap();
        assert!(!g.wired());

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["security"]["auth"]["selectedType"], "oauth");
        assert_eq!(
            test_read_env(&env_file)
                .get("GOOGLE_GEMINI_BASE_URL")
                .unwrap(),
            "https://orig"
        );
    }
    #[test]
    fn malformed_settings_never_creates_backup_or_modifies_env() {
        let (settings, env, state) = tmp("malformed");
        std::fs::write(&settings, "{broken").unwrap();
        std::fs::write(&env, "# keep\nCUSTOM=x\n").unwrap();
        let g = GeminiCli::new(settings.clone(), env.clone(), state.clone(), 3427);
        assert!(g.apply().is_err());
        assert!(!state.join("gemini.stash.json").exists());
        assert_eq!(std::fs::read_to_string(env).unwrap(), "# keep\nCUSTOM=x\n");
    }

    #[test]
    fn env_round_trip_preserves_comments_duplicates_and_bytes() {
        let (settings, env, state) = tmp("raw-env");
        let original = "# credentials\r\nCUSTOM='a=b' # keep\r\nexport GEMINI_API_KEY=old # original\r\nGEMINI_API_KEY=second\r\n";
        std::fs::write(&env, original).unwrap();
        let g = GeminiCli::new(settings, env.clone(), state, 3427);
        g.apply().unwrap();
        assert!(std::fs::read_to_string(&env)
            .unwrap()
            .contains("CUSTOM='a=b' # keep\r\n"));
        g.apply().unwrap();
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(env).unwrap(), original);
    }

    #[test]
    fn foreign_host_with_same_port_is_not_wired() {
        let (settings, env, state) = tmp("foreign");
        std::fs::write(&env, "GOOGLE_GEMINI_BASE_URL=https://evil.invalid:3427\n").unwrap();
        assert!(!GeminiCli::new(settings, env, state, 3427).wired());
    }
    #[test]
    fn model_port_and_drift_keep_first_backup() {
        let (settings, env, state) = tmp("model-drift");
        std::fs::write(&settings, r#"{"model":{"name":"original"},"security":{"auth":{"selectedType":"oauth-personal"}},"theme":"dark"}"#).unwrap();
        let g = GeminiCli::new(settings.clone(), env.clone(), state.clone(), 3427);
        g.apply().unwrap();
        assert_eq!(g.current_model().as_deref(), Some("aubridge"));
        g.set_model("provider/model:free").unwrap();
        let moved = GeminiCli::new(settings.clone(), env.clone(), state, 3428);
        assert_eq!(moved.drift(), Drift::PortChanged);
        moved.repair_port().unwrap();
        assert!(moved.wired());
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        value["theme"] = json!("light");
        atomic_write_json(&settings, &value).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&env)
            .unwrap()
            .write_all(b"# added\nCUSTOM=2\n")
            .unwrap();
        moved.disconnect().unwrap();
        let restored: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored["model"]["name"], "original");
        assert_eq!(
            restored["security"]["auth"]["selectedType"],
            "oauth-personal"
        );
        assert_eq!(restored["theme"], "light");
        assert_eq!(std::fs::read_to_string(env).unwrap(), "# added\nCUSTOM=2\n");
    }

    #[test]
    fn replaced_field_blocks_all_writes_and_review() {
        let (settings, env, state) = tmp("replaced");
        let g = GeminiCli::new(settings.clone(), env.clone(), state, 3427);
        g.apply().unwrap();
        std::fs::write(
            &env,
            "GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3427\nGEMINI_API_KEY=third-party\n",
        )
        .unwrap();
        let before = std::fs::read(&env).unwrap();
        assert_eq!(g.drift(), Drift::Replaced);
        assert!(!g.wired());
        assert!(g.disconnect_preview().unwrap().blocked);
        assert!(g.apply().is_err());
        assert!(g.set_model("new").is_err());
        assert!(g.disconnect().is_err());
        assert_eq!(std::fs::read(&env).unwrap(), before);
    }

    #[test]
    fn legacy_backup_does_not_remove_user_model() {
        let (settings, env, state) = tmp("legacy");
        std::fs::create_dir_all(&state).unwrap();
        atomic_write_json(
            &state.join("gemini.stash.json"),
            &json!({"selectedType":"oauth", "GEMINI_API_KEY":null, "GOOGLE_GEMINI_BASE_URL":null}),
        )
        .unwrap();
        atomic_write_json(&settings,&json!({"security":{"auth":{"selectedType":"gemini-api-key"}}, "model":{"name":"user-choice"}})).unwrap();
        std::fs::write(
            &env,
            "GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3427\nGEMINI_API_KEY=aubridge-gemini\n",
        )
        .unwrap();
        GeminiCli::new(settings.clone(), env, state, 3427)
            .disconnect()
            .unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(value["model"]["name"], "user-choice");
        assert_eq!(value["security"]["auth"]["selectedType"], "oauth");
    }
    #[test]
    fn restores_explicit_null_and_empty_parent_settings() {
        let (settings, env, state) = tmp("null-empty");
        let original = r#"{"security":{"auth":{}},"model":{"name":null}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn interrupted_apply_and_disconnect_recover_original_backup() {
        let (settings, env, state) = tmp("interrupted");
        let g = GeminiCli::new(settings.clone(), env.clone(), state, 3427);
        std::fs::write(&env, "# original\nCUSTOM=x\n").unwrap();
        g.apply().unwrap();
        let mut saved = g.load().unwrap().unwrap();
        saved.pending = true;
        atomic_write_json(&g.stash_path(), &serde_json::to_value(&saved).unwrap()).unwrap();
        // Simulate settings restored before an interrupted env write.
        std::fs::remove_file(&settings).unwrap();
        g.disconnect().unwrap();
        assert!(!settings.exists());
        assert_eq!(
            std::fs::read_to_string(env).unwrap(),
            "# original\nCUSTOM=x\n"
        );
        assert!(!g.stash_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (settings, env, state) = tmp("private");
        let g = GeminiCli::new(settings, env, state, 3427);
        g.apply().unwrap();
        assert_eq!(
            std::fs::metadata(g.stash_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn auto_alias_is_reversible_and_preserves_user_overrides() {
        let (settings, env, state) = tmp("auto-alias");
        let original = r#"{"modelConfigs":{"customAliases":{},"customOverrides":[{"match":{"model":"aubridge"},"modelConfig":{"generateContentConfig":{"topK":7}}}]}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            value["modelConfigs"]["customAliases"]["aubridge"],
            json!({"modelConfig":{"model":"aubridge"}})
        );
        assert_eq!(
            value["modelConfigs"]["customOverrides"][0]["modelConfig"]["generateContentConfig"]
                ["topK"],
            7
        );
        g.apply().unwrap();
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn conflicting_user_alias_never_changes_files_or_stash() {
        let (settings, env, state) = tmp("alias-conflict");
        let original = r#"{"modelConfigs":{"customAliases":{"aubridge":{"modelConfig":{"model":"other","generateContentConfig":{"topK":19}}}}}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        assert!(g.apply().is_err());
        assert!(!g.stash_path().exists());
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn compatible_user_alias_keeps_sampling_and_is_never_removed() {
        let (settings, env, state) = tmp("alias-user");
        let original = r#"{"modelConfigs":{"customAliases":{"aubridge":{"modelConfig":{"model":"aubridge","generateContentConfig":{"topK":19}}}}}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            value["modelConfigs"]["customAliases"]["aubridge"]["modelConfig"]
                ["generateContentConfig"]["topK"],
            19
        );
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn managed_alias_drift_blocks_mutation() {
        let (settings, env, state) = tmp("alias-drift");
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        value["modelConfigs"]["customAliases"]["aubridge"]["modelConfig"]["model"] =
            json!("external");
        atomic_write_json(&settings, &value).unwrap();
        assert_eq!(g.drift(), Drift::Replaced);
        assert!(g.apply().is_err());
        assert!(g.disconnect().is_err());
    }

    #[test]
    fn old_v2_stash_without_alias_ownership_preserves_user_alias() {
        let (settings, env, state) = tmp("alias-old-v2");
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let mut saved: Value =
            serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
        saved.as_object_mut().unwrap().remove("auto_alias");
        saved.as_object_mut().unwrap().remove("selected_alias");
        saved["managed"].as_object_mut().unwrap().remove("alias");
        atomic_write_json(&g.stash_path(), &saved).unwrap();
        g.disconnect().unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(
            find_json(&value, AUTO_ALIAS_PATH),
            Some(&auto_alias_value())
        );
    }
    #[test]
    fn pending_first_alias_install_is_reapplied_after_interruption() {
        let (settings, env, state) = tmp("alias-retry");
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let mut saved = g.load().unwrap().unwrap();
        saved.pending = true;
        atomic_write_json(&g.stash_path(), &serde_json::to_value(saved).unwrap()).unwrap();
        std::fs::remove_file(&settings).unwrap();
        g.apply().unwrap();
        assert!(g.wired());
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(
            find_json(&value, AUTO_ALIAS_PATH),
            Some(&auto_alias_value())
        );
    }

    #[test]
    fn sdk_alias_sampling_is_not_shadowed_by_new_custom_alias() {
        let (settings, env, state) = tmp("alias-sdk");
        let original = r#"{"modelConfigs":{"aliases":{"aubridge":{"modelConfig":{"model":"aubridge","generateContentConfig":{"topK":19}}}}}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(find_json(&value, AUTO_ALIAS_PATH).is_none());
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn conflicting_sdk_alias_rejects_before_any_write() {
        let (settings, env, state) = tmp("alias-sdk-conflict");
        let original =
            r#"{"modelConfigs":{"aliases":{"aubridge":{"modelConfig":{"model":"other"}}}}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        assert!(g.apply().is_err());
        assert!(!g.stash_path().exists());
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }
    #[test]
    fn selected_alias_switch_is_literal_and_leaves_only_current_owned_key() {
        let (settings, env, state) = tmp("selected-switch");
        let original = "{ \"theme\": \"dark\", \"modelConfigs\": {\"customAliases\": {}} }";
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        for model in ["provider/model:free", "gpt-6.1-sol", "aubridge"] {
            g.set_model(model).unwrap();
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
            assert_eq!(value["model"]["name"], model);
            let aliases = value["modelConfigs"]["customAliases"].as_object().unwrap();
            assert_eq!(aliases.len(), 1);
            assert_eq!(aliases[model], json!({"modelConfig":{"model":model}}));
        }
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn selected_alias_conflict_does_not_change_any_of_three_files() {
        let (settings, env, state) = tmp("selected-conflict");
        std::fs::write(&settings,r#"{"modelConfigs":{"customAliases":{"gpt-6.1-sol":{"modelConfig":{"model":"other"}}}}}"#).unwrap();
        let g = GeminiCli::new(settings.clone(), env.clone(), state, 3427);
        g.apply().unwrap();
        let before = [&settings, &env, &g.stash_path()].map(|p| std::fs::read(p).unwrap());
        assert!(g.set_model("gpt-6.1-sol").is_err());
        let after = [&settings, &env, &g.stash_path()].map(|p| std::fs::read(p).unwrap());
        assert_eq!(before, after);
    }

    #[test]
    fn selected_sdk_alias_and_user_overrides_keep_ownership() {
        let (settings, env, state) = tmp("selected-sdk");
        let original = r#"{"modelConfigs":{"aliases":{"gpt-6.1-sol":{"modelConfig":{"model":"gpt-6.1-sol","generateContentConfig":{"topK":19}}}},"customOverrides":[{"model":"gpt-6.1-sol","temperature":0.3}]}}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        g.set_model("gpt-6.1-sol").unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(value["modelConfigs"].get("customAliases").is_none());
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn selected_alias_drift_is_never_deleted_or_repaired() {
        let (settings, env, state) = tmp("selected-drift");
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        g.set_model("gpt-6.1-sol").unwrap();
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        value["modelConfigs"]["customAliases"]["gpt-6.1-sol"] =
            json!({"modelConfig":{"model":"user-replacement"}});
        atomic_write_json(&settings, &value).unwrap();
        assert_eq!(g.drift(), Drift::Replaced);
        let before = std::fs::read(&settings).unwrap();
        assert!(g.apply().is_err());
        assert!(g.disconnect().is_err());
        assert_eq!(std::fs::read(settings).unwrap(), before);
    }

    #[test]
    fn selected_alias_migrates_old_v2_auto_marker_with_explicit_model() {
        let (settings, env, state) = tmp("selected-v2");
        let original = r#"{"theme":"dark"}"#;
        std::fs::write(&settings, original).unwrap();
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let mut document: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        document["model"]["name"] = json!("legacy-explicit");
        atomic_write_json(&settings, &document).unwrap();
        let mut saved: Value =
            serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
        let mut marker = saved
            .as_object_mut()
            .unwrap()
            .remove("selected_alias")
            .unwrap();
        marker.as_object_mut().unwrap().remove("name");
        saved["auto_alias"] = marker;
        saved["managed"]["settings"][1] = json!("legacy-explicit");
        atomic_write_json(&g.stash_path(), &saved).unwrap();
        assert!(g.wired());
        g.apply().unwrap();
        g.set_model("gpt-6.1-sol").unwrap();
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let aliases = document["modelConfigs"]["customAliases"]
            .as_object()
            .unwrap();
        assert_eq!(aliases.len(), 1);
        assert!(aliases.contains_key("gpt-6.1-sol"));
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
        assert!(saved.get("auto_alias").is_none());
        g.disconnect().unwrap();
        assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
    }

    #[test]
    fn selected_alias_pending_switch_recovers_atomic_before_and_after_states() {
        for before_write in [true, false] {
            let (settings, env, state) = tmp(if before_write {
                "selected-pending-before"
            } else {
                "selected-pending-after"
            });
            let original = r#"{"modelConfigs":{"customAliases":{}}}"#;
            std::fs::write(&settings, original).unwrap();
            let g = GeminiCli::new(settings.clone(), env, state, 3427);
            g.apply().unwrap();
            let before: Value =
                serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
            let before_settings = std::fs::read(&settings).unwrap();
            g.set_model("gpt-6.1-sol").unwrap();
            let mut pending: Value =
                serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
            pending["pending"] = json!(true);
            pending["previous"] = before["managed"].clone();
            pending["previous_alias"] = before["selected_alias"].clone();
            atomic_write_json(&g.stash_path(), &pending).unwrap();
            if before_write {
                std::fs::write(&settings, before_settings).unwrap();
                g.apply().unwrap();
                assert_eq!(g.current_model().as_deref(), Some("gpt-6.1-sol"));
                assert!(g.wired());
            }
            g.disconnect().unwrap();
            assert_eq!(std::fs::read_to_string(settings).unwrap(), original);
        }
    }

    #[test]
    fn selected_alias_pending_rejects_user_replacement_in_old_owner_key() {
        let (settings, env, state) = tmp("selected-pending-replaced");
        let g = GeminiCli::new(settings.clone(), env, state, 3427);
        g.apply().unwrap();
        let before: Value =
            serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
        g.set_model("gpt-6.1-sol").unwrap();
        let mut pending: Value =
            serde_json::from_str(&std::fs::read_to_string(g.stash_path()).unwrap()).unwrap();
        pending["pending"] = json!(true);
        pending["previous"] = before["managed"].clone();
        pending["previous_alias"] = before["selected_alias"].clone();
        atomic_write_json(&g.stash_path(), &pending).unwrap();
        let mut document: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        document["modelConfigs"]["customAliases"]["aubridge"] =
            json!({"modelConfig":{"model":"user-replacement"}});
        atomic_write_json(&settings, &document).unwrap();
        let bytes = std::fs::read(&settings).unwrap();
        assert!(g.apply().is_err());
        assert!(g.disconnect().is_err());
        assert_eq!(std::fs::read(settings).unwrap(), bytes);
    }
}

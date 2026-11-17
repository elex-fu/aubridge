//! An opt-in Codex profile with a separate lifetime from the user's config.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    atomic_create, atomic_write, remove_if_present, AgentWiring, DisconnectChange,
    DisconnectPreview, Drift, RestoreAction,
};

const PROFILE: &str = "aubridge.config.toml";
const LEGACY_MARKER: &str = "# aubridge: managed block, disconnect restores original";
const BASE_PREFIX: &str = "http://127.0.0.1:";

pub struct Codex {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub port: u16,
}

/// Editable top-level fields of the managed Codex profile. The provider
/// table (base URL, wired token, wire API) stays owned by aubridge.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexConfigFields {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(default = "default_web_search")]
    pub web_search: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_max_output_tokens: Option<u64>,
}

fn default_web_search() -> String {
    "disabled".into()
}

impl CodexConfigFields {
    fn with_model(model: &str) -> Self {
        Self {
            model: model.into(),
            web_search: default_web_search(),
            ..Self::default()
        }
    }

    fn normalized(mut self) -> io::Result<Self> {
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            return Err(io::Error::other("Codex 模型不能为空"));
        }
        self.model_reasoning_effort = clean(self.model_reasoning_effort);
        self.model_reasoning_summary = clean(self.model_reasoning_summary);
        self.model_verbosity = clean(self.model_verbosity);
        self.approval_policy = clean(self.approval_policy);
        self.sandbox_mode = clean(self.sandbox_mode);
        self.web_search = clean(Some(self.web_search)).unwrap_or_else(default_web_search);
        for (name, value) in [
            ("model_context_window", self.model_context_window),
            ("model_max_output_tokens", self.model_max_output_tokens),
        ] {
            if value == Some(0) {
                return Err(io::Error::other(format!("{name} 必须大于 0")));
            }
        }
        Ok(self)
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

struct Profile {
    port: u16,
    fields: CodexConfigFields,
}

impl Profile {
    fn text(&self) -> String {
        let mut text = String::from("# aubridge: managed Codex profile\n");
        text.push_str(&format!(
            "model = {}\nmodel_provider = \"aubridge\"\n",
            toml_string(&self.fields.model)
        ));
        for (key, value) in [
            (
                "model_reasoning_effort",
                self.fields.model_reasoning_effort.as_deref(),
            ),
            (
                "model_reasoning_summary",
                self.fields.model_reasoning_summary.as_deref(),
            ),
            ("model_verbosity", self.fields.model_verbosity.as_deref()),
            ("approval_policy", self.fields.approval_policy.as_deref()),
            ("sandbox_mode", self.fields.sandbox_mode.as_deref()),
        ] {
            if let Some(value) = value {
                text.push_str(&format!("{key} = {}\n", toml_string(value)));
            }
        }
        if let Some(value) = self.fields.model_context_window {
            text.push_str(&format!("model_context_window = {value}\n"));
        }
        if let Some(value) = self.fields.model_max_output_tokens {
            text.push_str(&format!("model_max_output_tokens = {value}\n"));
        }
        text.push_str(&format!(
            "web_search = {}\n\n[model_providers.aubridge]\nname = \"aubridge\"\n\
             base_url = \"{BASE_PREFIX}{}/v1\"\nexperimental_bearer_token = \"aubridge-codex\"\n\
             wire_api = \"responses\"\nsupports_websockets = false\n",
            toml_string(&self.fields.web_search),
            self.port,
        ));
        text
    }

    fn owned(text: &str) -> Option<Self> {
        let table: toml::Table = toml::from_str(text).ok()?;
        let base = table
            .get("model_providers")?
            .get("aubridge")?
            .get("base_url")?
            .as_str()?;
        let fields = CodexConfigFields {
            model: table.get("model")?.as_str()?.into(),
            model_reasoning_effort: table_string(&table, "model_reasoning_effort"),
            model_reasoning_summary: table_string(&table, "model_reasoning_summary"),
            model_verbosity: table_string(&table, "model_verbosity"),
            approval_policy: table_string(&table, "approval_policy"),
            sandbox_mode: table_string(&table, "sandbox_mode"),
            web_search: table
                .get("web_search")
                .and_then(toml::Value::as_str)
                .unwrap_or("disabled")
                .into(),
            model_context_window: table
                .get("model_context_window")
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok()),
            model_max_output_tokens: table
                .get("model_max_output_tokens")
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok()),
        };
        let profile = Self {
            port: parse_port(base)?,
            fields,
        };
        (profile.text() == text).then_some(profile)
    }
}

fn table_string(table: &toml::Table, key: &str) -> Option<String> {
    Some(table.get(key)?.as_str()?.to_string())
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serialize TOML string")
}

struct Legacy {
    active: Option<String>,
    original: Option<String>,
}

impl Codex {
    pub fn new(config: PathBuf, state_dir: PathBuf, port: u16) -> Self {
        Self {
            config,
            state_dir,
            port,
        }
    }

    pub fn apply(&self) -> io::Result<()> {
        let path = self.profile_path();
        let current = read_regular(&path)?;
        let fields = match &current {
            Some(text) => Profile::owned(text).ok_or_else(profile_conflict)?.fields,
            None => CodexConfigFields::with_model("aubridge"),
        };
        let legacy = self.legacy()?;
        let base = match &legacy {
            Some(legacy) => legacy.original.clone(),
            None => read_optional(&self.config)?,
        };
        validate_base(base.as_deref())?;
        if let Some(legacy) = &legacy {
            self.restore_legacy(legacy)?;
        }
        let next = Profile {
            port: self.port,
            fields,
        }
        .text();
        write_profile(&path, current.as_deref(), &next)?;
        if legacy.is_some() {
            remove_if_present(&self.legacy_path())?;
        }
        Ok(())
    }

    pub fn repair_port(&self) -> io::Result<()> {
        let path = self.profile_path();
        if let Some(current) = read_regular(&path)? {
            if let Some(mut profile) = Profile::owned(&current) {
                profile.port = self.port;
                write_profile(&path, Some(&current), &profile.text())?;
            }
        }
        Ok(())
    }

    pub fn disconnect_preview(&self) -> io::Result<DisconnectPreview> {
        let current = read_regular(&self.profile_path())?;
        Ok(Self::preview(current.as_deref(), self.legacy().as_ref()))
    }

    pub fn disconnect(&self) -> io::Result<()> {
        self.disconnect_reviewed(None)
    }

    pub fn disconnect_reviewed(&self, expected: Option<&DisconnectPreview>) -> io::Result<()> {
        let path = self.profile_path();
        let current = read_regular(&path)?;
        let legacy = self.legacy();
        let preview = Self::preview(current.as_deref(), legacy.as_ref());
        if preview.blocked || expected.is_some_and(|expected| expected != &preview) {
            return Err(io::Error::other("Codex 配置已变化，请重新预览"));
        }
        if let Some(legacy) = legacy? {
            self.restore_legacy(&legacy)?;
            remove_if_present(&self.legacy_path())?;
        }
        if let Some(current) = current {
            if read_regular(&path)?.as_deref() != Some(&current) {
                return Err(profile_conflict());
            }
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn preview(
        profile: Option<&str>,
        legacy: Result<&Option<Legacy>, &io::Error>,
    ) -> DisconnectPreview {
        let mut changes = Vec::new();
        let mut blocked = legacy.is_err();
        if let Some(profile) = profile {
            let owned = Profile::owned(profile).is_some();
            blocked |= !owned;
            changes.push(DisconnectChange {
                field: PROFILE.into(),
                action: if owned {
                    RestoreAction::Remove
                } else {
                    RestoreAction::Keep
                },
            });
        }
        if !matches!(legacy, Ok(None)) {
            changes.push(DisconnectChange {
                field: "config.toml（旧接线）".into(),
                action: if legacy.is_ok() {
                    RestoreAction::Restore
                } else {
                    RestoreAction::Keep
                },
            });
        }
        DisconnectPreview { changes, blocked }
    }

    pub fn wired(&self) -> bool {
        self.current()
            .is_some_and(|profile| profile.port == self.port)
    }

    pub fn current_model(&self) -> Option<String> {
        self.current().map(|profile| profile.fields.model)
    }

    pub fn set_model(&self, model: &str) -> io::Result<()> {
        let path = self.profile_path();
        let current = read_regular(&path)?.ok_or_else(|| io::Error::other("请先接线 Codex"))?;
        let mut profile = Profile::owned(&current).ok_or_else(profile_conflict)?;
        profile.fields.model = model.into();
        profile.port = self.port;
        write_profile(&path, Some(&current), &profile.text())
    }

    pub(crate) fn profile_text(&self) -> io::Result<Option<String>> {
        read_regular(&self.profile_path())
    }

    pub(crate) fn profile_fields(&self) -> io::Result<Option<CodexConfigFields>> {
        Ok(self
            .profile_text()?
            .and_then(|text| Profile::owned(&text))
            .map(|profile| profile.fields))
    }

    pub(crate) fn update_fields(
        &self,
        expected_revision: &str,
        fields: CodexConfigFields,
    ) -> io::Result<()> {
        let path = self.profile_path();
        let current = read_regular(&path)?.ok_or_else(|| io::Error::other("请先接线 Codex"))?;
        if crate::content_revision(Some(&current)) != expected_revision {
            return Err(io::Error::other(
                "Codex 配置已在其他地方修改，请重新加载后再保存",
            ));
        }
        Profile::owned(&current).ok_or_else(profile_conflict)?;
        let next = Profile {
            port: self.port,
            fields: fields.normalized()?,
        }
        .text();
        write_profile(&path, Some(&current), &next)
    }

    fn current(&self) -> Option<Profile> {
        Profile::owned(&read_regular(&self.profile_path()).ok().flatten()?)
    }

    fn profile_path(&self) -> PathBuf {
        self.config.with_file_name(PROFILE)
    }

    fn legacy_path(&self) -> PathBuf {
        self.state_dir.join("codex.stash.txt")
    }

    fn legacy(&self) -> io::Result<Option<Legacy>> {
        let current = read_optional(&self.config)?;
        let Some(stash) = read_regular(&self.legacy_path())? else {
            return if current
                .as_deref()
                .is_some_and(|text| text.contains(LEGACY_MARKER))
            {
                Err(legacy_conflict())
            } else {
                Ok(None)
            };
        };
        let (original, timestamp) = stash
            .rsplit_once("\n---mtime---\n")
            .ok_or_else(legacy_conflict)?;
        let timestamp = timestamp.parse::<u64>().map_err(|_| legacy_conflict())?;
        let original_file = (timestamp != 0 || !original.is_empty()).then(|| original.to_owned());
        if current != original_file {
            let text = current.as_deref().ok_or_else(legacy_conflict)?;
            let (prefix, block) = text
                .rsplit_once(LEGACY_MARKER)
                .ok_or_else(legacy_conflict)?;
            let port = block
                .lines()
                .find_map(|line| {
                    let value = line.strip_prefix("base_url = \"")?.strip_suffix('"')?;
                    parse_port(value)
                })
                .ok_or_else(legacy_conflict)?;
            if !prefix.ends_with('\n') || text != legacy_text(original, port) {
                return Err(legacy_conflict());
            }
        }
        Ok(Some(Legacy {
            active: current,
            original: original_file,
        }))
    }

    fn restore_legacy(&self, legacy: &Legacy) -> io::Result<()> {
        if legacy.active == legacy.original {
            return Ok(());
        }
        if read_regular(&self.config)? != legacy.active {
            return Err(legacy_conflict());
        }
        match &legacy.original {
            Some(original) => atomic_write(&self.config, original.as_bytes()),
            None => fs::remove_file(&self.config),
        }
    }
}

impl AgentWiring for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn name(&self) -> &'static str {
        "Codex CLI"
    }
    fn apply(&self) -> io::Result<()> {
        Codex::apply(self)
    }
    fn disconnect(&self) -> io::Result<()> {
        Codex::disconnect(self)
    }
    fn wired(&self) -> bool {
        Codex::wired(self)
    }
    fn detected(&self) -> bool {
        self.config.exists() || self.profile_path().exists()
    }
    fn hint(&self) -> Option<&'static str> {
        Some("codex --profile aubridge")
    }
    fn current_model(&self) -> Option<String> {
        Codex::current_model(self)
    }
    fn set_model(&self, model: &str) -> io::Result<()> {
        Codex::set_model(self, model)
    }
    fn drift(&self) -> Option<Drift> {
        Some(match read_regular(&self.profile_path()) {
            Ok(Some(text)) => match Profile::owned(&text) {
                Some(profile) if profile.port == self.port => Drift::Clean,
                Some(_) => Drift::PortChanged,
                None => Drift::Replaced,
            },
            Ok(None) if self.legacy().is_ok() => Drift::Unwired,
            _ => Drift::Replaced,
        })
    }
}

fn validate_base(text: Option<&str>) -> io::Result<()> {
    let Some(text) = text else { return Ok(()) };
    let table: toml::Table =
        toml::from_str(text).map_err(|_| io::Error::other("Codex 默认配置 TOML 无法解析"))?;
    if table.get("profile").and_then(toml::Value::as_str) == Some("aubridge")
        || ["profiles", "model_providers"].iter().any(|key| {
            table
                .get(*key)
                .and_then(|value| value.get("aubridge"))
                .is_some()
        })
    {
        return Err(profile_conflict());
    }
    Ok(())
}

fn write_profile(path: &Path, current: Option<&str>, next: &str) -> io::Result<()> {
    match current {
        None => atomic_create(path, next.as_bytes()),
        Some(current) if current == next => Ok(()),
        Some(current) if read_regular(path)?.as_deref() == Some(current) => {
            atomic_write(path, next.as_bytes())
        }
        Some(_) => Err(profile_conflict()),
    }
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_regular(path: &Path) -> io::Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => read_optional(path),
        Ok(_) => Err(profile_conflict()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_port(url: &str) -> Option<u16> {
    url.strip_prefix(BASE_PREFIX)?
        .strip_suffix("/v1")?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
}

fn profile_conflict() -> io::Error {
    io::Error::other("Codex 已有同名配置或受管 profile 已被修改")
}

fn legacy_conflict() -> io::Error {
    io::Error::other("无法确认旧版 Codex 接线与首次备份，请检查 config.toml")
}

fn legacy_text(original: &str, port: u16) -> String {
    const TOP: &str = "model_provider = \"aubridge\"\n";
    let start = if original.contains(TOP) {
        original.into()
    } else {
        format!("# aubridge: active provider\n{TOP}{original}")
    };
    format!("{start}\n{LEGACY_MARKER}\n[model_providers.aubridge]\nname = \"aubridge\"\n\
        base_url = \"{BASE_PREFIX}{port}/v1\"\nenv_key = \"AUBRIDGE_TOKEN\"\nwire_api = \"chat\"\n\n\
        # aubridge: set as active provider\n{TOP}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> (Codex, String) {
        let root = std::env::temp_dir().join(format!(
            "aubridge-codex-profile-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("state")).unwrap();
        let codex = Codex::new(root.join("config.toml"), root.join("state"), 3427);
        let original = "# user comment\nmodel = \"gpt-5.1\"\n".to_owned();
        fs::write(&codex.config, &original).unwrap();
        (codex, original)
    }

    #[test]
    fn profile_is_separate_idempotent_and_keeps_model_when_port_changes() {
        let (mut codex, original) = fixture("lifecycle");
        assert_eq!(codex.current_model(), None);
        codex.apply().unwrap();
        let created = fs::read_to_string(codex.profile_path()).unwrap();
        assert!(created.contains("wire_api = \"responses\""));
        codex.apply().unwrap();
        assert_eq!(fs::read_to_string(codex.profile_path()).unwrap(), created);
        let model = "group/one+two";
        codex.set_model(model).unwrap();
        codex.port = 4891;
        assert_eq!(codex.drift(), Some(Drift::PortChanged));
        codex.repair_port().unwrap();
        assert!(codex.wired());
        assert_eq!(codex.current_model().as_deref(), Some(model));
        let preview = codex.disconnect_preview().unwrap();
        assert_eq!(preview.changes[0].action, RestoreAction::Remove);
        codex.disconnect_reviewed(Some(&preview)).unwrap();
        assert!(!codex.profile_path().exists());
        assert_eq!(fs::read_to_string(&codex.config).unwrap(), original);
    }

    #[test]
    fn legacy_generated_profile_stays_owned_after_advanced_fields_land() {
        let old = "# aubridge: managed Codex profile\nmodel = \"aubridge\"\n\
model_provider = \"aubridge\"\nweb_search = \"disabled\"\n\n\
[model_providers.aubridge]\nname = \"aubridge\"\n\
base_url = \"http://127.0.0.1:3427/v1\"\nexperimental_bearer_token = \"aubridge-codex\"\n\
wire_api = \"responses\"\nsupports_websockets = false\n";
        let profile = Profile::owned(old).expect("old profile remains owned");
        assert_eq!(profile.fields.model, "aubridge");
        assert_eq!(profile.fields.web_search, "disabled");
        assert_eq!(profile.text(), old);

        let advanced = Profile {
            port: 3427,
            fields: CodexConfigFields {
                model: "gpt-6.1-sol".into(),
                model_reasoning_effort: Some("high".into()),
                model_reasoning_summary: Some("auto".into()),
                model_verbosity: Some("medium".into()),
                approval_policy: Some("on-request".into()),
                sandbox_mode: Some("read-only".into()),
                web_search: "disabled".into(),
                model_context_window: Some(400000),
                model_max_output_tokens: Some(131072),
            },
        };
        let text = advanced.text();
        let parsed = Profile::owned(&text).expect("advanced profile is owned");
        assert_eq!(parsed.fields, advanced.fields);
    }

    #[test]
    fn set_model_keeps_advanced_fields() {
        let (codex, _) = fixture("advanced-model");
        codex.apply().unwrap();
        let fields = CodexConfigFields {
            model: "keep-me".into(),
            model_reasoning_effort: Some("high".into()),
            ..CodexConfigFields::with_model("keep-me")
        };
        let text = fs::read_to_string(codex.profile_path()).unwrap();
        codex
            .update_fields(&crate::content_revision(Some(&text)), fields)
            .unwrap();
        codex.set_model("next-model").unwrap();
        let profile = codex.current().unwrap();
        assert_eq!(profile.fields.model, "next-model");
        assert_eq!(
            profile.fields.model_reasoning_effort.as_deref(),
            Some("high")
        );
    }

    #[test]
    fn an_existing_or_externally_edited_profile_is_left_in_place() {
        let (codex, _) = fixture("conflict");
        let path = codex.profile_path();
        let user = "model = \"user-model\"\n";
        fs::write(&path, user).unwrap();
        assert!(codex.apply().is_err());
        assert!(codex.disconnect_preview().unwrap().blocked);
        assert_eq!(fs::read_to_string(&path).unwrap(), user);
        fs::remove_file(&path).unwrap();
        codex.apply().unwrap();
        let preview = codex.disconnect_preview().unwrap();
        let edited = format!("{}# edited\n", fs::read_to_string(&path).unwrap());
        fs::write(&path, &edited).unwrap();
        assert!(codex.disconnect_reviewed(Some(&preview)).is_err());
        codex.repair_port().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn an_unchanged_legacy_install_migrates_and_restores_the_first_backup() {
        let (codex, original) = fixture("legacy");
        fs::write(&codex.config, legacy_text(&original, 1111)).unwrap();
        fs::write(codex.legacy_path(), format!("{original}\n---mtime---\n8")).unwrap();
        codex.apply().unwrap();
        assert!(codex.wired());
        assert_eq!(fs::read_to_string(&codex.config).unwrap(), original);
        assert!(!codex.legacy_path().exists());
    }

    #[test]
    fn legacy_conflict_and_user_profile_names_do_not_overwrite_the_default() {
        let (codex, original) = fixture("legacy-edited");
        let edited = format!("{}# edited\n", legacy_text(&original, 1111));
        fs::write(&codex.config, &edited).unwrap();
        fs::write(codex.legacy_path(), format!("{original}\n---mtime---\n8")).unwrap();
        assert!(codex.apply().is_err());
        assert!(codex.disconnect_preview().unwrap().blocked);
        assert_eq!(fs::read_to_string(&codex.config).unwrap(), edited);
        assert!(codex.legacy_path().exists());

        fs::remove_file(codex.legacy_path()).unwrap();
        let custom = "[profiles.aubridge]\nmodel = \"user-model\"\n";
        fs::write(&codex.config, custom).unwrap();
        assert!(codex.apply().is_err());
        assert_eq!(fs::read_to_string(&codex.config).unwrap(), custom);
        assert!(!codex.profile_path().exists());
    }
}

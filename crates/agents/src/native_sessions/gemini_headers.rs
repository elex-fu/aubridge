//! D54: refuse existing custom headers; never merge or retain their secret values.
use crate::{project_check::ProjectCheck, AgentsPaths};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
};

pub const CONFLICT: &str = "gemini_custom_headers_conflict";
const KEY: &str = "GEMINI_CLI_CUSTOM_HEADERS";
const LIMIT: u64 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderConflict;

pub fn expected(id: &str) -> Result<String, HeaderConflict> {
    if !super::uuid(id) {
        return Err(HeaderConflict);
    }
    Ok(format!("x-session-id: {id}"))
}

fn allowed(value: Option<&str>, expected: &str) -> Result<(), HeaderConflict> {
    if value.is_none_or(|value| value.is_empty() || value == expected) {
        Ok(())
    } else {
        Err(HeaderConflict)
    }
}

fn bytes(path: &Path) -> Result<Option<Vec<u8>>, HeaderConflict> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(HeaderConflict),
    };
    let before = file.metadata().map_err(|_| HeaderConflict)?;
    if !before.is_file() || before.len() > LIMIT {
        return Err(HeaderConflict);
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HeaderConflict)?;
    if bytes.len() as u64 > LIMIT {
        return Err(HeaderConflict);
    }
    let after = fs::metadata(path).map_err(|_| HeaderConflict)?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(HeaderConflict);
    }
    Ok(Some(bytes))
}

fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_str() == Some("")
        || value.as_object().is_some_and(|v| v.is_empty())
        || value.as_array().is_some_and(|v| v.is_empty())
}
fn header_value(value: &Value, expected: &str) -> Result<(), HeaderConflict> {
    if empty(value) {
        return Ok(());
    }
    allowed(Some(value.as_str().ok_or(HeaderConflict)?), expected)
}
fn connection(value: &Value, expected: &str) -> Result<(), HeaderConflict> {
    for key in [
        "/customHeaders",
        "/headers",
        "/httpOptions/headers",
        "/generateContentConfig/httpOptions/headers",
    ] {
        if let Some(value) = value.pointer(key) {
            header_value(value, expected)?;
        }
    }
    for key in [
        "/baseUrl",
        "/httpOptions/baseUrl",
        "/generateContentConfig/httpOptions/baseUrl",
    ] {
        if value.pointer(key).is_some_and(|v| !empty(v)) {
            return Err(HeaderConflict);
        }
    }
    Ok(())
}

fn settings(value: &Value, model: &str, expected: &str) -> Result<(), HeaderConflict> {
    // These are connection-shaped fields, not a new settings.env import mechanism.
    connection(value, expected)?;
    if let Some(auth) = value.pointer("/security/auth") {
        connection(auth, expected)?;
    }
    let configs = &value["modelConfigs"];
    let mut names = vec![model.to_owned()];
    let mut visited = std::collections::BTreeSet::new();
    let mut eligible = std::collections::BTreeSet::from([model.to_owned()]);
    let mut unknown_base = false;
    while let Some(name) = names.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        eligible.insert(name.clone());
        if visited.len() > 32 {
            return Err(HeaderConflict);
        }
        let mut found = false;
        for kind in ["customAliases", "aliases"] {
            if let Some(alias) = configs[kind].get(name.as_str()) {
                found = true;
                if let Some(config) = alias.get("modelConfig") {
                    connection(config, expected)?;
                    if let Some(base) = config["model"].as_str() {
                        eligible.insert(base.to_owned());
                    }
                }
                if let Some(parent) = alias["extends"].as_str() {
                    names.push(parent.into());
                }
            }
        }
        if !found && name != model {
            unknown_base = true;
        }
    }
    for kind in ["overrides", "customOverrides"] {
        if let Some(overrides) = configs[kind].as_array() {
            for item in overrides {
                if !unknown_base
                    && item
                        .pointer("/match/model")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !eligible.contains(name))
                {
                    continue;
                }
                if let Some(config) = item.get("modelConfig") {
                    connection(config, expected)?;
                }
            }
        }
    }
    Ok(())
}

fn dotenv(data: &[u8], expected: &str) -> Result<(), HeaderConflict> {
    let text = std::str::from_utf8(data).map_err(|_| HeaderConflict)?;
    let mut seen = false;
    for line in text.strip_prefix('\u{feff}').unwrap_or(text).lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .or_else(|| line.strip_prefix("export\t"))
            .unwrap_or(line)
            .trim_start();
        let (name, value) = line.split_once('=').ok_or(HeaderConflict)?;
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        {
            return Err(HeaderConflict);
        }
        let value = crate::project_env::value(value).map_err(|_| HeaderConflict)?;
        if name == KEY {
            if seen {
                return Err(HeaderConflict);
            }
            seen = true;
            allowed(Some(&value), expected)?;
        }
    }
    Ok(())
}

fn sources(
    files: &[PathBuf],
    selected_env: Option<&Path>,
    inherited: Option<&OsStr>,
    model: &str,
    id: &str,
) -> Result<u64, HeaderConflict> {
    let expected = expected(id)?;
    let inherited = inherited
        .map(|v| v.to_str().ok_or(HeaderConflict))
        .transpose()?;
    allowed(inherited, &expected)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    inherited.hash(&mut hash);
    for path in files {
        path.hash(&mut hash);
        let data = bytes(path)?;
        if let Some(data) = &data {
            let text = std::str::from_utf8(data).map_err(|_| HeaderConflict)?;
            let config = jsonc_parser::parse_to_serde_value::<Value>(
                text,
                &jsonc_parser::ParseOptions::default(),
            )
            .map_err(|_| HeaderConflict)?;
            if !config.is_object() {
                return Err(HeaderConflict);
            }
            settings(&config, model, &expected)?;
        }
        data.hash(&mut hash);
    }
    selected_env.hash(&mut hash);
    if let Some(path) = selected_env {
        let data = bytes(path)?.ok_or(HeaderConflict)?;
        dotenv(&data, &expected)?;
        data.hash(&mut hash);
    }
    Ok(hash.finish())
}

/// Call only after the ordinary project/record/profile checks, with D53's actual env hit.
pub fn check(
    paths: &AgentsPaths,
    project: &ProjectCheck,
    id: &str,
    inherited: Option<&OsStr>,
) -> Result<u64, HeaderConflict> {
    if !project.can_launch() || project.env.as_ref().is_none_or(|v| v.trust != "trusted") {
        return Err(HeaderConflict);
    }
    let system = crate::project_check::system_settings().map_err(|_| HeaderConflict)?;
    let defaults = crate::project_check::system_defaults(&system).map_err(|_| HeaderConflict)?;
    let files = [
        defaults,
        paths.gemini_settings.clone(),
        project.canonical_path.join(".gemini/settings.json"),
        system,
    ];
    let user = bytes(&paths.gemini_settings)?.ok_or(HeaderConflict)?;
    let user = std::str::from_utf8(&user).map_err(|_| HeaderConflict)?;
    let user =
        jsonc_parser::parse_to_serde_value::<Value>(user, &jsonc_parser::ParseOptions::default())
            .map_err(|_| HeaderConflict)?;
    let model = user
        .pointer("/model/name")
        .and_then(Value::as_str)
        .ok_or(HeaderConflict)?;
    sources(
        &files,
        project.selected_env.as_deref(),
        inherited,
        model,
        id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "c18365a3-0000-4000-8000-000000000001";
    #[test]
    fn only_empty_or_exact_single_same_native_header_is_allowed() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/gemini-session-correlation.json"
        ))
        .unwrap();
        assert_eq!(
            super::expected(fixture["native_id"].as_str().unwrap()).unwrap(),
            fixture["header"].as_str().unwrap()
        );
        let expected = expected(ID).unwrap();
        for value in [None, Some(""), Some(expected.as_str())] {
            assert!(allowed(value, &expected).is_ok());
        }
        for secret in [
            "private-header-secret",
            "X-Session-Id: c18365a3-0000-4000-8000-000000000001",
            "thread-id: other",
            "x-session-id: other",
            "x-session-id: other, authorization: private-header-secret",
            " ",
            "x-session-id: other\n",
        ] {
            let error = allowed(Some(secret), &expected).unwrap_err();
            assert_eq!(error, HeaderConflict);
            assert!(!format!("{error:?}").contains(secret));
        }
    }
    #[test]
    fn dotenv_reuses_safe_decoder_and_rejects_duplicates_or_unknown_syntax() {
        let header = expected(ID).unwrap();
        assert!(dotenv(
            format!("export {KEY}='{header}' # local\nOTHER=value\n").as_bytes(),
            &header
        )
        .is_ok());
        for text in [
            format!("{KEY}=secret"),
            format!("{KEY}=\n{KEY}=\n"),
            format!("{KEY}=\"unterminated"),
            "unknown syntax".into(),
        ] {
            assert_eq!(dotenv(text.as_bytes(), &header), Err(HeaderConflict));
        }
    }
    #[test]
    fn connection_checks_do_not_read_mcp_headers_or_unselected_model_aliases() {
        let header = expected(ID).unwrap();
        let config = serde_json::json!({"mcpServers":{"fixture":{"headers":{"authorization":"private-mcp-secret"}}},"modelConfigs":{"customAliases":{"other":{"modelConfig":{"httpOptions":{"headers":{"authorization":"private-unused-secret"}}}}}}});
        settings(&config, "aubridge", &header).unwrap();
        let conflicting = serde_json::json!({"modelConfigs":{"customAliases":{"aubridge":{"modelConfig":{"httpOptions":{"headers":{"thread-id":"other"}}}}}}});
        assert_eq!(
            settings(&conflicting, "aubridge", &header),
            Err(HeaderConflict)
        );
    }
    #[test]
    fn standard_and_custom_overrides_check_related_models_without_blocking_obvious_unrelated_models(
    ) {
        let header = expected(ID).unwrap();
        for kind in ["overrides", "customOverrides"] {
            let mut config = serde_json::json!({"modelConfigs":{}});
            config["modelConfigs"][kind] = serde_json::json!([{"match":{"model":"aubridge"},"modelConfig":{"generateContentConfig":{"httpOptions":{"headers":{"thread-id":"fixture-other"}}}}}]);
            assert_eq!(settings(&config, "aubridge", &header), Err(HeaderConflict));
            config["modelConfigs"][kind][0]["match"]["model"] =
                serde_json::json!("obviously-unrelated-model");
            settings(&config, "aubridge", &header).unwrap();
        }
    }
    #[test]
    fn only_selected_dotenv_is_checked_and_its_changes_invalidate_snapshot() {
        let root =
            std::env::temp_dir().join(format!("gemini-resume-headers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let selected = root.join("managed.env");
        let unrelated = root.join("unread.env");
        fs::write(&unrelated, format!("{KEY}=private-unselected-secret\n")).unwrap();
        fs::write(&selected, "OTHER=before\n").unwrap();
        let before = sources(&[], Some(&selected), None, "aubridge", ID).unwrap();
        fs::write(&selected, "OTHER=after\n").unwrap();
        assert_ne!(
            before,
            sources(&[], Some(&selected), None, "aubridge", ID).unwrap()
        );
        fs::write(&selected, format!("{KEY}=private-selected-secret\n")).unwrap();
        assert_eq!(
            sources(&[], Some(&selected), None, "aubridge", ID),
            Err(HeaderConflict)
        );
        assert!(fs::read_to_string(&unrelated)
            .unwrap()
            .contains("private-unselected-secret"));
        fs::remove_dir_all(root).unwrap();
    }
}

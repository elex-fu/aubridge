//! Decision 53: read-only configuration checks; matched never means project verified.
use crate::AgentsPaths;
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
const MAX_FILE: u64 = 1_048_576;
#[derive(Debug, Clone, Serialize)]
pub struct ProjectCheck {
    #[serde(skip)]
    pub canonical_path: PathBuf,
    #[serde(skip)]
    pub selected_env: Option<PathBuf>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub checked_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Environment>,
    pub actions: Vec<String>,
    pub verified: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    pub layer: String,
    pub trust: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_key_present: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_present: Option<bool>,
}
impl ProjectCheck {
    pub fn can_launch(&self) -> bool {
        self.status == "matched"
    }
    fn state(&mut self, status: &str, reason: &str) {
        self.status = status.into();
        self.reason = Some(reason.into());
        self.actions = vec![
            "recheck".into(),
            "review_configuration".into(),
            "keep_unchanged".into(),
        ];
    }
}
fn read_limited(path: &Path) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_FILE {
        return Err(io::Error::other("configuration too large"));
    }
    let mut text = String::new();
    file.take(MAX_FILE + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE {
        return Err(io::Error::other("configuration too large"));
    }
    Ok(text)
}
fn json(path: &Path) -> io::Result<Option<Value>> {
    match read_limited(path) {
        Ok(text) => jsonc_parser::parse_to_serde_value::<Value>(
            &text,
            &jsonc_parser::ParseOptions::default(),
        )
        .map_err(|e| io::Error::other(e.to_string()))
        .and_then(|value| {
            if value.is_object() {
                Ok(Some(value))
            } else {
                Err(io::Error::other("configuration must be an object"))
            }
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub fn check(paths: &AgentsPaths, agent: &str, cwd: &Path) -> io::Result<ProjectCheck> {
    if !cwd.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute project directory required",
        ));
    }
    let canonical = std::fs::canonicalize(cwd)?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "project directory required",
        ));
    }
    let mut result = ProjectCheck {
        canonical_path: canonical,
        selected_env: None,
        status: "matched".into(),
        reason: None,
        checked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        env: None,
        actions: vec![
            "recheck".into(),
            "restart_cli".into(),
            "run_project_task".into(),
            "keep_unchanged".into(),
        ],
        verified: false,
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let inspected = match agent {
        "gemini" => gemini(paths, &mut result, home.as_deref()),
        "claude-code" | "codex" | "opencode" => project_config(paths, agent, &mut result),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown agent")),
    };
    if let Err(error) = inspected {
        result.state(
            if error.kind() == io::ErrorKind::PermissionDenied {
                "unreadable"
            } else {
                "unknown"
            },
            "project_configuration_unreadable",
        );
    }
    Ok(result)
}
fn target_key(env: &Value) -> bool {
    env.as_object().is_some_and(|m| {
        m.keys().any(|key| {
            key.starts_with("ANTHROPIC_")
                || matches!(
                    key.as_str(),
                    "CLAUDE_CODE_USE_BEDROCK"
                        | "CLAUDE_CODE_USE_VERTEX"
                        | "CLAUDE_CODE_SUBAGENT_MODEL"
                )
        })
    })
}
fn same_file(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a)
        .ok()
        .zip(std::fs::canonicalize(b).ok())
        .is_some_and(|(a, b)| a == b)
}
fn project_config(paths: &AgentsPaths, agent: &str, result: &mut ProjectCheck) -> io::Result<()> {
    let dirs = result
        .canonical_path
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    for dir in dirs {
        match agent {
            "claude-code" => {
                for name in [".claude/settings.json", ".claude/settings.local.json"] {
                    let file = dir.join(name);
                    if same_file(&file, &paths.claude_settings) {
                        continue;
                    }
                    if let Some(v) = json(&file)? {
                        if target_key(&v["env"]) {
                            result.state("could_shadow", "project_connection_override");
                            return Ok(());
                        }
                    }
                }
            }
            "codex" => {
                let path = dir.join(".codex/config.toml");
                if same_file(&path, &paths.codex_config) {
                    continue;
                }
                match read_limited(&path) {
                    Ok(text) => {
                        let value: toml::Value = toml::from_str(&text)
                            .map_err(|_| io::Error::other("invalid project config"))?;
                        if value.get("model").is_some()
                            || value.get("model_provider").is_some()
                            || value
                                .get("model_providers")
                                .and_then(|v| v.get("aubridge"))
                                .is_some()
                            || value
                                .get("profiles")
                                .and_then(|v| v.get("aubridge"))
                                .is_some()
                        {
                            result.state("could_shadow", "project_profile_override");
                            return Ok(());
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            "opencode" => {
                for name in [
                    "opencode.json",
                    "opencode.jsonc",
                    ".opencode/opencode.json",
                    ".opencode/opencode.jsonc",
                ] {
                    let file = dir.join(name);
                    if same_file(&file, &paths.opencode_config) {
                        continue;
                    }
                    if let Some(v) = json(&file)? {
                        if v.get("model").is_some() || v.pointer("/provider/aubridge").is_some() {
                            result.state("could_shadow", "project_provider_override");
                            return Ok(());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn context_path(value: Option<std::ffi::OsString>, fallback: PathBuf) -> io::Result<PathBuf> {
    let path = value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute context configuration required",
        ));
    }
    Ok(path)
}
pub(crate) fn system_settings() -> io::Result<PathBuf> {
    context_path(
        std::env::var_os("GEMINI_CLI_SYSTEM_SETTINGS_PATH"),
        if cfg!(target_os = "macos") {
            PathBuf::from("/Library/Application Support/GeminiCli/settings.json")
        } else {
            PathBuf::from("/etc/gemini-cli/settings.json")
        },
    )
}
pub(crate) fn system_defaults(system: &Path) -> io::Result<PathBuf> {
    context_path(
        std::env::var_os("GEMINI_CLI_SYSTEM_DEFAULTS_PATH"),
        system.with_file_name("system-defaults.json"),
    )
}
fn trusted_folders(paths: &AgentsPaths) -> io::Result<PathBuf> {
    context_path(
        std::env::var_os("GEMINI_CLI_TRUSTED_FOLDERS_PATH"),
        paths.gemini_settings.with_file_name("trustedFolders.json"),
    )
}
/// Additional loader inputs needed to invalidate a project receipt. Values stay on device.
pub fn gemini_context_files(paths: &AgentsPaths) -> io::Result<Vec<PathBuf>> {
    let system = system_settings()?;
    let mut files = vec![system_defaults(&system)?, system, trusted_folders(paths)?];
    if let Some(home) = std::env::var_os("HOME") {
        let home = context_path(Some(home), PathBuf::new())?;
        files.extend([home.join(".gemini/.env"), home.join(".env")]);
    }
    Ok(files)
}
fn trust(paths: &AgentsPaths, cwd: &Path, user: &Value) -> io::Result<String> {
    if std::env::var("GEMINI_RESTRICTED_MODE").as_deref() == Ok("true")
        || std::env::var("GEMINI_CLI_TRUST_WORKSPACE").as_deref() == Ok("false")
    {
        return Ok("untrusted".into());
    }
    if std::env::var("GEMINI_CLI_TRUST_WORKSPACE").as_deref() == Ok("true") {
        return Ok("trusted".into());
    }
    let system = system_settings()?;
    let defaults = system_defaults(&system)?;
    let mut enabled = true;
    for config in [
        json(&defaults)?.unwrap_or(Value::Null),
        user.clone(),
        json(&system)?.unwrap_or(Value::Null),
    ] {
        if let Some(value) = config
            .pointer("/security/folderTrust/enabled")
            .and_then(Value::as_bool)
        {
            enabled = value;
        }
    }
    if !enabled {
        return Ok("trusted".into());
    }
    // An IDE may have higher-priority live trust; without its signal do not infer it.
    if std::env::var_os("GEMINI_CLI_IDE_SERVER_PORT").is_some() {
        return Ok("unknown".into());
    }
    let file = trusted_folders(paths)?;
    let Some(rules) = json(&file)? else {
        return Ok("unknown".into());
    };
    let mut best = None;
    if let Some(map) = rules.as_object() {
        for (path, level) in map {
            let path = PathBuf::from(path);
            let effective = if level == "TRUST_PARENT" {
                path.parent().map(Path::to_path_buf).unwrap_or(path.clone())
            } else {
                path.clone()
            };
            let effective = std::fs::canonicalize(&effective).unwrap_or(effective);
            if cwd.starts_with(effective)
                && best
                    .as_ref()
                    .is_none_or(|(len, _)| path.as_os_str().len() > *len)
            {
                best = Some((path.as_os_str().len(), level.as_str().unwrap_or("")));
            }
        }
    }
    Ok(match best.map(|(_, level)| level) {
        Some("TRUST_FOLDER" | "TRUST_PARENT") => "trusted",
        Some("DO_NOT_TRUST") => "untrusted",
        _ => "unknown",
    }
    .into())
}
fn env_presence(path: &Path) -> io::Result<(bool, bool)> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_FILE {
        return Err(io::Error::other("environment too large"));
    }
    let mut base = false;
    let mut key = false;
    for line in io::BufReader::new(file.take(MAX_FILE + 1)).lines() {
        let line = line?;
        let trimmed = line
            .trim_start()
            .strip_prefix("export ")
            .unwrap_or(line.trim_start());
        if let Some((name, _)) = trimmed.split_once('=') {
            match name.trim() {
                "GOOGLE_GEMINI_BASE_URL" => base = true,
                "GEMINI_API_KEY" => key = true,
                _ => {}
            }
        }
    }
    Ok((base, key))
}
// Gemini CLI 0.68 findEnvFile: first existing file per layer, independent of key names.
pub(crate) fn find_env_file(
    start: &Path,
    home: Option<&Path>,
    trusted: bool,
    ignore: bool,
) -> io::Result<Option<(PathBuf, bool)>> {
    let mut hit = None;
    for dir in start.ancestors() {
        for (suffix, gemini_env) in [(".gemini/.env", true), (".env", false)] {
            if gemini_env && !trusted {
                continue;
            }
            if !gemini_env && ignore && home != Some(dir) {
                continue;
            }
            let path = dir.join(suffix);
            match std::fs::metadata(&path) {
                Ok(_) => {
                    hit = Some((path, gemini_env));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if hit.is_some() {
            break;
        }
    }
    if hit.is_none() {
        if let Some(home) = home {
            for (suffix, g) in [(".gemini/.env", true), (".env", false)] {
                if g && !trusted {
                    continue;
                }
                let p = home.join(suffix);
                if p.try_exists()? {
                    hit = Some((p, g));
                    break;
                }
            }
        }
    }
    Ok(hit)
}
fn gemini(paths: &AgentsPaths, result: &mut ProjectCheck, home: Option<&Path>) -> io::Result<()> {
    let user = json(&paths.gemini_settings)?.unwrap_or(Value::Null);
    let trust = trust(paths, &result.canonical_path, &user)?;
    let mut env = Environment {
        layer: "none".into(),
        trust: trust.clone(),
        base_key_present: None,
        api_key_present: None,
    };
    let system_path = system_settings()?;
    let defaults_path = system_defaults(&system_path)?;
    let project =
        json(&result.canonical_path.join(".gemini/settings.json"))?.unwrap_or(Value::Null);
    let mut ignore = false;
    for config in [
        json(&defaults_path)?.unwrap_or(Value::Null),
        user,
        if trust == "trusted" {
            project.clone()
        } else {
            Value::Null
        },
        json(&system_path)?.unwrap_or(Value::Null),
    ] {
        if let Some(value) = config
            .pointer("/advanced/ignoreLocalEnv")
            .and_then(Value::as_bool)
        {
            ignore = value;
        }
    }
    let project_override =
        project.pointer("/security/auth").is_some() || project.get("model").is_some();
    if project_override {
        result.state("could_shadow", "project_auth_or_model_override");
    }
    let hit = find_env_file(&result.canonical_path, home, trust == "trusted", ignore)?;
    if let Some((path, g)) = hit {
        result.selected_env = Some(path.clone());
        let dir = if g {
            path.parent().and_then(Path::parent)
        } else {
            path.parent()
        };
        env.layer = format!(
            "{}_{}",
            if dir == home {
                "home"
            } else if dir == Some(result.canonical_path.as_path()) {
                "project"
            } else {
                "ancestor"
            },
            if g { "gemini" } else { "plain" }
        );
        let (base, key) = env_presence(&path)?;
        env.base_key_present = Some(base);
        env.api_key_present = Some(key);
        if !same_file(&path, &paths.gemini_env)
            && !project_override
            && !crate::project_env::matches_managed(paths, &path).unwrap_or(false)
        {
            result.state("could_shadow", "project_environment_shadow");
        }
    }
    if trust != "trusted" {
        result.state(
            "unknown",
            if trust == "untrusted" {
                "project_untrusted"
            } else {
                "project_trust_unknown"
            },
        );
    }
    if trust == "unknown" {
        env.layer = "unknown".into();
        env.base_key_present = None;
        env.api_key_present = None;
    }
    result.env = Some(env);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        paths: AgentsPaths,
        project: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aubridge-project-check-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let home = root.join("home");
        let project = home.join("work/project");
        std::fs::create_dir_all(&project).unwrap();
        for dir in [".codex", ".claude", ".gemini"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let paths = AgentsPaths {
            claude_settings: home.join(".claude/settings.json"),
            codex_config: home.join(".codex/config.toml"),
            opencode_config: home.join(".config/opencode/opencode.json"),
            gemini_settings: home.join(".gemini/settings.json"),
            gemini_env: home.join(".gemini/.env"),
            state_dir: root.join("state"),
        };
        Fixture {
            root,
            paths,
            project,
        }
    }
    #[test]
    fn global_default_provider_is_not_a_project_profile_override() {
        let f = fixture();
        std::fs::write(
            &f.paths.codex_config,
            "model = 'user-default'\nmodel_provider = 'other'\n",
        )
        .unwrap();
        let result = check(&f.paths, "codex", &f.project).unwrap();
        assert!(result.can_launch(), "{result:?}");
        assert!(!result.verified);
        std::fs::create_dir(f.project.join(".codex")).unwrap();
        std::fs::write(
            f.project.join(".codex/config.toml"),
            "[model_providers.aubridge]\nbase_url='https://elsewhere.invalid'\n",
        )
        .unwrap();
        assert_eq!(
            check(&f.paths, "codex", &f.project).unwrap().status,
            "could_shadow"
        );
    }
    #[test]
    fn global_claude_connection_and_unrelated_project_setting_are_not_overrides() {
        let f = fixture();
        std::fs::write(
            &f.paths.claude_settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:1"}}"#,
        )
        .unwrap();
        std::fs::create_dir(f.project.join(".claude")).unwrap();
        let local = f.project.join(".claude/settings.local.json");
        std::fs::write(
            &local,
            r#"{"theme":"dark","env":{"UNRELATED":"must-not-leak"}}"#,
        )
        .unwrap();
        let result = check(&f.paths, "claude-code", &f.project).unwrap();
        assert!(result.can_launch());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("must-not-leak"));
        std::fs::write(
            &local,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"must-not-leak"}}"#,
        )
        .unwrap();
        let result = check(&f.paths, "claude-code", &f.project).unwrap();
        assert_eq!(result.status, "could_shadow");
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("must-not-leak"));
    }
    #[test]
    fn unrelated_dotenv_has_no_managed_keys_but_remains_a_file() {
        let f = fixture();
        let path = f.project.join(".env");
        std::fs::write(&path, "UNRELATED=must-not-leak\nexport SOMETHING=else\n").unwrap();
        assert!(path.try_exists().unwrap());
        assert_eq!(env_presence(&path).unwrap(), (false, false));
        std::fs::write(
            &path,
            "export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1\nGEMINI_API_KEY=must-not-leak\n",
        )
        .unwrap();
        assert_eq!(env_presence(&path).unwrap(), (true, true));
    }
    #[test]
    fn relative_path_does_not_create_a_project_context() {
        let f = fixture();
        assert_eq!(
            check(&f.paths, "gemini", Path::new("project"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    fn fixture_home(f: &Fixture) -> &Path {
        f.paths.gemini_settings.parent().unwrap().parent().unwrap()
    }
    #[test]
    fn unrelated_project_dotenv_shadows_managed_home_env() {
        let f = fixture();
        std::fs::write(
            &f.paths.gemini_env,
            "GOOGLE_GEMINI_BASE_URL=http://127.0.0.1\nGEMINI_API_KEY=fixture-secret\n",
        )
        .unwrap();
        let plain = f.project.join(".env");
        std::fs::write(&plain, "UNRELATED=must-not-leak\n").unwrap();
        let (selected, gemini) = find_env_file(&f.project, Some(fixture_home(&f)), true, false)
            .unwrap()
            .unwrap();
        assert_eq!(selected, plain);
        assert!(!gemini);
        assert_eq!(env_presence(&selected).unwrap(), (false, false));
        assert_ne!(selected, f.paths.gemini_env);
    }
    #[test]
    fn gemini_env_wins_same_layer_and_ignore_only_skips_plain_project_env() {
        let f = fixture();
        std::fs::write(&f.paths.gemini_env, "GEMINI_API_KEY=home\n").unwrap();
        std::fs::write(f.project.join(".env"), "UNRELATED=project\n").unwrap();
        std::fs::create_dir(f.project.join(".gemini")).unwrap();
        let gemini = f.project.join(".gemini/.env");
        std::fs::write(&gemini, "UNRELATED=gemini\n").unwrap();
        for ignore in [false, true] {
            assert_eq!(
                find_env_file(&f.project, Some(fixture_home(&f)), true, ignore).unwrap(),
                Some((gemini.clone(), true))
            );
        }
        std::fs::remove_file(&gemini).unwrap();
        assert_eq!(
            find_env_file(&f.project, Some(fixture_home(&f)), true, true).unwrap(),
            Some((f.paths.gemini_env.clone(), true))
        );
    }
    #[test]
    fn untrusted_loader_skips_gemini_files_and_keeps_plain_home_fallback() {
        let f = fixture();
        let home = fixture_home(&f);
        std::fs::write(&f.paths.gemini_env, "GEMINI_API_KEY=home\n").unwrap();
        std::fs::create_dir(f.project.join(".gemini")).unwrap();
        std::fs::write(f.project.join(".gemini/.env"), "GEMINI_API_KEY=project\n").unwrap();
        let plain = f.project.join(".env");
        std::fs::write(&plain, "UNRELATED=project\n").unwrap();
        let home_plain = home.join(".env");
        std::fs::write(&home_plain, "UNRELATED=home\n").unwrap();
        assert_eq!(
            find_env_file(&f.project, Some(home), false, false).unwrap(),
            Some((plain, false))
        );
        assert_eq!(
            find_env_file(&f.project, Some(home), false, true).unwrap(),
            Some((home_plain, false))
        );
    }
    #[test]
    fn relative_custom_context_configuration_is_not_assumed_safe() {
        assert!(context_path(
            Some("relative/trust.json".into()),
            PathBuf::from("/default/trust.json")
        )
        .is_err());
        assert_eq!(
            context_path(
                Some("/custom/trust.json".into()),
                PathBuf::from("/default/trust.json")
            )
            .unwrap(),
            PathBuf::from("/custom/trust.json")
        );
    }
}

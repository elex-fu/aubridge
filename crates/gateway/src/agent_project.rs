//! Opaque short-lived bindings for user-selected project directories (decision 53).
use crate::state::GatewayState;
use serde::Serialize;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
mod environment;
pub(crate) use environment::{env_apply, env_preview, EnvPreviewResponse};
const TTL: u64 = 600;
#[derive(Default)]
pub(crate) struct ProjectService {
    bindings: Mutex<HashMap<String, Binding>>,
    io: Mutex<()>,
    env_previews: Mutex<HashMap<String, environment::Entry>>,
}
#[derive(Clone)]
struct Binding {
    agent: String,
    path: PathBuf,
    selected_path: PathBuf,
    alias: String,
    identity: String,
    directory_identity: String,
    expires: u64,
}
#[derive(Serialize)]
pub(crate) struct CheckResponse {
    pub project_handle: String,
    pub project_alias: String,
    #[serde(flatten)]
    pub check: agents::project_check::ProjectCheck,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn hash_configuration(
    file: &Path,
    hash: &mut std::collections::hash_map::DefaultHasher,
) -> Result<(), String> {
    file.hash(hash);
    match std::fs::symlink_metadata(file) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            0u8.hash(hash);
            return Ok(());
        }
        Err(_) => return Err("project_configuration_unreadable".into()),
    }
    let opened = std::fs::File::open(file).map_err(|_| "project_configuration_unreadable")?;
    let metadata = opened
        .metadata()
        .map_err(|_| "project_configuration_unreadable")?;
    if !metadata.is_file() || metadata.len() > 1_048_576 {
        return Err("project_configuration_unreadable".into());
    }
    let canonical = std::fs::canonicalize(file).map_err(|_| "project_configuration_unreadable")?;
    let mut bytes = Vec::new();
    opened
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| "project_configuration_unreadable")?;
    if bytes.len() > 1_048_576 {
        return Err("project_configuration_unreadable".into());
    }
    1u8.hash(hash);
    canonical.hash(hash);
    bytes.hash(hash);
    Ok(())
}
fn directory_identity(path: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(path).map_err(|_| "project_changed")?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| "project_changed")?;
    if !metadata.is_dir() {
        return Err("project_changed".into());
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(&mut hash);
        metadata.ino().hash(&mut hash);
    }
    #[cfg(not(unix))]
    metadata.created().ok().hash(&mut hash);
    Ok(format!("{:016x}", hash.finish()))
}
fn identity(state: &GatewayState, agent: &str, path: &Path) -> Result<String, String> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let canonical = std::fs::canonicalize(path).map_err(|_| "project_changed")?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| "project_changed")?;
    canonical.hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(&mut hash);
        metadata.ino().hash(&mut hash);
    }
    #[cfg(not(unix))]
    metadata.created().ok().hash(&mut hash);
    state.validation_routing_identity().hash(&mut hash);
    // Fingerprint only configuration files, never project source content.
    let relatives: &[&str] = match agent {
        "gemini" => &[".env", ".gemini/.env", ".gemini/settings.json"],
        "claude-code" => &[".claude/settings.json", ".claude/settings.local.json"],
        "codex" => &[".codex/config.toml"],
        "opencode" => &[
            "opencode.json",
            "opencode.jsonc",
            ".opencode/opencode.json",
            ".opencode/opencode.jsonc",
        ],
        _ => return Err("unknown_agent".into()),
    };
    for dir in canonical.ancestors() {
        for relative in relatives {
            hash_configuration(&dir.join(relative), &mut hash)?;
        }
    }
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let mut files = match agent {
        "claude-code" => vec![paths.claude_settings.clone()],
        "codex" => vec![
            paths.codex_config.clone(),
            paths.codex_config.with_file_name("aubridge.config.toml"),
        ],
        "opencode" => vec![paths.opencode_config.clone()],
        "gemini" => vec![paths.gemini_settings.clone(), paths.gemini_env.clone()],
        _ => return Err("unknown_agent".into()),
    };
    if agent == "gemini" {
        files.extend(
            agents::project_check::gemini_context_files(paths)
                .map_err(|_| "project_context_unknown")?,
        );
        for key in [
            "HOME",
            "GEMINI_CLI_HOME",
            "GEMINI_CLI_TRUSTED_FOLDERS_PATH",
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
            "GEMINI_CLI_SYSTEM_DEFAULTS_PATH",
            "GEMINI_RESTRICTED_MODE",
            "GEMINI_CLI_TRUST_WORKSPACE",
            "GEMINI_CLI_IDE_SERVER_PORT",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENAI_USE_VERTEXAI",
        ] {
            key.hash(&mut hash);
            std::env::var_os(key).hash(&mut hash);
        }
    }
    for file in files {
        hash_configuration(&file, &mut hash)?;
    }
    if let Some(name) = agents::readiness::program_name(agent) {
        let program = agents::readiness::resolve_program(agent, name).ok();
        program.hash(&mut hash);
        if let Some(program) = program {
            if let Ok(meta) = std::fs::metadata(program) {
                meta.len().hash(&mut hash);
                meta.modified().ok().hash(&mut hash);
            }
        }
    }
    agent.hash(&mut hash);
    state.port().hash(&mut hash);
    Ok(format!("{:016x}", hash.finish()))
}
pub(crate) fn check(
    state: &GatewayState,
    agent: &str,
    path: &str,
) -> Result<CheckResponse, String> {
    let _guard = state
        .agent_projects
        .io
        .lock()
        .map_err(|_| "project_check_unavailable")?;
    check_inner(state, agent, path)
}
fn check_inner(state: &GatewayState, agent: &str, path: &str) -> Result<CheckResponse, String> {
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let mut check = agents::project_check::check(paths, agent, Path::new(path))
        .map_err(|_| "invalid_project_directory")?;
    let identity = match identity(state, agent, &check.canonical_path) {
        Ok(identity) => identity,
        Err(_) => {
            check.status = "unreadable".into();
            check.reason = Some("project_configuration_unreadable".into());
            String::new()
        }
    };
    if agent == "gemini" {
        if check.env.as_ref().is_some_and(|env| {
            env.trust == "trusted"
                && matches!(env.layer.as_str(), "project_plain" | "project_gemini")
        }) && check.reason.as_deref() != Some("project_auth_or_model_override")
        {
            if let Some(target) = check.selected_env.as_deref() {
                if agents::project_env::preview_add(paths, &check.canonical_path, target)
                    .is_ok_and(|plan| plan.changes.iter().any(|change| change.action == "add"))
                {
                    check.actions.push("preview_project_env_add".into());
                }
            }
        }
        if !identity.is_empty()
            && check.env.as_ref().is_some_and(|env| env.trust == "trusted")
            && check.reason.as_deref() != Some("project_auth_or_model_override")
            && agents::project_env::preview_undo(paths, &check.canonical_path)
                .is_ok_and(|plan| check.selected_env.as_deref() == Some(plan.target()))
        {
            check.actions.push("preview_project_env_undo".into());
        }
    }
    let directory_identity = directory_identity(&check.canonical_path)?;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "random_unavailable")?;
    let handle = bytes.iter().map(|v| format!("{v:02x}")).collect::<String>();
    let alias = check
        .canonical_path
        .file_name()
        .map(|s| {
            s.to_string_lossy()
                .chars()
                .filter(|c| !c.is_control())
                .take(64)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "项目".into());
    let mut bindings = state
        .agent_projects
        .bindings
        .lock()
        .map_err(|_| "project_check_unavailable")?;
    bindings.retain(|_, b| b.expires > now());
    if bindings.len() >= 64 {
        return Err("project_check_capacity".into());
    }
    bindings.insert(
        handle.clone(),
        Binding {
            agent: agent.into(),
            path: check.canonical_path.clone(),
            selected_path: PathBuf::from(path),
            alias: alias.clone(),
            identity,
            directory_identity,
            expires: now() + TTL,
        },
    );
    Ok(CheckResponse {
        project_handle: handle,
        project_alias: alias,
        check,
    })
}
fn binding(
    state: &GatewayState,
    agent: &str,
    handle: &str,
    require_context: bool,
) -> Result<Binding, String> {
    let binding = state
        .agent_projects
        .bindings
        .lock()
        .map_err(|_| "project_check_unavailable")?
        .get(handle)
        .cloned()
        .ok_or("project_handle_unknown")?;
    if binding.agent != agent {
        return Err("project_handle_mismatch".into());
    }
    if binding.expires <= now() {
        return Err("project_handle_expired".into());
    }
    if std::fs::canonicalize(&binding.selected_path).map_err(|_| "project_changed")? != binding.path
        || directory_identity(&binding.path)? != binding.directory_identity
    {
        return Err("project_changed".into());
    }
    if require_context && identity(state, agent, &binding.path)? != binding.identity {
        return Err("project_changed".into());
    }
    Ok(binding)
}
pub(crate) fn resolve(
    state: &GatewayState,
    agent: &str,
    handle: &str,
) -> Result<(PathBuf, String), String> {
    let binding = binding(state, agent, handle, true)?;
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let check =
        agents::project_check::check(paths, agent, &binding.path).map_err(|_| "project_changed")?;
    if !check.can_launch() {
        return Err("project_context_conflict".into());
    }
    Ok((binding.path, binding.alias))
}
pub(crate) fn valid(state: &GatewayState, agent: &str, handle: &str) -> bool {
    resolve(state, agent, handle).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "project-context-fingerprint-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Fixture(path)
    }
    fn digest(path: &Path) -> Result<u64, String> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        hash_configuration(path, &mut hash)?;
        Ok(hash.finish())
    }
    #[test]
    fn custom_trust_and_system_changes_invalidate_even_when_metadata_matches() {
        let root = fixture();
        for (name, before, after) in [
            (
                "custom-trust.json",
                r#"{"directory":"TRUST_FOLDER"}"#,
                r#"{"directory":"DO_NOT_TRUST"}"#,
            ),
            (
                "system-defaults.json",
                r#"{"advanced":{"ignoreLocalEnv":false}}"#,
                r#"{"advanced":{"ignoreLocalEnv":true }}"#,
            ),
        ] {
            assert_eq!(before.len(), after.len());
            let path = root.0.join(name);
            std::fs::write(&path, before).unwrap();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let original = digest(&path).unwrap();
            std::fs::write(&path, after).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                modified
            );
            assert_ne!(digest(&path).unwrap(), original);
        }
    }
    #[test]
    fn unreadable_configuration_is_not_hashed_as_an_absent_file() {
        let root = fixture();
        let path = root.0.join("custom-trust.json");
        assert!(digest(&path).is_ok());
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            digest(&path).unwrap_err(),
            "project_configuration_unreadable"
        );
    }
    #[cfg(unix)]
    #[test]
    fn dangling_custom_configuration_symlink_fails_closed() {
        let root = fixture();
        let path = root.0.join("custom-trust.json");
        std::os::unix::fs::symlink(root.0.join("missing.json"), &path).unwrap();
        assert_eq!(
            digest(&path).unwrap_err(),
            "project_configuration_unreadable"
        );
    }
}

//! One-shot project environment previews; no environment values leave the backend.
use super::*;
use agents::project_env::{Change, Plan};
#[derive(Clone)]
pub(super) struct Entry {
    project_handle: String,
    project: PathBuf,
    context_identity: String,
    plan: Plan,
    operation: String,
    expires: u64,
}
#[derive(Serialize)]
pub(crate) struct EnvPreviewResponse {
    pub preview_handle: String,
    pub project_alias: String,
    pub layer: String,
    pub operation: String,
    pub changes: Vec<Change>,
    pub expires_at: u64,
}
fn env_error(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::NotFound => "project_env_conflict",
        _ => "project_env_io",
    }
    .into()
}
fn current_scope(state: &GatewayState, project: &Path, plan: &Plan) -> Result<String, String> {
    let paths = state.agents_paths().ok_or("project_env_conflict")?;
    let current = agents::project_check::check(paths, "gemini", project).map_err(env_error)?;
    if !current
        .env
        .as_ref()
        .is_some_and(|env| env.trust == "trusted")
        || current.selected_env.as_deref() != Some(plan.target())
        || current.reason.as_deref() == Some("project_auth_or_model_override")
    {
        return Err("project_env_conflict".into());
    }
    Ok(current.env.ok_or("project_env_conflict")?.layer)
}
pub(crate) fn env_preview(
    state: &GatewayState,
    project_handle: &str,
    operation: &str,
) -> Result<EnvPreviewResponse, String> {
    if !matches!(operation, "add" | "undo") {
        return Err("project_env_invalid_operation".into());
    }
    let _guard = state
        .agent_projects
        .io
        .lock()
        .map_err(|_| "project_env_io")?;
    let binding = super::binding(state, "gemini", project_handle, operation == "add")?;
    let paths = state.agents_paths().ok_or("project_env_conflict")?;
    let plan = if operation == "add" {
        if !agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            state.port(),
        )
        .wired()
        {
            return Err("project_env_conflict".into());
        }
        let check =
            agents::project_check::check(paths, "gemini", &binding.path).map_err(env_error)?;
        if !check.env.as_ref().is_some_and(|env| {
            env.trust == "trusted"
                && matches!(env.layer.as_str(), "project_plain" | "project_gemini")
        }) || check.reason.as_deref() == Some("project_auth_or_model_override")
        {
            return Err("project_env_conflict".into());
        }
        agents::project_env::preview_add(
            paths,
            &binding.path,
            check
                .selected_env
                .as_deref()
                .ok_or("project_env_conflict")?,
        )
        .map_err(env_error)?
    } else {
        agents::project_env::preview_undo(paths, &binding.path).map_err(env_error)?
    };
    let context_identity =
        identity(state, "gemini", &binding.path).map_err(|_| "project_env_conflict")?;
    let layer = current_scope(state, &binding.path, &plan)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "project_env_io")?;
    let handle = random
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let expires = (now() + TTL).min(binding.expires);
    let response = EnvPreviewResponse {
        preview_handle: handle.clone(),
        project_alias: binding.alias,
        layer,
        operation: operation.into(),
        changes: plan.changes.clone(),
        expires_at: expires,
    };
    let mut previews = state
        .agent_projects
        .env_previews
        .lock()
        .map_err(|_| "project_env_io")?;
    previews.retain(|_, entry| entry.expires > now());
    if previews.len() >= 64 {
        return Err("project_env_capacity".into());
    }
    previews.insert(
        handle,
        Entry {
            project_handle: project_handle.into(),
            project: binding.path.clone(),
            context_identity,
            plan,
            operation: operation.into(),
            expires,
        },
    );
    Ok(response)
}
pub(crate) fn env_apply(
    state: &GatewayState,
    preview_handle: &str,
    confirmed: bool,
) -> Result<CheckResponse, String> {
    if !confirmed {
        return Err("project_env_confirmation_required".into());
    }
    let _activity = state.begin_update_activity().map_err(str::to_owned)?;
    let _guard = state
        .agent_projects
        .io
        .lock()
        .map_err(|_| "project_env_io")?;
    let entry = state
        .agent_projects
        .env_previews
        .lock()
        .map_err(|_| "project_env_io")?
        .remove(preview_handle)
        .ok_or("project_env_preview_unknown")?;
    if entry.expires <= now() {
        return Err("project_env_preview_expired".into());
    }
    let binding = super::binding(
        state,
        "gemini",
        &entry.project_handle,
        entry.operation == "add",
    )?;
    if binding.path != entry.project
        || identity(state, "gemini", &binding.path).map_err(|_| "project_env_changed")?
            != entry.context_identity
    {
        return Err("project_env_changed".into());
    }
    let paths = state.agents_paths().ok_or("project_env_conflict")?;
    current_scope(state, &binding.path, &entry.plan)?;
    if entry.operation == "add"
        && !agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            state.port(),
        )
        .wired()
    {
        return Err("project_env_changed".into());
    }
    agents::project_env::apply(paths, &entry.plan).map_err(env_error)?;
    state
        .agent_projects
        .bindings
        .lock()
        .map_err(|_| "project_env_io")?
        .retain(|_, binding| binding.path != entry.project);
    state
        .agent_projects
        .env_previews
        .lock()
        .map_err(|_| "project_env_io")?
        .retain(|_, preview| preview.project != entry.project);
    let selected = binding
        .selected_path
        .to_str()
        .ok_or("project_env_conflict")?;
    super::check_inner(state, "gemini", selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Fixture {
        root: PathBuf,
        project: PathBuf,
        target: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn paths(&self) -> agents::AgentsPaths {
            agents::AgentsPaths {
                claude_settings: self.root.join("claude/settings.json"),
                codex_config: self.root.join("codex/config.toml"),
                opencode_config: self.root.join("opencode/opencode.json"),
                gemini_settings: self.root.join("home/.gemini/settings.json"),
                gemini_env: self.root.join("home/.gemini/.env"),
                state_dir: self.root.join("state"),
            }
        }
        fn state(&self, port: u16) -> GatewayState {
            let state = GatewayState::from_manifests(std::collections::BTreeMap::new(), "test")
                .with_agents(self.paths());
            state.set_port(port);
            state
        }
    }
    fn fixture() -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "project-env-api-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let target = project.join(".env");
        std::fs::write(&target, b"UNRELATED=before\n").unwrap();
        let fixture = Fixture {
            root,
            project,
            target,
        };
        let paths = fixture.paths();
        std::fs::create_dir_all(paths.gemini_settings.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.gemini_settings,
            r#"{"security":{"folderTrust":{"enabled":false}}}"#,
        )
        .unwrap();
        agents::gemini::GeminiCli::new(
            paths.gemini_settings,
            paths.gemini_env,
            paths.state_dir,
            3427,
        )
        .apply()
        .unwrap();
        fixture
    }
    fn inspect(state: &GatewayState, fixture: &Fixture) -> CheckResponse {
        super::super::check(state, "gemini", fixture.project.to_str().unwrap()).unwrap()
    }
    #[test]
    fn add_and_undo_issue_fresh_handles_and_never_revive_an_old_handle() {
        let f = fixture();
        let state = f.state(3427);
        let original = std::fs::read(&f.target).unwrap();
        let before = inspect(&state, &f);
        assert!(before
            .check
            .actions
            .iter()
            .any(|action| action == "preview_project_env_add"));
        assert!(!before.check.verified);
        let add = env_preview(&state, &before.project_handle, "add").unwrap();
        let json = serde_json::to_string(&add).unwrap();
        assert!(!json.contains("127.0.0.1") && !json.contains("aubridge-gemini"));
        let after = env_apply(&state, &add.preview_handle, true).unwrap();
        assert_ne!(after.project_handle, before.project_handle);
        assert_eq!(after.check.status, "matched");
        assert!(!after.check.verified);
        assert!(super::super::resolve(&state, "gemini", &before.project_handle).is_err());
        let undo = env_preview(&state, &after.project_handle, "undo").unwrap();
        let restored = env_apply(&state, &undo.preview_handle, true).unwrap();
        assert!(!restored.check.verified);
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
        for old in [&before.project_handle, &after.project_handle] {
            assert!(super::super::resolve(&state, "gemini", old).is_err());
        }
        assert!(env_apply(&state, &add.preview_handle, true).is_err());
        assert!(env_apply(&state, &undo.preview_handle, true).is_err());
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
    }
    #[test]
    fn undo_after_restart_unwired_and_new_port_preserves_unrelated_change() {
        let f = fixture();
        let state = f.state(3427);
        let before = inspect(&state, &f);
        let add = env_preview(&state, &before.project_handle, "add").unwrap();
        env_apply(&state, &add.preview_handle, true).unwrap();
        drop(state);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.target)
            .unwrap()
            .write_all(b"AFTER=user-edit\n")
            .unwrap();
        let paths = f.paths();
        agents::gemini::GeminiCli::new(
            paths.gemini_settings,
            paths.gemini_env,
            paths.state_dir,
            3427,
        )
        .disconnect()
        .unwrap();
        let restarted = f.state(4971);
        let current = inspect(&restarted, &f);
        assert!(current
            .check
            .actions
            .iter()
            .any(|action| action == "preview_project_env_undo"));
        let undo = env_preview(&restarted, &current.project_handle, "undo").unwrap();
        let restored = env_apply(&restarted, &undo.preview_handle, true).unwrap();
        assert_eq!(
            std::fs::read(&f.target).unwrap(),
            b"UNRELATED=before\nAFTER=user-edit\n"
        );
        assert!(!restored.check.verified);
        assert!(super::super::resolve(&restarted, "gemini", &restored.project_handle).is_err());
    }
    #[test]
    fn preview_drift_rejects_file_loader_and_trust_changes_without_writing() {
        for change in ["file", "loader", "trust"] {
            let f = fixture();
            let state = f.state(3427);
            let before = inspect(&state, &f);
            let preview = env_preview(&state, &before.project_handle, "add").unwrap();
            match change {
                "file" => std::fs::OpenOptions::new()
                    .append(true)
                    .open(&f.target)
                    .unwrap()
                    .write_all(b"AFTER=external\n")
                    .unwrap(),
                "loader" => {
                    std::fs::create_dir(f.project.join(".gemini")).unwrap();
                    std::fs::write(f.project.join(".gemini/.env"), b"OTHER=layer\n").unwrap();
                }
                _ => {
                    let paths = f.paths();
                    let mut config: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&paths.gemini_settings).unwrap())
                            .unwrap();
                    config["security"]["folderTrust"]["enabled"] = serde_json::json!(true);
                    std::fs::write(&paths.gemini_settings, serde_json::to_vec(&config).unwrap())
                        .unwrap();
                }
            }
            let external = std::fs::read(&f.target).unwrap();
            assert!(env_apply(&state, &preview.preview_handle, true).is_err());
            assert_eq!(std::fs::read(&f.target).unwrap(), external);
        }
    }
    #[test]
    fn confirmation_is_required_and_each_preview_is_consumed_once() {
        let f = fixture();
        let state = f.state(3427);
        let check = inspect(&state, &f);
        let preview = env_preview(&state, &check.project_handle, "add").unwrap();
        let original = std::fs::read(&f.target).unwrap();
        assert_eq!(
            env_apply(&state, &preview.preview_handle, false)
                .err()
                .as_deref(),
            Some("project_env_confirmation_required")
        );
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
        env_apply(&state, &preview.preview_handle, true).unwrap();
        let added = std::fs::read(&f.target).unwrap();
        assert!(env_apply(&state, &preview.preview_handle, true).is_err());
        assert_eq!(std::fs::read(&f.target).unwrap(), added);
    }
    #[test]
    fn undo_preview_requires_current_trusted_first_hit_and_determinate_context() {
        for change in ["trust", "first_hit", "unknown_context"] {
            let f = fixture();
            let state = f.state(3427);
            let before = inspect(&state, &f);
            let preview = env_preview(&state, &before.project_handle, "add").unwrap();
            env_apply(&state, &preview.preview_handle, true).unwrap();
            match change {
                "trust" => {
                    let paths = f.paths();
                    let mut config: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&paths.gemini_settings).unwrap())
                            .unwrap();
                    config["security"]["folderTrust"]["enabled"] = serde_json::json!(true);
                    std::fs::write(&paths.gemini_settings, serde_json::to_vec(&config).unwrap())
                        .unwrap();
                }
                "first_hit" => {
                    std::fs::create_dir(f.project.join(".gemini")).unwrap();
                    std::fs::write(f.project.join(".gemini/.env"), b"OTHER=higher-priority\n")
                        .unwrap();
                }
                _ => {
                    let paths = f.paths();
                    std::fs::remove_file(&paths.gemini_env).unwrap();
                    std::fs::create_dir(&paths.gemini_env).unwrap();
                }
            }
            let current = inspect(&state, &f);
            let bytes = std::fs::read(&f.target).unwrap();
            assert!(
                env_preview(&state, &current.project_handle, "undo").is_err(),
                "unsafe undo scope: {change}"
            );
            assert_eq!(std::fs::read(&f.target).unwrap(), bytes);
        }
    }
}

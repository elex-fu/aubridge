//! Private probe copies preserve managed routing without loading user hooks or plugins.
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};

pub struct GeminiProbeLayout {
    pub home: PathBuf,
    pub work: PathBuf,
}

/// Gemini searches cwd ancestors before its HOME fallback, even for unrelated .env keys.
pub fn gemini_layout(root: &Path) -> io::Result<GeminiProbeLayout> {
    let home = root.join("home");
    let work = home.join("work");
    for path in [&home, &work] {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("private Gemini directory replaced"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private Gemini directory permissions",
                ));
            }
        }
    }
    Ok(GeminiProbeLayout { home, work })
}
pub fn opencode_provider(path: &Path, session: &str) -> io::Result<Value> {
    let text = std::fs::read_to_string(path)?;
    let document: Value = jsonc_parser::parse_to_serde_value::<serde_json::Value>(
        &text,
        &jsonc_parser::ParseOptions::default(),
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    let mut provider = document
        .pointer("/provider/aubridge")
        .cloned()
        .ok_or_else(|| io::Error::other("missing managed provider"))?;
    provider["options"]["headers"]["x-session-id"] = json!(session);
    Ok(json!({"provider":{"aubridge":provider}}))
}
pub fn claude_settings(path: &Path) -> io::Result<Value> {
    let document: Value =
        serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    let mut env = serde_json::Map::new();
    for key in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
    ] {
        if let Some(value) = document["env"].get(key) {
            env.insert(key.into(), value.clone());
        }
    }
    let mut probe = json!({"env":env});
    if let Some(model) = document.get("model") {
        let model = model
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Claude model must be a nonempty string",
                )
            })?;
        probe["model"] = json!(model);
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn gemini_private_work_finds_managed_env_before_unrelated_ancestor() {
        let (_fixture, path) = boundary_fixture();
        let repository = path.parent().unwrap().join("repository");
        let job = repository.join("jobs/probe");
        std::fs::create_dir_all(&job).unwrap();
        let ancestor_env = repository.join(".env");
        let unrelated = b"UNRELATED_FIXTURE=present\n";
        std::fs::write(&ancestor_env, unrelated).unwrap();
        let layout = gemini_layout(&job).unwrap();
        let config = layout.home.join(".gemini");
        std::fs::create_dir(&config).unwrap();
        let managed_env = config.join(".env");
        std::fs::write(
            &managed_env,
            "GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3427\nGEMINI_API_KEY=aubridge-gemini\n",
        )
        .unwrap();
        let find = crate::project_check::find_env_file;
        assert_eq!(
            find(&job, Some(&layout.home), true, false).unwrap(),
            Some((ancestor_env.clone(), false))
        );
        assert_eq!(
            find(&layout.work, Some(&layout.home), true, false).unwrap(),
            Some((managed_env, true))
        );
        assert_ne!(
            layout.work, layout.home,
            "avoid the official loader's cwd=HOME special case"
        );
        std::fs::write(layout.work.join("challenge.txt"), "fixture-nonce").unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.work.join("challenge.txt")).unwrap(),
            "fixture-nonce"
        );
        assert!(!job.join("challenge.txt").exists());
        assert_eq!(std::fs::read(ancestor_env).unwrap(), unrelated);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [&layout.home, &layout.work] {
                assert_eq!(
                    std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        let again = gemini_layout(&job).unwrap();
        assert_eq!(again.work, layout.work);
    }
    #[test]
    fn claude_probe_preserves_selected_top_level_model_without_hooks_or_external_env() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "claude-probe-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let _fixture = Fixture(directory.clone());
        let path = directory.join("settings.json");
        // Derived from the real fresh wiring + set_model shape: env lacks ANTHROPIC_MODEL.
        let settings = json!({
            "model":"gpt-6.1-sol",
            "env":{
                "ANTHROPIC_BASE_URL":"http://127.0.0.1:12345",
                "ANTHROPIC_AUTH_TOKEN":"aubridge-claude-code",
                "ANTHROPIC_API_KEY":"external-fixture-secret",
                "UNRELATED":"other-fixture-secret"
            },
            "hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"fixture-must-not-run"}]}]},
            "enabledPlugins":{"fixture-external-plugin":true},
            "permissions":{"allow":["Bash"]},
            "unrelated":{"retain_only_in_source":true}
        });
        let original = serde_json::to_vec(&settings).unwrap();
        std::fs::write(&path, &original).unwrap();
        let probe = claude_settings(&path).unwrap();
        assert_eq!(
            probe["env"]["ANTHROPIC_BASE_URL"],
            settings["env"]["ANTHROPIC_BASE_URL"]
        );
        assert_eq!(
            probe["env"]["ANTHROPIC_AUTH_TOKEN"],
            settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        );
        assert!(probe["env"].get("ANTHROPIC_API_KEY").is_none());
        assert!(probe["env"].get("UNRELATED").is_none());
        for excluded in ["hooks", "enabledPlugins", "permissions", "unrelated"] {
            assert!(probe.get(excluded).is_none());
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            probe["model"], settings["model"],
            "the private probe must retain the model selected in the product"
        );
    }
    fn boundary_fixture() -> (Fixture, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "claude-probe-model-boundary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        (Fixture(directory), path)
    }
    #[test]
    fn claude_probe_keeps_missing_model_and_preserves_nonempty_model_bytes() {
        let (_fixture, path) = boundary_fixture();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"env":{"ANTHROPIC_MODEL":"existing-env-model"}})).unwrap(),
        )
        .unwrap();
        let probe = claude_settings(&path).unwrap();
        assert!(probe.get("model").is_none());
        assert_eq!(probe["env"]["ANTHROPIC_MODEL"], "existing-env-model");
        let selected = "  custom-provider/model-name\t";
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"model":selected,"env":{}})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            claude_settings(&path).unwrap()["model"].as_str(),
            Some(selected)
        );
    }
    #[test]
    fn claude_probe_rejects_invalid_present_model_instead_of_falling_back() {
        let (_fixture, path) = boundary_fixture();
        for model in [
            Value::Null,
            json!(false),
            json!(42),
            json!(["model"]),
            json!({"name":"model"}),
            json!(""),
            json!(" \t\n"),
        ] {
            let original = serde_json::to_vec(
                &json!({"model":model,"env":{"ANTHROPIC_MODEL":"existing-env-model"}}),
            )
            .unwrap();
            std::fs::write(&path, &original).unwrap();
            assert_eq!(
                claude_settings(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
}

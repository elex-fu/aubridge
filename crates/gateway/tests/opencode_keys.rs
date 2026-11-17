//! This dedicated test binary owns its environment and never reads user auth paths.
use std::collections::BTreeMap;
use std::sync::Arc;

#[test]
fn saved_then_environment_then_opencode_without_writes_or_secret_admin_fields() {
    let original_env = std::env::var_os("KEY_OPENROUTER");
    struct Restore(Option<std::ffi::OsString>, std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("KEY_OPENROUTER", value),
                None => std::env::remove_var("KEY_OPENROUTER"),
            }
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let dir = std::env::temp_dir().join(format!(
        "aubridge-opencode-api-priority-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let _restore = Restore(original_env, dir.clone());
    std::env::remove_var("KEY_OPENROUTER");
    let path = dir.join("auth.json");
    let contents = r#"{"openrouter":{"type":"api","key":"synthetic-import"},"openai":{"type":"oauth","access":"synthetic-oauth"},"unknown":false}"#;
    std::fs::write(&path, contents).unwrap();
    let m = registry::openrouter();
    let state =
        gateway::state::GatewayState::from_manifests(BTreeMap::from([(m.id.clone(), m)]), "test")
            .with_opencode_api_auth(path.clone())
            .with_key_store(Arc::new(credentials::MemoryStore::default()));
    assert_eq!(
        state.api_key_opt("openrouter").as_deref(),
        Some("synthetic-import")
    );
    assert_eq!(state.providers_overview()[0]["key_source"], "opencode");
    std::env::set_var("KEY_OPENROUTER", "synthetic-env");
    assert_eq!(
        state.api_key_opt("openrouter").as_deref(),
        Some("synthetic-env")
    );
    assert_eq!(state.providers_overview()[0]["key_source"], "environment");
    state
        .set_provider_key("openrouter", "synthetic-saved")
        .unwrap();
    assert_eq!(
        state.api_key_opt("openrouter").as_deref(),
        Some("synthetic-saved")
    );
    assert_eq!(state.providers_overview()[0]["key_source"], "saved");
    state.clear_provider_key("openrouter").unwrap();
    assert_eq!(state.providers_overview()[0]["key_source"], "environment");
    // Preserve the existing empty-env semantics: it blocks import even though it
    // cannot authorize a request. A rejected high-priority key is never replaced.
    std::env::set_var("KEY_OPENROUTER", "");
    assert_eq!(state.api_key_opt("openrouter").as_deref(), Some(""));
    assert_eq!(state.providers_overview()[0]["ready"], false);
    std::env::remove_var("KEY_OPENROUTER");
    assert_eq!(state.providers_overview()[0]["key_source"], "opencode");
    let admin = serde_json::to_string(&state.providers_overview()).unwrap();
    assert!(!admin.contains("synthetic-import"));
    assert!(!admin.contains("synthetic-oauth"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
}

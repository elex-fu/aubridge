//! Narrow credential boundary for the read-only OpenCode API fallback.
use manifest::schema::Manifest;

pub(super) fn official_target(manifest: &Manifest) -> bool {
    let official = registry::openrouter();
    manifest.id == official.id
        && manifest.kind == official.kind
        && manifest.provider == official.provider
        && manifest.permissions == official.permissions
}

#[cfg(test)]
mod tests {
    use super::super::GatewayState;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // These tests must run without an injected high-priority OpenRouter key.
            assert!(
                std::env::var_os("KEY_OPENROUTER").is_none(),
                "run with KEY_OPENROUTER unset"
            );
            let dir = std::env::temp_dir().join(format!(
                "aubridge-oc-key-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.join("auth.json"))
        }
        fn key(&self, key: &str) {
            std::fs::write(&self.0, serde_json::json!({"openrouter":{"type":"api","key":key}, "openai":{"type":"oauth","access":"synthetic-oauth"}}).to_string()).unwrap();
        }
        fn state(&self) -> GatewayState {
            let m = registry::openrouter();
            GatewayState::from_manifests(BTreeMap::from([(m.id.clone(), m)]), "test")
                .with_opencode_api_auth(self.0.clone())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    fn version(state: &GatewayState) -> super::super::ConnectionVersion {
        state.connection_versions.read().unwrap()["openrouter"].clone()
    }
    fn plan(state: &GatewayState) -> crate::route_plan::Plan {
        let req = serde_json::from_value(serde_json::json!({"model":registry::openrouter().free_models[0],"max_tokens":10,"messages":[{"role":"user","content":"test"}]})).unwrap();
        crate::route_plan::plan(
            state,
            &req,
            &axum::http::HeaderMap::new(),
            100,
            false,
            false,
            |_, _| None,
        )
    }

    #[test]
    fn opencode_keys_rotation_precedes_plan_and_late_rejection_cannot_freeze_new_key() {
        let f = Fixture::new();
        f.key("synthetic-old");
        let state = f.state();
        let manifest = registry::openrouter();
        let (_, old) = state.dispatch_credential(&manifest).unwrap();
        state.set_key_retry_at("openrouter", Some(500));
        assert!(plan(&state).ordered.is_empty());
        f.key("synthetic-new");
        assert_eq!(plan(&state).ordered.len(), 1);
        let (credential, new) = state.dispatch_credential(&manifest).unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(
            matches!(credential, crate::route_plan::Credential::Key(key) if key == "synthetic-new")
        );
        state.update_current_connection("openrouter", Some(&old), || {
            state.set_key_retry_at("openrouter", Some(700))
        });
        assert_eq!(state.snapshot()[0].key_retry_at, None);
        assert_eq!(state.providers_overview()[0]["key_source"], "opencode");
    }

    #[test]
    fn opencode_keys_baseline_same_key_and_irrelevant_changes_preserve_cooldown() {
        let f = Fixture::new();
        f.key("synthetic");
        let state = f.state();
        state.set_key_retry_at("openrouter", Some(500));
        let before = version(&state);
        assert!(plan(&state).ordered.is_empty()); // first observation establishes baseline
        assert!(Arc::ptr_eq(&before, &version(&state)));
        std::fs::write(
            &f.0,
            r#"{"unrelated":{"broken":"schema"},"openrouter":{"type":"api","key":"synthetic"}}"#,
        )
        .unwrap();
        assert!(plan(&state).ordered.is_empty());
        assert_eq!(state.snapshot()[0].key_retry_at, Some(500));
        assert!(Arc::ptr_eq(&before, &version(&state)));
    }

    #[test]
    fn opencode_keys_revocation_corruption_and_recovery_never_reuse_old_key() {
        let f = Fixture::new();
        f.key("synthetic");
        let state = f.state();
        let m = registry::openrouter();
        let (_, first) = state.dispatch_credential(&m).unwrap();
        std::fs::remove_file(&f.0).unwrap();
        assert!(state.dispatch_credential(&m).is_err());
        assert!(state.api_key_opt("openrouter").is_none());
        assert!(!Arc::ptr_eq(&first, &version(&state)));
        f.key("synthetic-restored");
        assert!(state.dispatch_credential(&m).is_ok());
        std::fs::write(&f.0, "broken-json").unwrap();
        assert!(state.dispatch_credential(&m).is_err());
        assert!(!state.key_set("openrouter"));
        std::fs::write(
            &f.0,
            r#"{"openrouter":{"type":"oauth","access":"not-an-api-key"}}"#,
        )
        .unwrap();
        assert!(state.dispatch_credential(&m).is_err());
        f.key("synthetic-final");
        let bytes = std::fs::read(&f.0).unwrap();
        assert!(state.dispatch_credential(&m).is_ok());
        assert_eq!(std::fs::read(&f.0).unwrap(), bytes);
    }

    #[test]
    fn opencode_keys_rotation_only_clears_key_cooldown() {
        let f = Fixture::new();
        f.key("synthetic-old");
        let state = f.state();
        state.reconcile_external_api_key();
        {
            let mut sources = state.providers.write().unwrap();
            let source = sources.get_mut("openrouter").unwrap();
            source.key_retry_at = Some(500);
            source.rest_until = Some(700);
            source.remaining = Some(0);
            source.breaker.open_until = Some(800);
            source.model_cooldowns.insert(
                source.models[0].clone(),
                manifest::state::ModelCooldown {
                    until: 900,
                    reason: manifest::state::ModelCooldownReason::ModelRateLimit,
                },
            );
        }
        state
            .paused_providers
            .write()
            .unwrap()
            .insert("openrouter".into());
        let mut expected = state.snapshot()[0].clone();
        expected.key_retry_at = None;
        f.key("synthetic-new");
        assert!(plan(&state).ordered.is_empty());
        assert_eq!(state.snapshot()[0], expected);
        assert!(state.provider_paused("openrouter"));
    }

    #[test]
    fn opencode_keys_custom_plugin_and_modified_builtin_cannot_import_key() {
        let f = Fixture::new();
        f.key("synthetic");
        for change in 0..5 {
            let mut m = registry::openrouter();
            match change {
                0 => m.provider.base = "https://openrouter.ai/api/alternate".into(),
                1 => m.provider.protocol = manifest::schema::WireProtocol::Responses,
                2 => m.provider.auth = manifest::schema::AuthKind::ApiKey,
                3 => m.permissions.net.push("evil.example".into()),
                _ => m.id = "custom-openrouter".into(),
            }
            let state =
                GatewayState::from_manifests(BTreeMap::from([(m.id.clone(), m.clone())]), "test")
                    .with_opencode_api_auth(f.0.clone());
            assert!(state.api_key_opt(&m.id).is_none());
        }
        let state = f.state();
        state
            .custom_ids
            .write()
            .unwrap()
            .insert("openrouter".into());
        assert!(state.api_key_opt("openrouter").is_none());
        state.custom_ids.write().unwrap().clear();
        state
            .plugin_ids
            .write()
            .unwrap()
            .insert("openrouter".into());
        assert!(state.api_key_opt("openrouter").is_none());
    }

    #[test]
    fn opencode_keys_saved_key_rejection_does_not_select_import_and_clear_tracks_fallback() {
        let f = Fixture::new();
        f.key("synthetic-import");
        let store = Arc::new(credentials::MemoryStore::default());
        let state = f.state().with_key_store(store);
        state
            .set_provider_key("openrouter", "synthetic-saved")
            .unwrap();
        let (_, old) = state.dispatch_credential(&registry::openrouter()).unwrap();
        state.set_key_retry_at("openrouter", Some(500));
        f.key("synthetic-import-updated");
        assert!(plan(&state).ordered.is_empty());
        assert_eq!(
            state.api_key_opt("openrouter").as_deref(),
            Some("synthetic-saved")
        );
        assert!(Arc::ptr_eq(&old, &version(&state)));
        state.clear_provider_key("openrouter").unwrap();
        assert_eq!(
            state.api_key_opt("openrouter").as_deref(),
            Some("synthetic-import-updated")
        );
        state.set_key_retry_at("openrouter", Some(600));
        assert!(plan(&state).ordered.is_empty()); // clear already updated baseline
        assert_eq!(state.snapshot()[0].key_retry_at, Some(600));
    }
}

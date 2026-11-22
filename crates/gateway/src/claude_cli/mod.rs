//! A single native Claude login, served through the official CLI and client-owned tools.
mod auth;
mod mcp;
mod run;
pub(crate) use run::Lease;

use axum::http::HeaderMap;
use proto::protocol::MessagesRequest;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Semaphore;

pub(crate) const ID: &str = "claude-cli";
pub(crate) const CALL_PREFIX: &str = "aubcc_";
pub(crate) fn owns(m: &manifest::schema::Manifest) -> bool {
    m.id == ID
        && m.provider.auth == manifest::schema::AuthKind::ClaudeCli
        && m.provider.base == "https://claude.ai"
        && m.kind == manifest::schema::ProviderKind::Subscription
}

#[derive(Default)]
pub(crate) struct Channel {
    update_gate: Arc<tokio::sync::RwLock<()>>,
    program: Option<PathBuf>,
    login: Mutex<Option<auth::Login>>,
    probe: tokio::sync::Mutex<()>,
    runs: Mutex<BTreeMap<String, Arc<run::Run>>>,
    generation: AtomicU64,
    epoch: Mutex<Arc<()>>,
}
impl Channel {
    pub(crate) fn with_update_gate(mut self, gate: Arc<tokio::sync::RwLock<()>>) -> Self {
        self.update_gate = gate;
        self
    }

    pub(crate) fn native() -> Self {
        Self {
            update_gate: Default::default(),
            program: super::agent_test::resolve_program("claude-code", "claude").ok(),
            login: Mutex::new(None),
            probe: tokio::sync::Mutex::new(()),
            runs: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            epoch: Mutex::new(Arc::new(())),
        }
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
    pub(crate) fn ready(&self) -> bool {
        self.login.lock().expect("claude login").is_some()
    }
    pub(crate) async fn refresh(&self, enabled: impl Fn() -> bool) {
        let Some(program) = &self.program else {
            return;
        };
        let _guard = self.probe.lock().await;
        if !enabled() {
            return;
        }
        let Ok(_activity) = crate::state::UpdateActivityLease::acquire(self.update_gate.clone())
        else {
            return;
        };
        let login = auth::probe(program).await;
        let subject = login.as_ref().map(|l| &l.subject);
        let mut current = self.login.lock().expect("claude login");
        if current.as_ref().map(|l| &l.subject) != subject {
            self.cancel_all();
        }
        *current = login;
    }
    pub(crate) fn binding(&self) -> Option<(String, Arc<()>)> {
        let login = self.login.lock().expect("claude login");
        Some((
            login.as_ref()?.subject.clone(),
            self.epoch.lock().expect("claude epoch").clone(),
        ))
    }
    pub(crate) fn update_bound(&self, epoch: Option<&Arc<()>>, update: impl FnOnce()) {
        let current = self.epoch.lock().expect("claude epoch");
        if epoch.is_some_and(|epoch| Arc::ptr_eq(epoch, &current)) {
            update();
        }
    }
    pub(crate) fn cancel_all(&self) {
        *self.epoch.lock().expect("claude epoch") = Arc::new(());
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut runs = self.runs.lock().expect("claude runs");
        for run in runs.values() {
            run.cancel();
        }
        runs.clear();
    }
    pub(crate) async fn send(
        &self,
        req: &MessagesRequest,
        headers: &HeaderMap,
        slot: Arc<Semaphore>,
        generation: u64,
        admitted: impl Fn() -> bool,
    ) -> Result<reqwest::Response, &'static str> {
        let login = self
            .login
            .lock()
            .expect("claude login")
            .clone()
            .ok_or("missing-credential")?;
        let owner = owner(req, headers);
        let results = results(req);
        if !results.is_empty() {
            let runs = self.runs.lock().expect("claude runs");
            if self.generation() != generation || !admitted() {
                return Err("claude-login-changed");
            }
            let run = runs
                .values()
                .find(|r| r.contains(&results[0].0))
                .ok_or("claude-continuation-expired")?;
            return run.claim(&owner, &login.subject, results);
        }
        let activity = crate::state::UpdateActivityLease::acquire(self.update_gate.clone())?;
        let permit = slot.try_acquire_owned().map_err(|_| "provider-busy")?;
        let program = self.program.as_ref().ok_or("missing-credential")?;
        let (_, response) = run::start(program, login, owner, req, permit, activity, |run| {
            let mut runs = self.runs.lock().expect("claude runs");
            runs.retain(|_, r| !r.finished());
            if self.generation() != generation || !admitted() {
                return Err("claude-login-changed");
            }
            if runs.len() >= 2 {
                return Err("provider-busy");
            }
            runs.insert(run.id.clone(), run.clone());
            Ok(())
        })
        .await?;
        Ok(response)
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

fn owner(req: &MessagesRequest, headers: &HeaderMap) -> String {
    let caller = crate::security::caller_key(headers).unwrap_or_default();
    let session = crate::session_identity::extract(headers, Some(req))
        .session
        .or_else(|| crate::route_plan::session_key(req, headers))
        .unwrap_or_default();
    credentials::quota::subject_tag("claude-cli-owner", &format!("{caller}|{session}"))
}
pub(crate) fn results(req: &MessagesRequest) -> Vec<(String, Value)> {
    let value = serde_json::to_value(req).unwrap_or_default();
    let Some(messages) = value["messages"].as_array() else {
        return Vec::new();
    };
    let after = messages
        .iter()
        .rposition(|m| m["role"] == "assistant")
        .map_or(0, |i| i + 1);
    messages[after..]
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| Some((b["tool_use_id"].as_str()?.to_owned(), b.clone())))
        .collect()
}
pub(crate) fn continuation(req: &MessagesRequest) -> bool {
    results(req)
        .iter()
        .any(|(id, _)| id.starts_with(CALL_PREFIX))
}

pub(crate) fn mixed_continuation(req: &MessagesRequest) -> bool {
    if !continuation(req) {
        return false;
    }
    let value = serde_json::to_value(req).unwrap_or_default();
    let Some(messages) = value["messages"].as_array() else {
        return true;
    };
    let after = messages
        .iter()
        .rposition(|m| m["role"] == "assistant")
        .map_or(0, |i| i + 1);
    messages[after..].iter().any(|m| match &m["content"] {
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().any(|b| b["type"] != "tool_result"),
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_old_success_and_limit_cannot_write_after_login_epoch_changes() {
        let manifest = registry::claude_cli();
        let root = std::env::temp_dir().join(format!(
            "aubridge-cli-epoch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, r#"{"claude_cli_enabled":true}"#).unwrap();
        let state = crate::state::GatewayState::from_manifests(
            BTreeMap::from([(ID.to_owned(), manifest.clone())]),
            "test",
        )
        .with_settings(config, root.join("stash"));
        *state.claude_cli.login.lock().unwrap() = Some(auth::Login {
            subject: "A".into(),
            env: BTreeMap::new(),
        });
        let (_, version) = state.dispatch_credential(&manifest).unwrap();
        let (_, epoch_a) = state.claude_cli.binding().unwrap();
        state.claude_cli.cancel_all();
        *state.claude_cli.login.lock().unwrap() = Some(auth::Login {
            subject: "B".into(),
            env: BTreeMap::new(),
        });
        let writes = std::sync::atomic::AtomicUsize::new(0);
        for _old_outcome in ["success", "429"] {
            state.update_bound_connection(ID, Some(&version), None, Some(&epoch_a), || {
                writes.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        let (_, epoch_b) = state.claude_cli.binding().unwrap();
        state.update_bound_connection(ID, Some(&version), None, Some(&epoch_b), || {
            writes.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pending_result_before_trailing_text_stays_pinned_and_is_explicitly_rejected() {
        let mut req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"auto", "max_tokens":64, "messages":[
                {"role":"assistant", "content":[{"type":"tool_use", "id":"aubcc_one", "name":"read", "input":{}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"aubcc_one", "content":"result"}]},
                {"role":"user", "content":"additional instruction"}
            ]
        })).unwrap();
        assert!(continuation(&req));
        assert!(mixed_continuation(&req));
        req.messages.push(proto::protocol::InboundMessage {
            role: "assistant".into(),
            content: proto::protocol::InboundContent::Text("done".into()),
        });
        req.messages.push(proto::protocol::InboundMessage {
            role: "user".into(),
            content: proto::protocol::InboundContent::Text("next ordinary turn".into()),
        });
        assert!(!continuation(&req));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_settings_keep_logged_in_cli_disabled_until_explicit_enable() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "aubridge-cli-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, r#"{"paused_providers":[]}"#).unwrap();
        let program = root.join("claude");
        std::fs::write(&program, r#"#!/bin/sh
printf x >> "$0.count"
printf '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","accountUuid":"fixture"}'
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let count = root.join("claude.count");
        let make_state = || {
            let manifest = registry::claude_cli();
            let mut state = crate::state::GatewayState::from_manifests(
                BTreeMap::from([(ID.into(), manifest)]),
                "test",
            )
            .with_settings(config.clone(), root.join("stash"));
            state.claude_cli.program = Some(program.clone());
            Arc::new(state)
        };
        let state = make_state();
        state.refresh_claude_cli().await;
        state.refresh_claude_cli().await;
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"sonnet","max_tokens":8,"stream":false,
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-claude-code".parse().unwrap());
        let body = serde_json::to_string(&req).unwrap();
        let _ = crate::upstream::forward(state.clone(), &req, &headers, &body).await;
        crate::upstream::check_provider(&state, ID, None)
            .await
            .unwrap();
        assert!(state.provider_paused(ID));
        assert!(!count.exists());
        headers.insert("authorization", "Bearer test-admin".parse().unwrap());
        let response = crate::provider_pause_route(
            state.clone(),
            headers.clone(),
            ID.into(),
            r#"{"paused":false}"#.into(),
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(std::fs::read(&count).unwrap(), b"x");
        assert!(crate::route_plan::credential(&state, &registry::claude_cli()).is_some());
        let plan = crate::route_plan::plan(
            &state,
            &req,
            &headers,
            crate::upstream::unix_now(),
            true,
            false,
            |_, _| None,
        );
        assert!(plan
            .ordered
            .iter()
            .any(|candidate| candidate.manifest.id == ID));
        let reloaded = make_state();
        reloaded.refresh_claude_cli().await;
        assert!(!reloaded.provider_paused(ID));
        assert!(crate::route_plan::credential(&reloaded, &registry::claude_cli()).is_some());
        assert_eq!(std::fs::read(&count).unwrap(), b"xx");
        reloaded.set_provider_paused(ID, true).unwrap();
        reloaded.refresh_claude_cli().await;
        crate::upstream::check_provider(&reloaded, ID, None)
            .await
            .unwrap();
        assert!(crate::route_plan::credential(&reloaded, &registry::claude_cli()).is_none());
        assert_eq!(std::fs::read(&count).unwrap(), b"xx");
        let disabled = make_state();
        disabled.refresh_claude_cli().await;
        assert!(disabled.provider_paused(ID));
        std::fs::write(&config, "broken").unwrap();
        assert!(disabled.set_provider_paused(ID, false).is_err());
        disabled.refresh_claude_cli().await;
        assert!(disabled.provider_paused(ID));
        assert_eq!(std::fs::read(&count).unwrap(), b"xx");
        let invalid = make_state();
        invalid.refresh_claude_cli().await;
        assert!(invalid.provider_paused(ID));
        std::fs::remove_file(&config).unwrap();
        let missing = make_state();
        missing.refresh_claude_cli().await;
        assert!(missing.provider_paused(ID));
        assert_eq!(std::fs::read(&count).unwrap(), b"xx");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cli_owner_prioritizes_thread_over_shared_session() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"sonnet", "max_tokens":8, "messages":[]
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "shared-session".parse().unwrap());
        headers.insert("thread-id", "parent-thread".parse().unwrap());
        let parent = owner(&req, &headers);
        headers.insert("thread-id", "child-thread".parse().unwrap());
        assert_ne!(parent, owner(&req, &headers));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enabled_cli_media_never_spawns_and_count_remains_a_local_estimate() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "aubridge-cli-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, r#"{"claude_cli_enabled":true}"#).unwrap();
        let program = root.join("claude");
        std::fs::write(&program, "#!/bin/sh\nprintf x >> \"$0.count\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut state = crate::state::GatewayState::from_manifests(
            BTreeMap::from([(ID.into(), registry::claude_cli())]),
            "test",
        )
        .with_settings(config, root.join("stash"));
        state.claude_cli.program = Some(program);
        *state.claude_cli.login.lock().unwrap() = Some(auth::Login {
            subject: "fixture".into(),
            env: BTreeMap::new(),
        });
        let state = Arc::new(state);
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-claude-code".parse().unwrap());
        for block in [
            serde_json::json!({"type":"image","source":{"type":"url","url":"https://example.invalid/probe.png"}}),
            serde_json::json!({"type":"document","source":{"type":"text","media_type":"text/plain","data":"file bytes"}}),
        ] {
            let body = serde_json::json!({"model":"sonnet","max_tokens":16,"stream":false,
                "messages":[{"role":"user","content":[block]}]})
            .to_string();
            let response =
                crate::messages_route(state.clone(), headers.clone(), body.clone(), false).await;
            assert_eq!(response.status(), 400);
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains(translate::media::UNSUPPORTED));
            let count = crate::messages_route(state.clone(), headers.clone(), body, true).await;
            assert_eq!(count.status(), 200);
            let bytes = axum::body::to_bytes(count.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["estimated"], true);
        }
        assert!(!root.join("claude.count").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

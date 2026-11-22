//! Isolated-data-dir restore: pending cannot resolve credentials or reach the network.
use credentials::work_config_backup::{Archive, Group, Payload, Source, SourceKind};
use gateway::state::GatewayState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Denied(AtomicUsize);
impl credentials::KeyStore for Denied {
    fn get(&self, _: &str) -> Option<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Err("denied".into())
    }
    fn delete(&self, _: &str) -> Result<(), String> {
        Err("denied".into())
    }
}
fn payload() -> Payload {
    Payload {
        sources: vec![Source {
            id: "fixture-source".into(),
            source: SourceKind::Custom,
            base: "http://127.0.0.1:9/v1".into(),
            auth: "bearer".into(),
            protocol: "chat".into(),
            kind: "free_tier".into(),
            models: vec!["fixture-model".into()],
        }],
        groups: vec![Group {
            name: "fixture-group".into(),
            members: vec!["fixture-model".into()],
            rule: "sequential".into(),
        }],
        model_selections: BTreeMap::new(),
        paused_providers: vec![],
        chatgpt_login_source: "codex".into(),
        client_wiring: vec![],
        cost_intent: None,
    }
}
#[tokio::test]
async fn staging_does_not_fetch_keys_or_expose_models_and_denied_rebind_stays_pending() {
    let dir = std::env::temp_dir().join(format!("aubridge-backup-gateway-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"port":3427,"future":11}"#).unwrap();
    let archive = dir.join("backup.json");
    std::fs::write(&archive, Archive::encode(payload()).unwrap()).unwrap();
    let keys = Arc::new(Denied(AtomicUsize::new(0)));
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "fixture")
            .with_codex_channel(gateway::chatgpt::ChatGptChannel::new(
                dir.join("isolated-auth.json"),
                dir.join("isolated-config.toml"),
            ))
            .with_settings(config.clone(), dir.join("stash"))
            .with_key_store(keys.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task =
        tokio::spawn(async move { axum::serve(listener, gateway::app(state)).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let post = |action: &str| {
        client
            .post(format!("{base}/admin/work-config/{action}"))
            .bearer_auth("fixture-admin")
    };
    assert_eq!(
        client
            .get(format!("{base}/admin/work-config"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let preview: Value = post("preview")
        .json(&json!({"path":archive}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(preview["preview_id"].is_string(), "{preview}");
    let staged: Value = post("stage")
        .json(&json!({"preview_id":preview["preview_id"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(staged["status"], "restore_pending", "{staged}");
    assert_eq!(
        keys.0.load(Ordering::SeqCst),
        0,
        "pending cannot even read Keychain"
    );
    let document: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert!(document["providers"].is_null());
    assert_eq!(document["future"], 11);
    let listed: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("fixture-ui-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!listed.to_string().contains("fixture-model"));
    let response=post("rebind").json(&json!({"restore_id":staged["restore_id"],"source_id":"fixture-source","expected_identity":payload().sources[0].identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).send().await.unwrap();
    assert!(!response.status().is_success());
    assert!(response.text().await.unwrap().contains("未取得"));
    assert_eq!(keys.0.load(Ordering::SeqCst), 1);
    let response = post("cancel")
        .json(&json!({"restore_id":staged["restore_id"]}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let document: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    assert!(document.get("pending_work_config_restore").is_none());
    task.abort();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restored_source_requires_explicit_binding_and_undo_survives_restart() {
    let dir =
        std::env::temp_dir().join(format!("aubridge-backup-roundtrip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"port":3427,"future":11,"share_login":true}"#).unwrap();
    let archive = dir.join("backup.json");
    std::fs::write(&archive, Archive::encode(payload()).unwrap()).unwrap();
    let keys = Arc::new(credentials::MemoryStore::default());
    credentials::KeyStore::set(keys.as_ref(), "fixture-source", "synthetic-local-key").unwrap();
    let build = || {
        Arc::new(
            GatewayState::from_manifests(BTreeMap::new(), "fixture")
                .with_codex_channel(gateway::chatgpt::ChatGptChannel::new(
                    dir.join("isolated-auth.json"),
                    dir.join("isolated-config.toml"),
                ))
                .with_settings(config.clone(), dir.join("stash"))
                .with_key_store(keys.clone()),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = build();
    let task =
        tokio::spawn(async move { axum::serve(listener, gateway::app(state)).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let post = |action: &str| {
        client
            .post(format!("{base}/admin/work-config/{action}"))
            .bearer_auth("fixture-admin")
    };
    let preview: Value = post("preview")
        .json(&json!({"path":archive}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let staged: Value = post("stage")
        .json(&json!({"preview_id":preview["preview_id"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = post("promote")
        .json(
            &json!({"restore_id":staged["restore_id"],"target_revision":staged["target_revision"]}),
        )
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    let bound:Value=post("rebind").json(&json!({"restore_id":staged["restore_id"],"source_id":"fixture-source","expected_identity":payload().sources[0].identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).send().await.unwrap().json().await.unwrap();
    assert_eq!(bound["sources"][0]["credential_status"], "bound", "{bound}");
    let promoted: Value = post("promote")
        .json(
            &json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(promoted["status"], "configuration_restored", "{promoted}");
    let mut document: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(document["providers"][0]["id"], "fixture-source");
    assert_eq!(
        document["restored_keychain_only"],
        json!(["fixture-source"])
    );
    assert_eq!(document["share_login"], true);
    task.abort();
    document["future"] = json!(42);
    std::fs::write(&config, serde_json::to_vec(&document).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = build();
    let task =
        tokio::spawn(async move { axum::serve(listener, gateway::app(state)).await.unwrap() });
    let status: Value = client
        .get(format!("{base}/admin/work-config"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status["restore_id"].is_string(), "{status}");
    let response = client
        .post(format!("{base}/admin/work-config/undo"))
        .bearer_auth("fixture-admin")
        .json(&json!({"restore_id":status["restore_id"]}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    let document: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert!(document["providers"].is_null());
    assert_eq!(document["future"], 42);
    assert_eq!(document["share_login"], true);
    task.abort();
    let _ = std::fs::remove_dir_all(dir);
}

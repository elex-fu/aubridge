//! Decision 52: cost intent gates the actual candidate set, not just labels.
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    routing::{get, post},
    Json, Router,
};
use gateway::state::GatewayState;
use serde_json::{json, Value};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    base: String,
    client: reqwest::Client,
    config: std::path::PathBuf,
    calls: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(self.config.parent().unwrap());
    }
}

impl Fixture {
    async fn new(intent: Option<&str>, kind: &str, authorized: bool, directory: Value) -> Self {
        Self::with_protocol(intent, kind, authorized, directory, "chat").await
    }

    async fn with_protocol(
        intent: Option<&str>,
        kind: &str,
        authorized: bool,
        directory: Value,
        protocol: &str,
    ) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let (upstream, server) = serve(Router::new()
            .route("/v1/models", get(move || { let value = directory.clone(); async move { Json(value) } }))
            .route("/v1/messages/count_tokens", post({
                let calls = calls.clone();
                move || {let calls=calls.clone(); async move {
                    calls.fetch_add(1,Ordering::SeqCst);
                    Json(json!({"input_tokens":4242}))
                }}
            }))
            .route("/v1/chat/completions", post({
                let calls = calls.clone();
                move |Json(request): Json<Value>| { let calls = calls.clone(); async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request["stream"],true,"Messages translation requires upstream SSE");
                    let chunk=json!({"id":"fixture-response","object":"chat.completion.chunk","model":"m",
                        "choices":[{"index":0,"delta":{"role":"assistant","content":"OK"},"finish_reason":null}]});
                    let finish=json!({"id":"fixture-response","object":"chat.completion.chunk","model":"m",
                        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                    ([("content-type","text/event-stream")],format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"))
                }}
            }))).await;
        let root = std::env::temp_dir().join(format!(
            "aubridge-cost-intent-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.json");
        let manifest = serde_json::from_value(json!({"id":"vendor","version":"1","kind":kind,
            "provider":{"base":format!("{upstream}/v1"),"auth":"bearer","protocol":protocol},
            "free_models":["m"],"model_facts":{"m":{"source":"vendor-directory","free_evidence":"vendor-zero-price",
                "price":{"input_per_million":0.0,"output_per_million":0.0}}},
            "permissions":{"net":["127.0.0.1"]}})).unwrap();
        std::fs::write(&config, json!({"future":{"preserve":true}, "cost_intent":intent,
            "subscription_authorizations": if authorized {json!([{"provider":"vendor","basis":"user_declared","connection_identity":credentials::usage_policy::connection_identity(&manifest)}])} else {json!([])},
            "model_groups":[{"name":"daily","members":["m"],"rule":"sequential"}]}).to_string()).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let state = Arc::new(
            GatewayState::with_keys(
                BTreeMap::from([("vendor".into(), manifest)]),
                "fixture",
                BTreeMap::from([("vendor".into(), "synthetic-secret".into())]),
            )
            .with_client(client.clone())
            .with_settings(config.clone(), root.join("stash"))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
        );
        let (base, gateway) = serve(gateway::app(state)).await;
        Self {
            base,
            client,
            config,
            calls,
            tasks: vec![server, gateway],
        }
    }

    async fn infer(&self, path: &str, model: &str) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth("fixture-ui-test")
            .json(&json!({"model":model,"max_tokens":16,"stream":false,
                "messages":[{"role":"user","content":"OK"}]}))
            .send()
            .await
            .unwrap()
    }

    async fn refresh(&self) {
        assert!(self
            .client
            .post(format!("{}/admin/providers/vendor/catalog", self.base))
            .bearer_auth("fixture-admin")
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
}

#[tokio::test]
async fn forged_manifest_free_evidence_cannot_bypass_explicit_auto_group_or_count() {
    let f = Fixture::new(
        Some("confirmed_free_only"),
        "free_tier",
        false,
        json!({"data":[{"id":"m"}]}),
    )
    .await;
    for model in ["m", "aubridge/auto", "daily"] {
        let response = f.infer("/v1/messages", model).await;
        assert!(
            !response.status().is_success(),
            "unconfirmed model routed via {model}"
        );
    }
    let response = f.infer("/v1/messages/count_tokens", "m").await;
    assert!(
        response.status().is_success(),
        "local count remains available without an upstream"
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    let preview: Value = f
        .client
        .get(format!("{}/admin/routing/preview?model=daily", f.base))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["candidates"], json!([]));
    assert!(preview["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["reason"] == "cost-unconfirmed-free"));
}

#[tokio::test]
async fn unknown_and_paid_vendor_directory_are_not_confirmed_free() {
    for row in [
        json!({"id":"m"}),
        json!({"id":"m","pricing":{"prompt":"0","completion":"0.001"}}),
        json!({"id":"m","pricing":{"prompt":"0","completion":"0","request":"1"}}),
    ] {
        let f = Fixture::new(
            Some("confirmed_free_only"),
            "free_tier",
            false,
            json!({"data":[row]}),
        )
        .await;
        f.refresh().await;
        assert!(!f.infer("/v1/messages", "m").await.status().is_success());
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn subscription_fallback_requires_an_explicit_source_authorization() {
    for (kind, authorized, allowed) in [
        ("subscription", false, false),
        ("subscription", true, false),
        ("free_tier", true, false),
    ] {
        let f = Fixture::new(
            Some("free_then_subscription"),
            kind,
            authorized,
            json!({"data":[{"id":"m"}]}),
        )
        .await;
        let response = f.infer("/v1/messages", "m").await;
        assert_eq!(
            response.status().is_success(),
            allowed,
            "kind={kind}, authorized={authorized}"
        );
        assert_eq!(f.calls.load(Ordering::SeqCst), usize::from(allowed));
    }
}

#[tokio::test]
async fn legacy_settings_preserve_routing_and_report_unselected_intent() {
    let f = Fixture::new(None, "free_tier", false, json!({"data":[{"id":"m"}]})).await;
    let response = f.infer("/v1/messages", "m").await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(
        status.is_success(),
        "legacy inference status={status}, calls={}, body={body}",
        f.calls.load(Ordering::SeqCst)
    );
    let settings: Value = f
        .client
        .get(format!("{}/admin/settings", f.base))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        settings.get("cost_intent").is_some(),
        "unselected intent must be visible"
    );
    assert!(settings["cost_intent"].is_null());
    let changed = f
        .client
        .post(format!("{}/admin/settings", f.base))
        .bearer_auth("fixture-admin")
        .json(&json!({"cost_intent":"confirmed_free_only","subscription_authorizations":[]}))
        .send()
        .await
        .unwrap();
    assert!(changed.status().is_success());
    let document: Value = serde_json::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
    assert_eq!(document["future"]["preserve"], true);
    assert_eq!(document["cost_intent"], "confirmed_free_only");
    assert!(!f.infer("/v1/messages", "m").await.status().is_success());
}

#[tokio::test]
async fn invalid_explicit_cost_policy_never_reverts_to_unrestricted_legacy() {
    let f = Fixture::new(
        Some("future_or_typo_policy"),
        "free_tier",
        false,
        json!({"data":[{"id":"m"}]}),
    )
    .await;
    assert!(!f.infer("/v1/messages", "m").await.status().is_success());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    let settings: Value = f
        .client
        .get(format!("{}/admin/settings", f.base))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["cost_policy_issue"], "invalid-settings");
}

#[tokio::test]
async fn native_count_uses_the_same_cost_filtered_candidates() {
    for (intent, expected_calls) in [(Some("confirmed_free_only"), 0), (None, 1)] {
        let f = Fixture::with_protocol(
            intent,
            "free_tier",
            false,
            json!({"data":[{"id":"m"}]}),
            "anthropic",
        )
        .await;
        let response = f.infer("/v1/messages/count_tokens", "m").await;
        assert!(response.status().is_success());
        let body: Value = response.json().await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(body["input_tokens"] == 4242, expected_calls == 1);
    }
}

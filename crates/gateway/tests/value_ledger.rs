//! Usage details and diagnostics are retained beyond the SSE lifetime.
use axum::{
    routing::{get, post},
    Json, Router,
};
use gateway::state::GatewayState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn reported_cache_and_price_snapshot_survive_the_http_request() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;
    let changed = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let fixture="data: {\"id\":\"cache-fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"cache-fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":12,\"cache_write_tokens\":4}}}\n\ndata: [DONE]\n\n";
    let (upstream,server)=serve(Router::new()
        .route("/v1/models",get({let changed=changed.clone();move || {let changed=changed.clone();async move {Json(json!({"data":[{"id":"priced","pricing":{
            "prompt":if changed.load(Ordering::Relaxed) {"0.000004"} else {"0.000002"},"completion":"0.000005","input_cache_read":"0.0000002","input_cache_write":"0.000003"}}]}))}}}))
        .route("/v1/chat/completions",post({let entered=entered.clone();let release=release.clone();move || {let entered=entered.clone();let release=release.clone();async move {
            entered.notify_one();release.notified().await;
            ([("content-type","text/event-stream"),("x-request-id","vendor-fixture")],fixture)
        }}}))).await;
    let manifest =
        serde_json::from_value(json!({"id":"vendor","version":"1","kind":"subscription",
        "provider":{"base":format!("{upstream}/v1"),"auth":"bearer","protocol":"chat"},
        "free_models":["priced"],"permissions":{"net":["127.0.0.1"]}}))
        .unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("vendor".into(), manifest)]),
            "fixture",
            BTreeMap::from([("vendor".into(), "synthetic-key".into())]),
        )
        .with_client(reqwest::Client::builder().no_proxy().build().unwrap())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, gateway) = serve(gateway::app(state)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    client
        .post(format!("{base}/admin/providers/vendor/catalog"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pending = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move {
            client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("fixture-ui-test")
        .header("x-aubridge-request-id", "untrusted-client-value")
        .json(&json!({"model":"priced","stream":true,"messages":[{"role":"user","content":"confidential-cache-probe"}]}))
        .send()
        .await
        .unwrap()
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    changed.store(true, Ordering::Relaxed);
    let refreshed: Value = client
        .post(format!("{base}/admin/providers/vendor/catalog"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        refreshed["models"][0]["facts"]["price"]["input_per_million"],
        4.0
    );
    release.notify_one();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), 200);
    let request_id = response
        .headers()
        .get("x-aubridge-request-id")
        .and_then(|id| id.to_str().ok())
        .map(str::to_owned);
    assert!(response.text().await.unwrap().contains("OK"));
    let history: Value = client
        .get(format!("{base}/admin/gateway"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &history["requests"][0];
    assert_eq!(
        row["cache_read_tokens"], 12,
        "reported cache read was discarded"
    );
    assert_eq!(row["cache_write_tokens"], 4);
    assert!(request_id
        .as_deref()
        .is_some_and(|id| id.starts_with("req_")));
    assert_eq!(row["request_id"].as_str(), request_id.as_deref());
    assert_ne!(request_id.as_deref(), Some("untrusted-client-value"));
    assert!(row["request_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        row["price_snapshot"]["price"]["input_per_million"], 2.0,
        "in-flight request picked up a later price"
    );
    assert_eq!(row["attempts"][0]["upstream_request_id"], "vendor-fixture");
    assert!(!history.to_string().contains("confidential-cache-probe"));
    let summary: Value = client
        .get(format!("{base}/admin/dashboard"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let value = &summary["value_summary"];
    assert_eq!(value["priced_records"], 1);
    assert_eq!(value["cache_read_tokens"], 12);
    assert!((value["api_equivalent_usd"].as_f64().unwrap() - 72.4 / 1e6).abs() < 1e-12);
    assert!((value["cache_hit_rate"].as_f64().unwrap() - 0.6).abs() < 1e-12);
    let filtered: Value = client
        .get(format!("{base}/admin/gateway"))
        .query(&[("request_id", request_id.as_deref().unwrap())])
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(filtered["requests"].as_array().unwrap().len(), 1);
    let invalid = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("fixture-ui-test")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    let rejected_id = invalid.headers()["x-aubridge-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    invalid.text().await.unwrap();
    let rejected: Value = client
        .get(format!("{base}/admin/gateway"))
        .query(&[("request_id", &rejected_id)])
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        rejected["requests"].as_array().unwrap().len(),
        1,
        "rejection should have one diagnostic"
    );
    assert_eq!(rejected["requests"][0]["outcome"], "request-rejected");
    let cleared = client
        .delete(format!("{base}/admin/gateway/history"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap();
    assert!(cleared.status().is_success());
    let after: Value = client
        .get(format!("{base}/admin/dashboard"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after["value_summary"], *value,
        "clearing diagnostic history changed usage or prices"
    );
    gateway.abort();
    server.abort();
}

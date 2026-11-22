//! Pricing administration never sends an upstream request.
use gateway::{app, state::GatewayState};
use serde_json::{json, Value};

#[tokio::test]
async fn pricing_api_is_admin_only_and_conflicts_do_not_overwrite() {
    let root = std::env::temp_dir().join(format!(
        "aubridge-pricing-api-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    std::fs::write(&path, r#"{"port":4321,"unrelated":{"preserve":true}}"#).unwrap();
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(Default::default(), "test")
            .with_settings(path.clone(), root.join("stash")),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{base}/admin/pricing");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let read: Value = client
        .get(&url)
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["mappings"], json!([]));
    let mapping = json!({"provider":"chatgpt","model":"gpt-6.1-sol","target_provider":"user-choice","target_model":"example",
        "source":"explicit test input, not official pricing","version":"test-v1","observed_at":1,
        "price":{"input_per_million":2,"output_per_million":10,"cache_read_per_million":null,"cache_write_per_million":null}});
    let write = client
        .post(&url)
        .bearer_auth("test-admin")
        .json(&json!({"expected":[],"mappings":[mapping.clone()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 200);
    let stale = client
        .post(&url)
        .bearer_auth("test-admin")
        .json(&json!({"expected":[],"mappings":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let read: Value = client
        .get(&url)
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["mappings"][0]["source"], mapping["source"]);
    assert_eq!(read["currency"], "USD");
    let document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(document["port"], 4321);
    assert_eq!(document["unrelated"]["preserve"], true);
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

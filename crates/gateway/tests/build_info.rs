use std::{collections::BTreeMap, sync::Arc};

use gateway::{app, cli, state::GatewayState};
use serde_json::Value;

#[tokio::test]
async fn build_identity_is_admin_only_and_reports_the_running_binary() {
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "test")
            .with_admin_token("build-admin".into()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for token in [None, Some("test"), Some("test-codex")] {
        let mut request = client.get(format!("{endpoint}/admin/build"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert_eq!(request.send().await.unwrap().status(), 401);
    }
    let response = client
        .get(format!("{endpoint}/admin/build"))
        .bearer_auth("build-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["pid"], std::process::id());
    assert!(matches!(
        value["mode"].as_str(),
        Some("source" | "development" | "distribution")
    ));
    assert!(value["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "responses-http"));
    assert!(!value.to_string().contains("build-admin"));
    if let Some(revision) = value["git_revision"].as_str() {
        assert_eq!(revision.len(), 40);
        assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    if value["mode"] == "source" {
        assert_eq!(value["source_verified"], false);
    }
    let dir = std::env::temp_dir().join(format!("aubridge-build-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("gateway-session.json"),
        serde_json::json!({
            "endpoint":endpoint,"admin_token":"build-admin"
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(cli::run_with_dir(&["status".into()], &dir).await, 0);
    std::fs::remove_dir_all(dir).unwrap();
    server.abort();
}

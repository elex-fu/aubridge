use gateway::{app, state::GatewayState};
use std::{collections::BTreeMap, sync::Arc};

#[tokio::test]
async fn native_resume_is_admin_only_and_cannot_take_browser_commands() {
    let state = Arc::new(GatewayState::from_manifests(BTreeMap::new(), "aubridge"));
    let token = state.admin_token().to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{address}/admin/native-sessions");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .post(format!("{url}/resume"))
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let list: serde_json::Value = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"], serde_json::json!([]));
    assert_eq!(list["agents"].as_array().unwrap().len(), 4);
    for body in [
        r#"{"handle":"x","cwd":"/tmp"}"#,
        r#"{"handle":"x","argv":["touch","/tmp/surprise"]}"#,
    ] {
        let response = client
            .post(format!("{url}/resume"))
            .bearer_auth(&token)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let response = client
        .post(format!("{url}/resume"))
        .bearer_auth(&token)
        .body(serde_json::json!({"handle":"0".repeat(64)}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    server.abort();
}

//! Saved custom-provider catalog discovery through real loopback Admin routes.
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{http::HeaderMap, routing::get, Json, Router};
use gateway::state::GatewayState;
use serde_json::{json, Value};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn saved_model_catalog_uses_latest_credentials_without_mutating_routing() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (upstream, upstream_task) = serve(Router::new().route("/saved/v1/models", get({
        let seen = seen.clone();
        move |headers: HeaderMap| {
            seen.lock().unwrap().push(headers["authorization"].to_str().unwrap().to_owned());
            async {
                Json(json!({"data":[
                    {"id":" vendor/new:model ","name":"ignore-name","pricing":{"private":"ignore"}},
                    {"id":"vendor/old"}, {"id":"vendor/new:model"}, {"id":"模型/一"},
                    {"id":""}, {"id":17}, {"name":"not-an-id"}, {"id":"bad,id"},
                    {"id":"bad\nidentifier"}
                ]}))
            }
        }
    }))).await;
    let dir = std::env::temp_dir().join(format!("aubridge-model-discovery-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_settings(config.clone(), dir.join("stash"))
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, task) = serve(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .json(&body)
            .send()
    };
    post(
        "providers",
        json!({
            "id":"saved","base":format!("{upstream}/saved/v1/"),"models":["vendor/old"],
            "kind":"subscription","protocol":"chat","key":"old-fixture-key"
        }),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    post("keys", json!({"id":"saved","key":"latest-fixture-key"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let saved = std::fs::read(&config).unwrap();
    let before = state.snapshot();
    let endpoint = format!("{base}/admin/providers/saved/model-catalog");
    let denied = client
        .get(&endpoint)
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert!(seen.lock().unwrap().is_empty());

    let response = client
        .get(&endpoint)
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "saved custom Chat catalog route");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        json!({"models":["vendor/new:model","vendor/old","模型/一"]})
    );
    assert_eq!(*seen.lock().unwrap(), ["Bearer latest-fixture-key"]);
    assert_eq!(std::fs::read(&config).unwrap(), saved);
    assert_eq!(state.snapshot(), before);
    assert_eq!(state.recent_decisions().unwrap().len(), 0);
    assert_eq!(state.manifest("saved").unwrap().free_models, ["vendor/old"]);
    let overview: Value = client
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["providers"][0]["model_discovery_supported"], true);
    assert!(!body.to_string().contains("fixture-key"));
    task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn saved_model_catalog_bounds_errors_redirects_and_stale_connections() {
    use axum::http::StatusCode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ticker = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let response = Arc::new(std::sync::Mutex::new((
        200_u16,
        String::new(),
        String::new(),
    )));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let target_hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/models",
            get({
                let response = response.clone();
                let entered = entered.clone();
                let release = release.clone();
                move || {
                    let (status, body, location) = response.lock().unwrap().clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        if body == "wait" {
                            entered.notify_one();
                            release.notified().await;
                        }
                        let mut reply = axum::http::Response::builder()
                            .status(status)
                            .header("content-type", "application/json");
                        if !location.is_empty() {
                            reply = reply.header("location", location);
                        }
                        reply
                            .body(axum::body::Body::from(if body == "wait" {
                                r#"{"data":[{"id":"stale-result"}]}"#.to_owned()
                            } else {
                                body
                            }))
                            .unwrap()
                    }
                }
            }),
        )
        .route(
            "/redirect-target",
            get({
                let hits = target_hits.clone();
                move || {
                    hits.fetch_add(1, Ordering::Relaxed);
                    async { Json(json!({"data":[{"id":"must-not-follow"}]})) }
                }
            }),
        );
    let (upstream, upstream_task) = serve(app).await;
    let dir = std::env::temp_dir().join(format!(
        "aubridge-model-catalog-bounds-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_settings(dir.join("config.json"), dir.join("stash"))
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, task) = serve(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (id, protocol) in [("saved", "chat"), ("anthropic", "anthropic")] {
        client
            .post(format!("{base}/admin/providers"))
            .bearer_auth("aubridge-admin")
            .json(
                &json!({"id":id,"base":format!("{upstream}/v1"),"models":["old"],
                "protocol":protocol,"key":"fixture-private-key"}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let request = |id: &str| {
        client
            .get(format!("{base}/admin/providers/{id}/model-catalog"))
            .bearer_auth("aubridge-admin")
            .send()
    };
    for (id, expected) in [
        ("unknown", "unknown-provider"),
        ("anthropic", "unsupported-provider"),
    ] {
        let reply = request(id).await.unwrap();
        assert_eq!(reply.headers()["cache-control"], "no-store");
        let body: Value = reply.json().await.unwrap();
        assert_eq!(body["code"], expected);
    }
    let cases = [
        (
            401,
            "fixture-private-key remote body".to_owned(),
            "authentication-failed",
        ),
        (
            500,
            "fixture-private-key remote body".to_owned(),
            "upstream-http",
        ),
        (
            200,
            "not-json fixture-private-key".to_owned(),
            "invalid-directory",
        ),
        (
            200,
            json!({"data":[{"name":"no id"},{"id":false},{"id":"bad,id"}]}).to_string(),
            "invalid-directory",
        ),
        (
            200,
            json!({"data":(0..1001).map(|n| json!({"id":format!("id-{n}")})).collect::<Vec<_>>()} )
                .to_string(),
            "catalog-too-large",
        ),
        (200, " ".repeat(2 * 1024 * 1024 + 1), "catalog-too-large"),
        (302, String::new(), "upstream-http"),
    ];
    for (status, body, code) in cases {
        *response.lock().unwrap() = (status, body, format!("{upstream}/redirect-target"));
        let reply = request("saved").await.unwrap();
        assert_eq!(reply.status(), StatusCode::BAD_GATEWAY, "{code}");
        assert_eq!(reply.headers()["cache-control"], "no-store");
        let body: Value = reply.json().await.unwrap();
        assert_eq!(body["code"], code);
        assert!(!body.to_string().contains("fixture-private-key"));
        assert!(!body.to_string().contains(&upstream));
    }
    assert_eq!(target_hits.load(Ordering::Relaxed), 0);
    *response.lock().unwrap() = (200, r#"{"data":[]}"#.into(), String::new());
    let reply = request("saved").await.unwrap();
    assert_eq!(reply.status(), 200);
    assert_eq!(reply.json::<Value>().await.unwrap(), json!({"models":[]}));

    *response.lock().unwrap() = (200, "wait".into(), String::new());
    let pending = tokio::spawn(request("saved"));
    entered.notified().await;
    state
        .set_provider_key("saved", "rotated-fixture-key")
        .unwrap();
    release.notify_one();
    let reply = pending.await.unwrap().unwrap();
    assert_eq!(reply.status(), 409);
    assert_eq!(reply.headers()["cache-control"], "no-store");
    assert_eq!(
        reply.json::<Value>().await.unwrap()["code"],
        "provider-changed"
    );

    let pending = tokio::spawn(request("saved"));
    entered.notified().await;
    tokio::time::advance(std::time::Duration::from_millis(10_001)).await;
    let reply = pending.await.unwrap().unwrap();
    assert_eq!(reply.status(), 504);
    assert_eq!(reply.headers()["cache-control"], "no-store");
    assert_eq!(reply.json::<Value>().await.unwrap()["code"], "timeout");
    release.notify_one();
    assert!(state.recent_decisions().unwrap().is_empty());
    assert_eq!(state.manifest("saved").unwrap().free_models, ["old"]);
    for server in [ticker, upstream_task, task] {
        server.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

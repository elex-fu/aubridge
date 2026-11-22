//! Complete vendor directories, identity isolation and capability preflight.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{routing::get, Json, Router};
use gateway::state::GatewayState;
use serde_json::{json, Value};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn vendor_directory_exposes_capabilities_and_confirms_configured_models() {
    let (upstream, server) = serve(Router::new().route(
        "/v1/models",
        get(|| async {
            Json(
                json!({"data":[{"id":"listed","context_length":8192,"max_output_tokens":2048,
            "supported_parameters":["tools","reasoning"],
            "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},
            "pricing":{"prompt":"0.000001","completion":"0.000002"}}]}),
            )
        }),
    ))
    .await;
    let manifest =
        serde_json::from_value(json!({"id":"vendor","version":"1","kind":"subscription",
        "provider":{"base":format!("{upstream}/v1"),"auth":"bearer","protocol":"chat"},
        "free_models":["listed","missing"],"permissions":{"net":["127.0.0.1"]}}))
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("vendor".into(), manifest)]),
            "fixture",
            BTreeMap::from([("vendor".into(), "synthetic-key".into())]),
        )
        .with_client(client.clone())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, gateway) = serve(gateway::app(state.clone())).await;
    let response = client
        .post(format!("{base}/admin/providers/vendor/catalog"))
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "missing structured vendor catalog refresh"
    );
    let body: Value = response.json().await.unwrap();
    let model = &body["models"][0];
    assert_eq!(model["id"], "listed");
    assert_eq!(model["facts"]["context"], 8192);
    assert_eq!(model["facts"]["output"], 2048);
    assert_eq!(model["facts"]["tools"], true);
    assert_eq!(model["facts"]["image_input"], true);
    assert_eq!(model["facts"]["reasoning"], true);
    assert_eq!(model["facts"]["price"]["input_per_million"], 1.0);
    assert_eq!(model["facts"]["free_evidence"], "subscription");
    let rejected = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .json(&json!({"model":"missing","stream":false,"max_tokens":16,
            "messages":[{"role":"user","content":"OK"}]}))
        .send()
        .await
        .unwrap();
    assert!(rejected.status().is_client_error());
    assert!(rejected.text().await.unwrap().contains("model-not-listed"));
    let output = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .json(&json!({"model":"listed","stream":false,"max_tokens":2049,
            "messages":[{"role":"user","content":"OK"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(output.status(), 400);
    assert!(output.text().await.unwrap().contains("output limit"));
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("fixture-ui-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "listed")
        .unwrap();
    assert_eq!(listed["capabilities"]["context"], 8192);
    gateway.abort();
    server.abort();
}

fn manifest(base: &str) -> manifest::schema::Manifest {
    serde_json::from_value(json!({"id":"vendor","version":"1","kind":"subscription",
        "provider":{"base":format!("{base}/v1"),"auth":"bearer","protocol":"chat"},
        "free_models":["first","last"],"permissions":{"net":["127.0.0.1"],"store_key":true}}))
    .unwrap()
}

async fn catalog(client: &reqwest::Client, base: &str, refresh: bool) -> Value {
    let url = format!("{base}/admin/providers/vendor/catalog");
    let request = if refresh {
        client.post(url)
    } else {
        client.get(url)
    };
    request
        .bearer_auth("fixture-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn complete_pages_survive_failures_restart_and_explicit_model_selection() {
    use axum::extract::Query;
    use std::sync::atomic::{AtomicBool, Ordering};
    let fail = Arc::new(AtomicBool::new(false));
    let (upstream,server)=serve(Router::new().route("/v1/models",get({
        let fail=fail.clone();
        move |Query(query):Query<BTreeMap<String,String>>| {
            let fail=fail.clone();
            async move {
                let after=query.get("after");
                if after.is_some() && fail.load(Ordering::Relaxed) {
                    return (axum::http::StatusCode::UNAUTHORIZED,Json(json!({"error":"denied"})));
                }
                let payload=if let Some(after)=after {
                    assert_eq!(after,"first");
                    json!({"data":[{"id":"last","context_window":16384}],"has_more":false})
                } else {json!({"data":[{"id":"first","supported_parameters":[]}],"has_more":true,"last_id":"first"})};
                (axum::http::StatusCode::OK,Json(payload))
            }
        }
    }))).await;
    let root = std::env::temp_dir().join(format!(
        "aubridge-directory-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let config = root.join("config.json");
    std::fs::write(&config, r#"{"future":{"keep":true}}"#).unwrap();
    let database = root.join("ledger.sqlite");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let make_state = || {
        Arc::new(
            GatewayState::with_keys(
                BTreeMap::from([("vendor".into(), manifest(&upstream))]),
                "fixture",
                BTreeMap::from([("vendor".into(), "synthetic-key".into())]),
            )
            .with_client(client.clone())
            .with_settings(config.clone(), root.join("stash"))
            .with_ledger(ledger::Ledger::open(&database).unwrap()),
        )
    };
    let state = make_state();
    let (base, gateway) = serve(gateway::app(state)).await;
    let initial = catalog(&client, &base, true).await;
    assert_eq!(initial["models"].as_array().unwrap().len(), 2);
    assert_eq!(initial["models"][0]["facts"]["tools"], false);
    assert_eq!(
        initial["models"][1]["facts"]["tools"],
        Value::Null,
        "missing capability isn't false"
    );
    let saved = client
        .post(format!("{base}/admin/providers/vendor/catalog-selection"))
        .bearer_auth("fixture-admin")
        .json(&json!({"models":["last"]}))
        .send()
        .await
        .unwrap();
    assert!(
        saved.status().is_success(),
        "{}",
        saved.text().await.unwrap()
    );
    let document: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(document["future"]["keep"], true);
    fail.store(true, Ordering::Relaxed);
    let failed = catalog(&client, &base, true).await;
    assert_eq!(failed["issue"], "authentication");
    assert_eq!(
        failed["models"], initial["models"],
        "partial pages replaced the complete directory"
    );
    assert_eq!(failed["fetched_at"], initial["fetched_at"]);
    gateway.abort();
    let restarted = make_state();
    assert_eq!(restarted.manifest("vendor").unwrap().free_models, ["last"]);
    assert_eq!(
        restarted.manifest("vendor").unwrap().context_windows["last"],
        16384
    );
    let (base, gateway) = serve(gateway::app(restarted)).await;
    assert_eq!(
        catalog(&client, &base, false).await["models"],
        initial["models"]
    );
    gateway.abort();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_late_directory_and_previous_query_error_do_not_cross_key_identities() {
    use axum::http::HeaderMap;
    use tokio::sync::Notify;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (upstream, server) = serve(Router::new().route(
        "/v1/models",
        get({
            let entered = entered.clone();
            let release = release.clone();
            move |headers: HeaderMap| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    let old = headers["authorization"]
                        .to_str()
                        .unwrap()
                        .ends_with("key-a");
                    if old {
                        entered.notify_one();
                        release.notified().await;
                    }
                    if old {
                        (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error":"old denied"})),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            Json(json!({"data":[{"id":"new-only"}]})),
                        )
                    }
                }
            }
        }),
    ))
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("vendor".into(), manifest(&upstream))]),
            "fixture",
            BTreeMap::from([("vendor".into(), "synthetic-key-a".into())]),
        )
        .with_client(client.clone())
        .with_key_store(Arc::new(credentials::MemoryStore::default()))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, gateway) = serve(gateway::app(state.clone())).await;
    let pending = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/admin/providers/vendor/catalog"))
                .bearer_auth("fixture-admin")
                .send()
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    state.set_provider_key("vendor", "synthetic-key-b").unwrap();
    release.notify_one();
    assert_eq!(pending.await.unwrap().status(), 409);
    let current = catalog(&client, &base, false).await;
    assert_eq!(current["models"], json!([]));
    assert_eq!(current["issue"], Value::Null);
    let fresh = catalog(&client, &base, true).await;
    assert_eq!(fresh["models"][0]["id"], "new-only");
    assert!(!fresh.to_string().contains("synthetic"));
    gateway.abort();
    server.abort();
}

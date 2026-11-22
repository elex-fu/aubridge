//! Core polling must work without GUI reads and expose actual window metadata.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{routing::get, Json, Router};
use gateway::state::GatewayState;
use serde_json::{json, Value};
use tokio::sync::Notify;

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn fixture() -> (
    Arc<GatewayState>,
    Arc<Notify>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let entered = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let signal = entered.clone();
    let count = calls.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let app = Router::new().route("/quota", get(move || {
        let signal = signal.clone();
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            signal.notify_one();
            Json(json!({"data":{"limits":[
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":20,"nextResetTime":(now+3600)*1000},
                {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":80,"nextResetTime":(now+86400)*1000}
            ]}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut manifest = registry::zhipu();
    manifest.id = "quota-poll-fixture".into();
    manifest.kind = manifest::schema::ProviderKind::Subscription;
    let base = manifest.provider.base.clone();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([(manifest.id.clone(), manifest)]),
            "aubridge",
            BTreeMap::from([("quota-poll-fixture".into(), "synthetic-key".into())]),
        )
        .with_client(client())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap())
        .with_plan_fetcher_for(
            &base,
            Arc::new(credentials::planquota::PlanFetcher::new(
                url,
                credentials::planquota::PlanAuth::RawKey,
            )),
        ),
    );
    (state, entered, calls, upstream)
}

async fn serve(state: Arc<GatewayState>) -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        gateway::serve(state, 0, move |port| {
            let _ = tx.send(port);
        })
        .await
    });
    let port = rx.await.unwrap();
    (format!("http://127.0.0.1:{port}"), server)
}

#[tokio::test]
async fn core_samples_quota_without_any_admin_or_gui_request() {
    let (state, entered, calls, upstream) = fixture().await;
    let (_base, server) = serve(state.clone()).await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("core never sampled quota while no GUI/admin request was made");
    tokio::time::timeout(Duration::from_secs(2), async {
        while state
            .quota_history(Some("quota-poll-fixture"), 0)
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background read did not reach SQLite history");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn providers_expose_each_window_reset_and_cached_read_time() {
    let (state, _entered, calls, upstream) = fixture().await;
    // Use app directly here so this case independently checks management reads.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, gateway::app(state)).await.unwrap() });
    let mut first_at = None;
    for _ in 0..2 {
        let body: Value = client()
            .get(format!("{base}/admin/providers"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let quota = &body["providers"][0]["quota"];
        let windows = quota["windows"]
            .as_array()
            .expect("missing per-window quota metadata");
        assert_eq!(windows.len(), 2);
        let short = windows.iter().find(|w| w["id"] == "5h").unwrap();
        let week = windows.iter().find(|w| w["id"] == "7d").unwrap();
        assert_eq!(short["used_percent"], json!(20.0));
        assert_eq!(week["used_percent"], json!(80.0));
        assert_ne!(short["resets_at"], week["resets_at"]);
        assert_eq!(quota["stale"], false);
        assert_eq!(quota["issue"], Value::Null);
        let observed = quota["observed_at"].as_u64().unwrap();
        if let Some(first) = first_at {
            assert_eq!(
                observed, first,
                "cache read fabricated a new observation time"
            );
        }
        first_at = Some(observed);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn providers_sample_zero_never_polls_and_default_get_still_samples() {
    let (state, _entered, calls, upstream) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, gateway::app(state)).await.unwrap() });
    for _ in 0..3 {
        let reply: Value = client()
            .get(format!("{base}/admin/providers?sample=0"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply["providers"][0]["id"], "quota-poll-fixture");
        assert!(reply["openrouter_key_budget"].is_null());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    client()
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for _ in 0..2 {
        client()
            .get(format!("{base}/admin/providers?sample=0"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn providers_sample_zero_also_skips_openrouter_budget_http() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let budget_url = format!("http://{}/budget", upstream_listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let app = Router::new().route(
            "/budget",
            get(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({"data":{"limit":10,"limit_remaining":4}})) }
            }),
        );
        axum::serve(upstream_listener, app).await.unwrap();
    });
    let state = GatewayState::with_keys(
        BTreeMap::from([("openrouter".into(), registry::openrouter())]),
        "aubridge",
        BTreeMap::from([("openrouter".into(), "synthetic-key".into())]),
    )
    .with_client(client())
    .with_openrouter_endpoint(budget_url);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, gateway::app(Arc::new(state)))
            .await
            .unwrap()
    });
    for _ in 0..3 {
        client()
            .get(format!("{base}/admin/providers?sample=0"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let reply: Value = client()
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reply["openrouter_key_budget"]["limit_remaining"], 4.0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    upstream.abort();
}

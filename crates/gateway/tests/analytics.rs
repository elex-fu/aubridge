use gateway::{app, state::GatewayState};
use std::{collections::BTreeMap, sync::Arc};

async fn server(ledger: Option<ledger::Ledger>) -> (String, tokio::task::JoinHandle<()>) {
    let mut state = GatewayState::from_manifests(BTreeMap::new(), "analytics-test");
    if let Some(ledger) = ledger {
        state = state.with_ledger(ledger);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app(Arc::new(state))).await.unwrap();
    });
    (endpoint, task)
}

#[tokio::test]
async fn analytics_auth_strict_query_and_empty_are_distinct() {
    let (endpoint, task) = server(Some(ledger::Ledger::open(":memory:").unwrap())).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let query = "start=2020-01-01&end=2020-01-02";
    for key in [None, Some("analytics-test"), Some("analytics-test-codex")] {
        let mut request = client.get(format!("{endpoint}/admin/analytics?{query}"));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        assert_eq!(request.send().await.unwrap().status(), 401);
    }
    for query in [
        "",
        "start=2020-01-01",
        "start=2020-1-01&end=2020-01-02",
        "start=2020-02-30&end=2020-03-02",
        "start=2020-01-01&end=2020-01-01",
        "start=2020-01-01&end=2020-02-02",
        "start=9999-01-01&end=9999-01-02",
        "start=2020-01-01&end=2020-01-02&compare=next",
        "start=2020-01-01&end=2020-01-02&provider=secret",
        "start=2020-01-01&end=2020-01-02&start=2020-01-01",
        "start=2020-01-01&end=2020-01-02&compare=previous&compare=previous",
    ] {
        let response = client
            .get(format!("{endpoint}/admin/analytics?{query}"))
            .bearer_auth("analytics-test-admin")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "query={query}");
    }
    let response = client
        .get(format!(
            "{endpoint}/admin/analytics?{query}&compare=previous"
        ))
        .bearer_auth("analytics-test-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["summary"]["requests"]["total"], 0);
    assert!(body["summary"]["requests"]["success_rate"].is_null());
    assert_eq!(body["window"]["scope"], "retained_ledger");
    assert_eq!(
        body["comparison"]["requests_change"]["reason"],
        "retention_unknown"
    );
    task.abort();
    let (endpoint, task) = server(None).await;
    let response = client
        .get(format!("{endpoint}/admin/analytics?{query}"))
        .bearer_auth("analytics-test-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert!(!body.contains("SELECT") && !body.contains("sqlite"));
    task.abort();
}

#[tokio::test]
async fn dated_history_filters_before_limit_and_preserves_cursor_and_legacy_query() {
    let ledger = ledger::Ledger::open(":memory:").unwrap();
    let window = ledger
        .analytics_window("2020-01-01", "2020-01-02", 2_000_000_000)
        .unwrap();
    let rows: Vec<_> = (0..26)
        .map(|index| ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                ts: if index == 25 {
                    window.end_ts
                } else {
                    window.start_ts
                },
                agent: "codex".into(),
                request_id: Some(format!("r{index}")),
                model: "model".into(),
                outcome: "ok".into(),
                attempts: "[]".into(),
                ..Default::default()
            },
            usage: vec![],
        })
        .collect();
    ledger.record_requests(&rows).unwrap();
    let (endpoint, task) = server(Some(ledger)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("{endpoint}/admin/gateway?start=2020-01-01&end=2020-01-02&agent=codex");
    let first: serde_json::Value = client
        .get(&base)
        .bearer_auth("analytics-test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["requests"].as_array().unwrap().len(), 20);
    assert!(first["requests"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["request_id"] != "r25"));
    let cursor = first["next"].as_str().unwrap();
    let second: serde_json::Value = client
        .get(format!("{base}&before={cursor}"))
        .bearer_auth("analytics-test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["requests"].as_array().unwrap().len(), 5);
    assert!(second["next"].is_null());
    for query in [
        "start=2020-01-01",
        "end=2020-01-02",
        "start=bad&end=2020-01-02",
    ] {
        assert_eq!(
            client
                .get(format!("{endpoint}/admin/gateway?{query}"))
                .bearer_auth("analytics-test-admin")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let legacy: serde_json::Value = client
        .get(format!("{endpoint}/admin/gateway?request_id=r25"))
        .bearer_auth("analytics-test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(legacy["requests"].as_array().unwrap().len(), 1);
    task.abort();
}

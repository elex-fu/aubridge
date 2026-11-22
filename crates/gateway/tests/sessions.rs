use gateway::{app, state::GatewayState};
use std::{collections::BTreeMap, sync::Arc};

#[tokio::test]
async fn sessions_are_admin_only_and_timeline_still_filters_request_id() {
    let ledger = ledger::Ledger::open(":memory:").unwrap();
    let rows = ["r1", "r2"]
        .into_iter()
        .map(|id| ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                ts: 1,
                agent: "codex".into(),
                session_id: Some("thread".into()),
                request_id: Some(id.into()),
                model: "model".into(),
                outcome: "ok".into(),
                attempts: "[]".into(),
                duration_ms: Some(10),
                ..Default::default()
            },
            usage: vec![],
        })
        .collect::<Vec<_>>();
    ledger.record_requests(&rows).unwrap();
    let state =
        Arc::new(GatewayState::from_manifests(BTreeMap::new(), "aubridge").with_ledger(ledger));
    let token = state.admin_token().to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app(state)).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let endpoint = format!("http://{address}");
    let denied = client
        .get(format!("{endpoint}/admin/gateway/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let sessions: serde_json::Value = client
        .get(format!(
            "{endpoint}/admin/gateway/sessions?agent=codex&session_id=thread"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"][0]["requests"], 2);
    assert_eq!(sessions["sessions"][0]["duration_ms"], 20);
    let page: serde_json::Value = client
        .get(format!(
            "{endpoint}/admin/gateway?agent=codex&session_id=thread&request_id=r2"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["requests"].as_array().unwrap().len(), 1);
    assert_eq!(page["requests"][0]["request_id"], "r2");
    assert_eq!(page["requests"][0]["session_id"], "thread");
    let bad = client
        .get(format!("{endpoint}/admin/gateway/sessions?before=invalid"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(
        client
            .delete(format!("{endpoint}/admin/gateway/history"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let empty: serde_json::Value = client
        .get(format!("{endpoint}/admin/gateway/sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["sessions"].as_array().unwrap().is_empty());
    server.abort();
}

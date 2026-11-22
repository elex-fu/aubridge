//! Synthetic protocol contract tests; real Codex acceptance is a separate script.
#[allow(dead_code)]
mod support;
use gateway::state::GatewayState;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
use support::replay::{spawn, Step};

fn checkpoint() -> Value {
    json!({"type":"compaction","encrypted_content":"opaque-fixture","id":"cmp_fixture"})
}
fn seal() -> String {
    [json!({"type":"response.output_item.done","output_index":0,"item":checkpoint()}),
     json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","output":[checkpoint()],"usage":{"input_tokens":6,"output_tokens":2}}})]
    .iter().map(|value| proto::sse::frame_event(value["type"].as_str().unwrap(), &value.to_string())).collect()
}
async fn gateway(base: &str, key: &str, ledger: ledger::Ledger) -> (Arc<GatewayState>, String) {
    let manifest = serde_json::from_value(json!({"id":"native","version":"1","kind":"free_tier","free_models":["model"],"provider":{"base":base,"auth":"bearer","protocol":"responses"}})).unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("native".into(), manifest)]),
            "aubridge",
            BTreeMap::from([("native".into(), key.into())]),
        )
        .with_ledger(ledger),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = gateway::app(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, url)
}
async fn post(base: &str, path: &str, request: Value) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/responses{path}"))
        .bearer_auth("aubridge-codex")
        .json(&request)
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn unary_checkpoint_persists_across_gateway_restart_and_changed_key_is_zero_outbound() {
    let (server, upstream, _) = spawn(vec![Step::Fail {
        status: 200,
        body: json!({"output":[checkpoint()],"usage":{"input_tokens":3,"output_tokens":1}})
            .to_string(),
    }])
    .await;
    let path = std::env::temp_dir().join(format!(
        "aubridge-checkpoint-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (state, base) = gateway(&upstream, "key-a", ledger::Ledger::open(&path).unwrap()).await;
    let result = post(
        &base,
        "/compact",
        json!({"model":"model","input":"remember fact"}),
    )
    .await;
    assert_eq!(result.status(), 200);
    assert_eq!(
        result.json::<Value>().await.unwrap()["output"][0],
        checkpoint()
    );
    assert_eq!(
        state.recent_decisions().unwrap()[0].purpose.as_deref(),
        Some("compact")
    );
    let (_, changed) = gateway(&upstream, "key-b", ledger::Ledger::open(&path).unwrap()).await;
    let result = post(&changed,"",json!({"model":"model","stream":true,"input":[checkpoint(),{"role":"user","content":"recall"}]})).await;
    assert_eq!(result.status(), 409);
    assert_eq!(server.seen_bodies.lock().await.len(), 1);
    let _ = std::fs::remove_file(path);
}
#[tokio::test]
async fn v2_returns_sealed_events_and_invalid_response_backs_off_only_compaction() {
    let (server, upstream, _) = spawn(vec![
        Step::Sse { body: seal() },
        Step::Sse {
            body: proto::sse::frame_event(
                "response.completed",
                &json!({"type":"response.completed","response":{"status":"completed","output":[]}})
                    .to_string(),
            ),
        },
        Step::Sse { body: seal() },
    ])
    .await;
    let (state, base) = gateway(
        &upstream,
        "key-a",
        ledger::Ledger::open(":memory:").unwrap(),
    )
    .await;
    let request = json!({"model":"model","stream":true,"input":[{"role":"user","content":"fact"},{"type":"compaction_trigger"}]});
    let result = post(&base, "", request.clone()).await;
    assert_eq!(result.status(), 200);
    assert!(result.text().await.unwrap().contains("opaque-fixture"));
    assert_eq!(
        state.recent_decisions().unwrap()[0].purpose.as_deref(),
        Some("compact")
    );
    let result = post(&base, "", request.clone()).await;
    assert_eq!(result.status(), 502);
    let result = post(&base, "", request).await;
    assert_eq!(result.status(), 429);
    assert_eq!(server.seen_bodies.lock().await.len(), 2);
    // An ordinary request still dispatches under the isolated compaction backoff.
    let result = post(
        &base,
        "",
        json!({"model":"model","stream":true,"input":"hello"}),
    )
    .await;
    let _ = result.text().await.unwrap();
    assert_eq!(server.seen_bodies.lock().await.len(), 3);
    assert!(state.recent_decisions().unwrap()[0].purpose.is_none());
}
#[tokio::test]
async fn unknown_history_and_unauthorized_compaction_never_dispatch() {
    let (server, upstream, _) = spawn(vec![]).await;
    let (_, base) = gateway(
        &upstream,
        "key-a",
        ledger::Ledger::open(":memory:").unwrap(),
    )
    .await;
    let result = post(
        &base,
        "",
        json!({"model":"model","stream":true,"input":[checkpoint()]}),
    )
    .await;
    assert_eq!(result.status(), 409);
    let result = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/responses/compact"))
        .json(&json!({"model":"model","input":"hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 401);
    assert!(server.seen_bodies.lock().await.is_empty());
}

#[tokio::test]
async fn compact_bypasses_inference_tool_loop_guard() {
    let (server, upstream, _) = spawn(vec![Step::Fail {
        status: 200,
        body: json!({"output":[checkpoint()]}).to_string(),
    }])
    .await;
    let (_, base) = gateway(&upstream, "key", ledger::Ledger::open(":memory:").unwrap()).await;
    let mut history = vec![json!({"role":"user","content":"read"})];
    for index in 0..8 {
        history.push(json!({"type":"function_call","name":"read","call_id":format!("call_{index}"),"arguments":"{\"path\":\"fixture\"}"}));
        history.push(json!({"type":"function_call_output","call_id":format!("call_{index}"),"output":"contents"}));
    }
    let result = post(&base,"/compact",json!({"model":"model","input":history,"tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}]})).await;
    assert_eq!(result.status(), 200);
    assert_eq!(
        result.json::<Value>().await.unwrap()["output"][0],
        checkpoint()
    );
    assert_eq!(server.seen_bodies.lock().await.len(), 1);
}

#[tokio::test]
async fn refresh_switch_rebinds_new_checkpoints_but_never_replays_sealed_history() {
    for sealed in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "aubridge-refresh-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let auth = root.join("auth.json");
        std::fs::write(
            &auth,
            r#"{"tokens":{"access_token":"initial","refresh_token":"r1","account_id":"first"}}"#,
        )
        .unwrap();
        let switched =
            r#"{"tokens":{"access_token":"initial","refresh_token":"r2","account_id":"second"}}"#;
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let route = axum::Router::new().route(
            "/oauth/token",
            axum::routing::post({
                let started = started.clone();
                let release = release.clone();
                move || {
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        started.notify_one();
                        release.notified().await;
                        axum::Json(json!({"access_token":"late", "refresh_token":"late-refresh"}))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refresh_url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let refresh_task = tokio::spawn(async move { axum::serve(listener, route).await.unwrap() });
        let second_checkpoint =
            json!({"type":"compaction","encrypted_content":"new-checkpoint-after-refresh"});
        let checkpoint_event = proto::sse::frame_event(
            "response.output_item.done",
            &json!({
                "type":"response.output_item.done","output_index":1,"item":second_checkpoint
            })
            .to_string(),
        );
        let reply = include_str!("../../translate/tests/fixtures/codex_text.sse").replace(
            "event: response.completed",
            &(checkpoint_event + "event: response.completed"),
        );
        let (upstream, upstream_url, upstream_task) = spawn(vec![
            Step::Fail {
                status: 200,
                body: json!({"output":[checkpoint()]}).to_string(),
            },
            Step::Fail {
                status: 401,
                body: r#"{"error":"token_expired"}"#.into(),
            },
            Step::Sse { body: reply },
        ])
        .await;
        let manifest = serde_json::from_value(json!({"id":"chatgpt","version":"1","kind":"subscription",
            "free_models":["gpt-6.1-sol"],"provider":{"base":upstream_url,"auth":"bearer","protocol":"responses"},
            "permissions":{"net":["127.0.0.1"],"store_key":false}})).unwrap();
        let channel = gateway::chatgpt::ChatGptChannel::new(auth.clone(), root.join("config.toml"))
            .with_token_endpoint(refresh_url)
            .with_responses_base(upstream_url);
        channel.set_share_login(true);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let path = root.join("ledger.sqlite");
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([("chatgpt".into(), manifest)]),
                "aubridge",
            )
            .with_codex_channel(channel)
            .with_client(client.clone())
            .with_ledger(ledger::Ledger::open(&path).unwrap()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = gateway::app(state);
        let gateway_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = post(
            &base,
            "/compact",
            json!({"model":"gpt-6.1-sol","input":"seed"}),
        )
        .await;
        assert_eq!(response.status(), 200);
        let _ = response.bytes().await.unwrap();
        let reader = ledger::Ledger::open(&path).unwrap();
        let old_digest = credentials::quota::subject_tag("compaction-checkpoint", "opaque-fixture");
        let old_owner = reader.compaction_owner(&old_digest).unwrap().unwrap();
        let input = if sealed {
            json!([checkpoint(),{"role":"user","content":"continue"}])
        } else {
            json!([{"role":"user","content":"ordinary request"}])
        };
        let pending = tokio::spawn(async move {
            post(
                &base,
                "",
                json!({"model":"gpt-6.1-sol","stream":true,"input":input}),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        std::fs::write(&auth, switched).unwrap();
        release.notify_one();
        let response = tokio::time::timeout(std::time::Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        let status = response.status();
        let output = response.text().await.unwrap();
        let new_digest = credentials::quota::subject_tag(
            "compaction-checkpoint",
            "new-checkpoint-after-refresh",
        );
        let headers = upstream.seen_headers.lock().await;
        if sealed {
            assert!(!status.is_success());
            assert_eq!(
                headers.len(),
                2,
                "sealed A history must not be replayed under B"
            );
            assert!(headers.iter().all(|h| h["chatgpt-account-id"] == "first"));
            assert!(reader.compaction_owner(&new_digest).unwrap().is_none());
        } else {
            assert_eq!(status, 200, "{output}");
            assert_eq!(headers.len(), 3);
            assert_eq!(headers[2]["chatgpt-account-id"], "second");
            assert_eq!(headers[2]["authorization"], "Bearer initial");
            let new_owner = reader.compaction_owner(&new_digest).unwrap().unwrap();
            assert_ne!(
                new_owner, old_owner,
                "B checkpoint must not retain A dispatch ownership"
            );
        }
        assert_eq!(
            reader.compaction_owner(&old_digest).unwrap().as_ref(),
            Some(&old_owner)
        );
        assert_eq!(
            std::fs::read_to_string(&auth).unwrap(),
            switched,
            "late A refresh must not overwrite B"
        );
        for task in [gateway_task, refresh_task, upstream_task] {
            task.abort();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

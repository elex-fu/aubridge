//! Google ingress exercises the production candidate loop.

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::{json, Value};
use support::replay::{spawn, Step};

const ANTHROPIC_TOOL: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_a\",\"model\":\"plan-model\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":25}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_a\",\"name\":\"read\",\"input\":{}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file\\\":\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"fixture\\\"}\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":30}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

fn source(id: &str, base: &str, model: &str, kind: &str, protocol: &str) -> (String, Manifest) {
    (
        id.into(),
        serde_json::from_value(json!({
            "id":id, "version":"1", "kind":kind, "free_models":[model],
            "provider":{"base":base, "auth":"bearer", "protocol":protocol},
        }))
        .unwrap(),
    )
}

async fn gateway(
    manifests: impl IntoIterator<Item = (String, Manifest)>,
) -> (reqwest::Client, String) {
    let state = GatewayState::from_manifests(
        manifests.into_iter().collect::<BTreeMap<_, _>>(),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, gateway::app(Arc::new(state)))
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap();
    (client, base)
}

#[tokio::test]
async fn google_auth_catalog_actions_and_opaque_rejection() {
    let (server, base, _) = spawn(vec![]).await;
    let model = "vendor/model:free";
    let (client, gw) = gateway([source("free", &base, model, "free_tier", "chat")]).await;
    let response = client
        .get(format!("{gw}/v1beta/models?key=aubridge-gemini"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let catalog: Value = response.json().await.unwrap();
    assert!(catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == format!("models/{model}")));
    let count = client
        .post(format!("{gw}/v1beta/models/{model}:countTokens"))
        .header("x-goog-api-key", "aubridge-gemini")
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(count.status(), 200);
    let count: Value = count.json().await.unwrap();
    assert_eq!(count["estimated"], true);
    assert!(count["totalTokens"].as_u64().unwrap() > 0);
    for body in [
        json!({"cachedContent":"cachedContents/opaque","contents":[]}),
        json!({"contents":[{"parts":[{"text":"secret","thoughtSignature":"opaque"}]}]}),
        json!({"contents":[{"parts":[{"fileData":{"fileUri":"files/opaque","mimeType":"text/plain"}}]}]}),
    ] {
        let response = client
            .post(format!("{gw}/v1beta/models/{model}:generateContent"))
            .header("x-goog-api-key", "aubridge-gemini")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["status"],
            "INVALID_ARGUMENT"
        );
    }
    let conflict = client
        .get(format!("{gw}/v1beta/models?key=aubridge-gemini"))
        .header("x-goog-api-key", "other")
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 401);
    let unknown = client
        .post(format!("{gw}/v1beta/models/gemini-unknown:generateContent"))
        .bearer_auth("aubridge-gemini")
        .json(&json!({"contents":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    assert!(server.seen_bodies.lock().await.is_empty());
}

#[tokio::test]
async fn google_stream_and_unary_use_production_tools_usage_and_data_frames() {
    let (server, base, _) = spawn(vec![
        Step::Sse {
            body: ANTHROPIC_TOOL.into(),
        },
        Step::Sse {
            body: ANTHROPIC_TOOL.into(),
        },
    ])
    .await;
    let (client, gw) = gateway([source(
        "free",
        &base,
        "plan-model",
        "free_tier",
        "anthropic",
    )])
    .await;
    let request = json!({"contents":[{"role":"user","parts":[{"text":"read fixture"}]}],
        "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"object","properties":{"file":{"type":"string"}}}}]}]});
    let unary = client
        .post(format!(
            "{gw}/v1beta/models/plan-model:generateContent?key=aubridge-gemini"
        ))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(unary.status(), 200);
    let unary: Value = unary.json().await.unwrap();
    assert_eq!(
        unary["candidates"][0]["content"]["parts"][0]["functionCall"]["name"],
        "read"
    );
    assert_eq!(unary["usageMetadata"]["promptTokenCount"], 25);
    assert_eq!(unary["usageMetadata"]["candidatesTokenCount"], 30);
    let stream = client
        .post(format!(
            "{gw}/v1beta/models/plan-model:streamGenerateContent?alt=sse"
        ))
        .header("x-goog-api-key", "aubridge-gemini")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    assert!(stream.headers().contains_key("x-aubridge-request-id"));
    let raw = stream.text().await.unwrap();
    assert!(!raw.contains("event:"));
    assert!(!raw.contains("[DONE]"));
    let values: Vec<Value> = proto::sse::parse(&raw)
        .0
        .into_iter()
        .map(|e| serde_json::from_str(&e.data).unwrap())
        .collect();
    assert!(values
        .iter()
        .any(|v| v["usageMetadata"]["candidatesTokenCount"] == 30));
    assert!(values.iter().any(
        |v| v["candidates"][0]["content"]["parts"][0]["functionCall"]["args"]["file"] == "fixture"
    ));
    assert_eq!(server.seen_bodies.lock().await.len(), 2);
}

#[tokio::test]
async fn google_tool_response_order_and_midstream_error_are_preserved() {
    let broken = ANTHROPIC_TOOL.replace(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "",
    );
    let (server, base, _) = spawn(vec![Step::Sse { body: broken }]).await;
    let (client, gw) = gateway([source(
        "free",
        &base,
        "plan-model",
        "free_tier",
        "anthropic",
    )])
    .await;
    let response=client.post(format!("{gw}/v1beta/models/plan-model:streamGenerateContent"))
        .bearer_auth("aubridge-gemini").json(&json!({"contents":[
            {"role":"user","parts":[{"text":"read"}]},
            {"role":"model","parts":[{"functionCall":{"id":"first","name":"read","args":{}}},{"functionCall":{"id":"second","name":"read","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"id":"second","name":"read","response":{"result":"B"}}},{"functionResponse":{"id":"first","name":"read","response":{"result":"A"}}},{"text":"continue"}]}
        ]})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let raw = response.text().await.unwrap();
    assert!(raw.contains("\"error\""));
    assert!(!raw.contains("\"finishReason\":\"STOP\""));
    assert!(!raw.contains("event:"));
    assert!(!raw.contains("[DONE]"));
    let seen = server.seen_bodies.lock().await;
    let sent: Value = serde_json::from_str(&seen[0]).unwrap();
    let parts = sent["messages"][2]["content"].as_array().unwrap();
    assert_eq!(parts[0]["tool_use_id"], "second");
    assert_eq!(parts[1]["tool_use_id"], "first");
    assert_eq!(parts[2]["text"], "continue");
}

async fn last_row(client: &reqwest::Client, gw: &str) -> Value {
    let rows: Value = client
        .get(format!("{gw}/admin/gateway"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    rows["requests"][0].clone()
}

#[tokio::test]
async fn google_codec_failure_aborts_upstream_and_records_failure() {
    let bad=concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"bad\",\"model\":\"plan-model\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"committed\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"read\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{bad\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    let (_, base, _) = spawn(vec![Step::Sse { body: bad.into() }]).await;
    let (client, gw) = gateway([source(
        "free",
        &base,
        "plan-model",
        "free_tier",
        "anthropic",
    )])
    .await;
    let raw = client
        .post(format!(
            "{gw}/v1beta/models/plan-model:streamGenerateContent"
        ))
        .bearer_auth("aubridge-gemini")
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(raw.contains("committed"));
    assert_eq!(raw.matches("\"error\"").count(), 1);
    assert!(!raw.contains("\"finishReason\":\"STOP\""));
    assert_eq!(last_row(&client, &gw).await["outcome"], "stream-error");
}

#[tokio::test]
async fn google_silent_interval_and_client_cancel_release_source() {
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let signal = dropped.clone();
    let app=axum::Router::new().route("/messages",axum::routing::post(move || {
        let signal=signal.clone(); async move {
            let prefix=concat!(": keepalive\n\n",
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"quiet\",\"model\":\"plan-model\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"before quiet\"}}\n\n");
            let suffix=concat!("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
            let chunks=vec![prefix.to_owned(),suffix.to_owned()];
            let stream=futures_util::stream::unfold((chunks.into_iter(),DropFlag(signal),false),|(mut chunks,guard,delayed)|async move{
                if delayed {tokio::time::sleep(Duration::from_millis(5200)).await;}
                chunks.next().map(|chunk|(Ok::<_,std::io::Error>(chunk),(chunks,guard,true)))
            });
            axum::http::Response::builder().header("content-type","text/event-stream").body(axum::body::Body::from_stream(stream)).unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (client, gw) = gateway([source(
        "free",
        &base,
        "plan-model",
        "free_tier",
        "anthropic",
    )])
    .await;
    let response = client
        .post(format!(
            "{gw}/v1beta/models/plan-model:streamGenerateContent"
        ))
        .bearer_auth("aubridge-gemini")
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
        .send()
        .await
        .unwrap();
    let raw = response.text().await.unwrap();
    assert!(raw.contains("STOP"), "{raw}");
    assert!(!raw.contains("error"));
    assert!(!raw.contains(": keepalive"));
    dropped.store(false, Ordering::SeqCst);
    let response = client
        .post(format!(
            "{gw}/v1beta/models/plan-model:streamGenerateContent"
        ))
        .bearer_auth("aubridge-gemini")
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("before quiet"));
    drop(stream);
    for _ in 0..50 {
        if dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn google_upstream_exhaustion_is_google_http_error_for_both_operations() {
    for operation in ["generateContent", "streamGenerateContent"] {
        let (_, base, _) = spawn(vec![Step::Fail {
            status: 500,
            body: "upstream failure".into(),
        }])
        .await;
        let (client, gw) = gateway([source(
            "free",
            &base,
            "plan-model",
            "free_tier",
            "anthropic",
        )])
        .await;
        let response = client
            .post(format!("{gw}/v1beta/models/plan-model:{operation}"))
            .bearer_auth("aubridge-gemini")
            .json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], 502);
        assert_eq!(body["error"]["status"], "UNAVAILABLE");
    }
}

#[tokio::test]
async fn google_chat_and_responses_share_canonical_tools_and_raw_usage() {
    for (protocol, fixture) in [
        (
            "chat",
            include_str!("../../translate/tests/fixtures/chat_tool_call.sse"),
        ),
        (
            "responses",
            include_str!("../../translate/tests/fixtures/codex_tool.sse"),
        ),
    ] {
        let (server, base, _) = spawn(vec![Step::Sse {
            body: fixture.into(),
        }])
        .await;
        let (client, gw) =
            gateway([source("free", &base, "plan-model", "free_tier", protocol)]).await;
        let response=client.post(format!("{gw}/v1beta/models/plan-model:generateContent"))
            .bearer_auth("aubridge-gemini").json(&json!({"contents":[{"role":"user","parts":[{"text":"read fixture"}]}],
                "generationConfig":{"temperature":0.2,"topP":0.8},
                "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"object"}}]}]}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200, "{protocol}");
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["candidates"][0]["finishReason"], "STOP", "{reply}");
        assert!(
            reply["candidates"][0]["content"]["parts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|part| part.get("functionCall").is_some()),
            "{reply}"
        );
        assert!(
            reply["usageMetadata"]["promptTokenCount"].as_u64().unwrap() > 0,
            "{reply}"
        );
        let sent: Value = serde_json::from_str(&server.seen_bodies.lock().await[0]).unwrap();
        assert_eq!(sent["temperature"], 0.2);
        assert_eq!(sent["top_p"], 0.8);
        let headers = server.seen_headers.lock().await;
        assert!(!headers[0].contains_key("x-goog-api-key"));
        assert_ne!(
            headers[0]
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer aubridge-gemini")
        );
    }
}

#[tokio::test]
async fn google_top_k_on_generic_chat_or_responses_rejects_before_outbound() {
    for protocol in ["chat", "responses"] {
        let (server, base, _) = spawn(vec![]).await;
        let (client, gw) =
            gateway([source("custom", &base, "plan-model", "free_tier", protocol)]).await;
        for action in ["generateContent", "streamGenerateContent", "countTokens"] {
            let response=client.post(format!("{gw}/v1beta/models/plan-model:{action}"))
                .bearer_auth("aubridge-gemini").json(&json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}],
                    "generationConfig":{"topK":64,"topP":0.95,"temperature":1,"thinkingConfig":{"includeThoughts":true}}}))
                .send().await.unwrap();
            assert_eq!(response.status(), 400, "{protocol}/{action}");
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["status"],
                "INVALID_ARGUMENT"
            );
        }
        assert!(server.seen_bodies.lock().await.is_empty());
    }
}

#[tokio::test]
async fn google_streamed_responses_tools_accept_canonical_null_start_then_complete_args() {
    let fixture = include_str!("../../translate/tests/fixtures/codex_tool.sse");
    let (_, base, _) = spawn(vec![Step::Sse {
        body: fixture.into(),
    }])
    .await;
    let (client, gw) = gateway([source(
        "free",
        &base,
        "plan-model",
        "free_tier",
        "responses",
    )])
    .await;
    let response=client.post(format!("{gw}/v1beta/models/plan-model:streamGenerateContent"))
        .bearer_auth("aubridge-gemini").json(&json!({"contents":[{"role":"user","parts":[{"text":"weather"}]}],
            "tools":[{"functionDeclarations":[{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let raw = response.text().await.unwrap();
    let frames: Vec<Value> = proto::sse::parse(&raw)
        .0
        .into_iter()
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    assert!(
        !frames.iter().any(|frame| frame.get("error").is_some()),
        "unexpected Google error instead of assembled tool"
    );
    let calls: Vec<&Value> = frames
        .iter()
        .flat_map(|frame| {
            frame["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("functionCall"))
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "get_weather");
    assert_eq!(calls[0]["args"]["city"], "Beijing");
    assert!(frames
        .iter()
        .any(|frame| frame["candidates"][0]["finishReason"] == "STOP"));
    assert!(frames
        .iter()
        .any(|frame| frame["usageMetadata"]["promptTokenCount"] == 67
            && frame["usageMetadata"]["candidatesTokenCount"] == 19));
    assert_eq!(last_row(&client, &gw).await["outcome"], "ok");
}

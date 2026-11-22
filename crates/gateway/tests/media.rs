//! Production HTTP preflight and outgoing attachment bytes.
#[allow(dead_code)]
mod support;
use gateway::state::GatewayState;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
use support::replay::{spawn, Step};

async fn setup(
    protocol: &str,
    image_input: Option<bool>,
) -> (
    reqwest::Client,
    String,
    Arc<support::replay::Replay>,
    tokio::task::JoinHandle<()>,
) {
    let anthropic = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"vision\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let reply = match protocol {
        "anthropic" => anthropic,
        "responses" => include_str!("../../translate/tests/fixtures/codex_text.sse"),
        _ => include_str!("../../translate/tests/fixtures/chat_tool_call.sse"),
    };
    let (replay, upstream, _) = spawn(vec![Step::Sse { body: reply.into() }]).await;
    let manifest = serde_json::from_value(json!({"id":"local","version":"1","free_models":["vision"],"provider":{"base":upstream,"auth":"bearer","protocol":protocol},"model_facts":{"vision":{"image_input":image_input}}})).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("local".into(), manifest)]),
            "fixture",
            BTreeMap::from([("local".into(), "synthetic".into())]),
        )
        .with_client(client.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, gateway::app(state)).await.unwrap();
    });
    (client, base, replay, task)
}
fn request(ingress: &str) -> Value {
    let url = "https://example.invalid/contract.png";
    match ingress {
        "messages" => {
            json!({"model":"vision","max_tokens":16,"stream":false,"messages":[{"role":"user","content":[{"type":"text","text":"before"},{"type":"image","source":{"type":"url","url":url}},{"type":"text","text":"after"}]}]})
        }
        "chat/completions" => {
            json!({"model":"vision","stream":true,"messages":[{"role":"user","content":[{"type":"text","text":"before"},{"type":"image_url","image_url":{"url":url}},{"type":"text","text":"after"}]}]})
        }
        _ => {
            json!({"model":"vision","stream":true,"input":[{"role":"user","content":[{"type":"input_text","text":"before"},{"type":"input_image","image_url":url},{"type":"input_text","text":"after"}]}]})
        }
    }
}
#[tokio::test]
async fn every_ingress_delivers_ordered_images_to_chat() {
    for ingress in ["messages", "chat/completions", "responses"] {
        let (client, base, replay, task) = setup("chat", Some(true)).await;
        let response = client
            .post(format!("{base}/v1/{ingress}"))
            .bearer_auth("fixture-ui-test")
            .json(&request(ingress))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "{ingress}: {}",
            response.text().await.unwrap()
        );
        let bodies = replay.seen_bodies.lock().await;
        assert_eq!(bodies.len(), 1);
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        let parts = &sent["messages"][0]["content"];
        assert_eq!(parts[0]["text"], "before");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "https://example.invalid/contract.png"
        );
        assert_eq!(parts[2]["text"], "after");
        task.abort();
    }
}
#[tokio::test]
async fn known_text_only_models_reject_all_ingresses_with_zero_outbound() {
    for ingress in ["messages", "chat/completions", "responses"] {
        let (client, base, replay, task) = setup("chat", Some(false)).await;
        let response = client
            .post(format!("{base}/v1/{ingress}"))
            .bearer_auth("fixture-ui-test")
            .json(&request(ingress))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("image-input-unsupported"));
        assert!(replay.seen_bodies.lock().await.is_empty());
        task.abort();
    }
}
#[tokio::test]
async fn invalid_base64_and_opaque_file_ids_never_reach_a_provider() {
    for source in [
        json!({"type":"base64","media_type":"image/png","data":"bad!"}),
        json!({"type":"base64","media_type":"image/jpeg","data":"iVBORw0KGgo="}),
        json!({"type":"file","file_id":"foreign-account-file"}),
    ] {
        let (client, base, replay, task) = setup("chat", Some(true)).await;
        let mut body = request("messages");
        body["messages"][0]["content"][1]["source"] = source;
        let response = client
            .post(format!("{base}/v1/messages"))
            .bearer_auth("fixture-ui-test")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(replay.seen_bodies.lock().await.is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn every_ingress_keeps_images_on_messages_and_responses_wires() {
    for protocol in ["anthropic", "responses"] {
        for ingress in ["messages", "chat/completions", "responses"] {
            let (client, base, replay, task) = setup(protocol, Some(true)).await;
            let response = client
                .post(format!("{base}/v1/{ingress}"))
                .bearer_auth("fixture-ui-test")
                .json(&request(ingress))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "{ingress} -> {protocol}: {}",
                response.text().await.unwrap()
            );
            let bodies = replay.seen_bodies.lock().await;
            assert_eq!(bodies.len(), 1);
            let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
            let parts = &sent[if protocol == "anthropic" {
                "messages"
            } else {
                "input"
            }][0]["content"];
            assert_eq!(parts[0]["text"], "before");
            assert_eq!(parts[2]["text"], "after");
            if protocol == "anthropic" {
                assert_eq!(
                    parts[1]["source"]["url"],
                    "https://example.invalid/contract.png"
                );
            } else {
                assert_eq!(
                    parts[1]["image_url"],
                    "https://example.invalid/contract.png"
                );
            }
            task.abort();
        }
    }
}

#[tokio::test]
async fn chat_media_cannot_interrupt_tool_results_or_render_them_as_user_text() {
    let (client, base, replay, task) = setup("chat", Some(true)).await;
    let mut body = request("messages");
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]}),
        json!({"role":"user","content":[{"type":"text","text":"intervening user turn"},{"type":"tool_result","tool_use_id":"a","content":"ok"}]}),
    ]);
    let response = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("attachment-wire-unsupported"));
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
}

#[tokio::test]
async fn oversized_attachment_uses_the_existing_wire_limit_with_zero_outbound() {
    let (client, base, replay, task) = setup("chat", Some(true)).await;
    let mut body = request("messages");
    body["messages"][0]["content"][1]["source"] =
        json!({"type":"base64","media_type":"image/png","data":"A".repeat(32 * 1024 * 1024)});
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&serde_json::to_vec(&body).unwrap())
        .unwrap();
    let compressed = encoder.finish().unwrap();
    let response = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .body(compressed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
}

#[tokio::test]
async fn count_estimates_the_attachment_wire_and_invalid_sources_do_not_become_estimates() {
    let (client, base, replay, task) = setup("chat", Some(true)).await;
    let with_image = request("messages");
    let mut text_only = with_image.clone();
    text_only["messages"][0]["content"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut counts = Vec::new();
    for body in [text_only, with_image.clone()] {
        let response = client
            .post(format!("{base}/v1/messages/count_tokens"))
            .bearer_auth("fixture-ui-test")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let result: Value = response.json().await.unwrap();
        assert_eq!(result["estimated"], true);
        counts.push(result["input_tokens"].as_u64().unwrap());
    }
    assert!(
        counts[1] > counts[0],
        "image disappeared from the estimated Chat prompt"
    );
    for source in [
        json!({"type":"base64","media_type":"image/png","data":"bad!"}),
        json!({"type":"url","url":"http://"}),
        json!({"type":"file","file_id":"foreign"}),
    ] {
        let mut body = with_image.clone();
        body["messages"][0]["content"][1]["source"] = source;
        let response = client
            .post(format!("{base}/v1/messages/count_tokens"))
            .bearer_auth("fixture-ui-test")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(!response
            .text()
            .await
            .unwrap()
            .contains("\"estimated\":true"));
    }
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
}

#[tokio::test]
async fn count_without_a_vision_candidate_estimates_the_complete_messages_representation() {
    let (client, base, replay, task) = setup("chat", Some(false)).await;
    let body = request("messages");
    let parsed: proto::protocol::MessagesRequest = serde_json::from_value(body.clone()).unwrap();
    let response = client
        .post(format!("{base}/v1/messages/count_tokens"))
        .bearer_auth("fixture-ui-test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["estimated"], true);
    assert_eq!(
        result["input_tokens"],
        translate::count::estimate_tokens(&parsed)
    );
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
}

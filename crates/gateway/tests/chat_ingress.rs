//! Replay at the Chat endpoint through the production candidate loop.

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::{json, Value};
use support::replay::{spawn, Step};

const CHAT_TOOL: &str = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");
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
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap();
    (client, base)
}

fn chat(client: &reqwest::Client, base: &str, request: Value) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("aubridge-opencode")
        .json(&request)
}

async fn last_request(client: &reqwest::Client, base: &str) -> Value {
    let result: Value = client
        .get(format!("{base}/admin/gateway"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    result["requests"][0].clone()
}

fn frames(sse: &str) -> Vec<Value> {
    proto::sse::parse(sse)
        .0
        .into_iter()
        .filter_map(|event| serde_json::from_str(&event.data).ok())
        .collect()
}

#[tokio::test]
async fn native_chat_keeps_options_and_only_sends_usage_on_request() {
    let (server, base, _) = spawn(vec![
        Step::Sse { body: CHAT_TOOL.into() },
        Step::Sse { body: CHAT_TOOL.into() },
        Step::Fail { status: 200, body: json!({
            "id":"chatcmpl-json", "object":"chat.completion", "model":"free-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"Done"},"finish_reason":"stop"}],
            "error": null,
            "usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7},
        }).to_string() },
    ]).await;
    let (client, gw) = gateway([source("free", &base, "free-model", "free_tier", "chat")]).await;
    for usage in [true, false] {
        let response = chat(
            &client,
            &gw,
            json!({
                "model":"free-model", "messages":[{"role":"user","content":"Check weather"}],
                "stream":true, "stream_options":{"include_usage":usage}, "seed":71,
            }),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.headers()["x-aubridge-provider"], "free");
        let raw = response.text().await.unwrap();
        assert!(raw.ends_with("data: [DONE]\n\n"));
        assert!(raw.contains("call_w_1"));
        assert_eq!(raw.contains("completion_tokens"), usage);
        assert_eq!(last_request(&client, &gw).await["output_tokens"], 30);
    }
    let response = chat(
        &client,
        &gw,
        json!({
            "model":"free-model", "messages":[{"role":"user","content":"Check weather"}],
        }),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let reply: Value = response.json().await.unwrap();
    assert_eq!(reply["choices"][0]["message"]["content"], "Done");
    let row = last_request(&client, &gw).await;
    assert!(row["duration_ms"].as_u64().unwrap() >= row["first_content_ms"].as_u64().unwrap());
    let received = server.seen_bodies.lock().await;
    let first: Value = serde_json::from_str(&received[0]).unwrap();
    let second: Value = serde_json::from_str(&received[1]).unwrap();
    let third: Value = serde_json::from_str(&received[2]).unwrap();
    assert_eq!(first["seed"], 71);
    assert_eq!(second["stream_options"]["include_usage"], true);
    assert_eq!(third["stream"], false);
}

#[tokio::test]
async fn embedded_chat_error_cools_only_the_throttled_model_before_output() {
    let (first, first_base, _) = spawn(vec![Step::SseWithHeaders {
        body: concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"error\":{\"code\":429,\"message\":\"private-vendor-text\",\"scope\":\"model\"}}\n\n",
        )
        .into(),
        headers: vec![("retry-after".into(), "30".into())],
    }])
    .await;
    let (second, second_base, _) = spawn(vec![
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (client, gw) = gateway([
        source("first", &first_base, "free-a", "free_tier", "chat"),
        source("second", &second_base, "free-b", "free_tier", "chat"),
    ])
    .await;
    let request = |model| {
        chat(
            &client,
            &gw,
            json!({"model":model,"stream":true,"messages":[{"role":"user","content":"Check weather"}]}),
        )
    };
    let group = "group/free-a+free-b";
    let response = request(group).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "second");
    assert!(!response
        .text()
        .await
        .unwrap()
        .contains("private-vendor-text"));
    let row = last_request(&client, &gw).await;
    assert_eq!(row["attempts"][0]["status"], 200);
    assert_eq!(row["attempts"][0]["reason"], "model-rate-limit");
    assert_eq!(row["attempts"][1]["chosen"], true);
    assert!(row["input_tokens"].is_number());
    assert!(!row.to_string().contains("private-vendor-text"));

    let response = request(group).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "second");
    response.text().await.unwrap();
    assert_eq!(
        last_request(&client, &gw).await["skipped"][0]["reason"],
        "model-rate-limit"
    );
    let response = request("free-a").send().await.unwrap();
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(first.seen_bodies.lock().await.len(), 1);
    assert_eq!(second.seen_bodies.lock().await.len(), 2);
}

#[tokio::test]
async fn embedded_chat_error_in_non_streaming_json_sets_the_account_cooldown() {
    let (upstream, base, _) = spawn(vec![Step::FailWithHeaders {
        status: 200,
        body: r#"{"error":{"code":"account_quota_exhausted"}}"#.into(),
        headers: vec![("retry-after".into(), "45".into())],
    }])
    .await;
    let (client, gw) = gateway([source("free", &base, "free-model", "free_tier", "chat")]).await;
    for _ in 0..2 {
        let response = chat(
            &client,
            &gw,
            json!({
                "model":"free-model","stream":false,"messages":[{"role":"user","content":"hello"}],
            }),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 429);
        let wait: u64 = response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((43..=46).contains(&wait));
    }
    assert_eq!(upstream.seen_bodies.lock().await.len(), 1);
    assert_eq!(
        last_request(&client, &gw).await["skipped"][0]["reason"],
        "account-quota-exhausted"
    );
}

#[tokio::test]
async fn embedded_chat_error_uses_structure_and_does_not_retry_after_commit() {
    let (first, first_base, _) = spawn(vec![
        Step::Sse {
            body: "data: {\"error\":{\"code\":\"unknown\",\"message\":\"quota exhausted\"}}\n\n".into(),
        },
        Step::Sse {
            body: concat!(
                "data: {\"id\":\"chatcmpl-a\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                "data: {\"error\":{\"code\":429,\"message\":\"private-vendor-text\"}}\n\n",
            )
            .into(),
        },
    ])
    .await;
    let (second, second_base, _) = spawn(vec![
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (client, gw) = gateway([
        source("first", &first_base, "free-a", "free_tier", "chat"),
        source("second", &second_base, "free-b", "free_tier", "chat"),
    ])
    .await;
    let request = || {
        chat(
            &client,
            &gw,
            json!({
                "model":"group/free-a+free-b","stream":true,"messages":[{"role":"user","content":"hello"}],
            }),
        )
    };
    let response = request().send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "second");
    response.text().await.unwrap();
    assert_eq!(
        last_request(&client, &gw).await["attempts"][0]["reason"],
        "stream-error"
    );
    let response = request().send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "first");
    let body = response.text().await.unwrap();
    assert!(
        body.contains("partial")
            && body.contains("upstream_stream_error")
            && body.contains("[DONE]")
    );
    assert!(!body.contains("private-vendor-text"));
    assert_eq!(last_request(&client, &gw).await["outcome"], "stream-error");
    assert_eq!(first.seen_bodies.lock().await.len(), 2);
    assert_eq!(second.seen_bodies.lock().await.len(), 1);

    let response = request().send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "second");
    response.text().await.unwrap();
    let row = last_request(&client, &gw).await;
    assert_eq!(row["skipped"][0]["id"], "first");
    assert_eq!(row["skipped"][0]["model"], "free-a");
    assert_eq!(row["skipped"][0]["reason"], "model-rate-limit");
    assert_eq!(first.seen_bodies.lock().await.len(), 2);
    assert_eq!(second.seen_bodies.lock().await.len(), 2);
}

#[tokio::test]
async fn explicit_chat_finish_error_uses_the_commit_boundary() {
    for (stream, committed) in [(true, false), (true, true), (false, false)] {
        let early = r#"data: {"choices":[{"index":0,"delta":{"content":"already sent"}}]}"#;
        let failed = json!({
            "choices":[{"index":0,
                "delta":{"content":"failed chunk"},
                "message":{"role":"assistant","content":"failed chunk"},
                "finish_reason":"error"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3},
        });
        let failure = if stream {
            let prefix = if committed {
                format!("{early}\n\n")
            } else {
                String::new()
            };
            Step::Sse {
                body: format!("{prefix}data: {failed}\n\ndata: [DONE]\n\n"),
            }
        } else {
            Step::Fail {
                status: 200,
                body: failed.to_string(),
            }
        };
        let (free, free_url, _) = spawn(vec![failure]).await;
        let fallback = if stream {
            Step::Sse {
                body: CHAT_TOOL.into(),
            }
        } else {
            Step::Fail {
                status: 200,
                body: r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"backup reply"},"finish_reason":"stop"}]}"#.into(),
            }
        };
        let (plan, plan_url, _) = spawn(vec![fallback]).await;
        let (client, gw) = gateway([
            source("free", &free_url, "free-model", "free_tier", "chat"),
            source("plan", &plan_url, "plan-model", "subscription", "chat"),
        ])
        .await;
        let response = chat(
            &client,
            &gw,
            json!({"model":"group/free-model+plan-model","stream":stream,
                "messages":[{"role":"user","content":"hello"}]}),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if committed { "free" } else { "plan" }
        );
        let body = response.text().await.unwrap();
        assert!(!body.contains("failed chunk"));
        let row = last_request(&client, &gw).await;
        assert_eq!(row["attempts"][0]["status"], 200);
        assert_eq!(row["attempts"][0]["reason"], "stream-error");
        if committed {
            assert!(body.contains("already sent") && body.contains("upstream_stream_error"));
            assert!(body.ends_with("data: [DONE]\n\n"));
            assert_eq!(row["outcome"], "stream-error");
            assert_eq!(row["output_tokens"], 3);
        } else {
            assert_eq!(row["outcome"], "ok");
            assert_eq!(row["provider"], "plan");
        }
        assert_eq!(free.seen_bodies.lock().await.len(), 1);
        assert_eq!(plan.seen_bodies.lock().await.len(), usize::from(!committed));
    }
}

#[tokio::test]
async fn openrouter_free_request_owns_the_model_and_price_ceiling() {
    let (upstream, url, _) = spawn(vec![
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (client, base) = gateway([source(
        "openrouter",
        &url,
        "vendor/actual:free",
        "free_tier",
        "chat",
    )])
    .await;
    let request = json!({
        "model":"vendor/actual:free", "messages":[{"role":"user","content":"Call weather"}],
        "stream":true, "seed":7,
        "tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object"}}}],
        "models":["vendor/paid"], "route":"fallback", "preset":"@preset/paid", "service_tier":"priority",
        "provider":{"max_price":{"prompt":9},"data_collection":"deny"},
        "unrecognized_extension":true,
    });
    let response = chat(&client, &base, request).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "openrouter");
    assert!(response.text().await.unwrap().contains("call_w_1"));
    let check: Value = client
        .post(format!("{base}/admin/providers/openrouter/check"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(check["status"], "ok");
    let sent = upstream.seen_bodies.lock().await;
    for body in sent.iter() {
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "vendor/actual:free");
        for field in [
            "models",
            "route",
            "preset",
            "service_tier",
            "unrecognized_extension",
        ] {
            assert!(
                body.get(field).is_none(),
                "unexpected outgoing field {field}"
            );
        }
        assert_eq!(
            body["provider"]["max_price"],
            json!({"prompt":0,"completion":0,"request":0,"image":0})
        );
        assert!(body["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == &json!({"id":"web","enabled":false})));
    }
    let first: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(first["provider"]["data_collection"], "deny");
    assert_eq!(first["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(first["seed"], 7);
}

#[tokio::test]
async fn openrouter_free_request_skips_hosted_tools_without_changing_other_upstreams() {
    let (free, free_url, _) = spawn(vec![]).await;
    let (other, other_url, _) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let (client, base) = gateway([
        source(
            "openrouter",
            &free_url,
            "vendor/actual:free",
            "free_tier",
            "chat",
        ),
        source("other", &other_url, "other-model", "subscription", "chat"),
    ])
    .await;
    let mut request = json!({
        "model":"vendor/actual:free", "messages":[{"role":"user","content":"Search"}],
        "stream":true, "tools":[{"type":"openrouter:web_search"}],
    });
    let refused = chat(&client, &base, request.clone()).send().await.unwrap();
    assert_eq!(refused.status(), 400);
    assert_eq!(
        refused.json::<Value>().await.unwrap()["error"]["code"],
        "free-route-restricted"
    );
    request["model"] = json!("group/other-model+vendor/actual:free");
    let response = chat(&client, &base, request).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "other");
    response.bytes().await.unwrap();
    assert!(free.seen_bodies.lock().await.is_empty());
    let other: Value = serde_json::from_str(&other.seen_bodies.lock().await[0]).unwrap();
    assert_eq!(other["tools"][0]["type"], "openrouter:web_search");
    assert!(other.get("provider").is_none());
    assert_eq!(
        last_request(&client, &base).await["skipped"][0]["reason"],
        "free-route-restricted"
    );
}

#[tokio::test]
async fn chat_tool_results_and_arguments_survive_switch_to_messages() {
    let (_, free, _) = spawn(vec![Step::Sse {
        body: String::new(),
    }])
    .await;
    let (plan, paid, _) = spawn(vec![
        Step::Sse {
            body: ANTHROPIC_TOOL.into(),
        },
        Step::Sse {
            body: ANTHROPIC_TOOL.into(),
        },
    ])
    .await;
    let (client, gw) = gateway([
        source("free", &free, "free-model", "free_tier", "chat"),
        source("plan", &paid, "plan-model", "subscription", "anthropic"),
    ])
    .await;
    let group = "group/plan-model+free-model";
    let response = chat(&client, &gw, json!({
        "model":group, "stream":true, "stream_options":{"include_usage":true},
        "messages":[{"role":"developer","content":"Use tools"},{"role":"user","content":"read the fixture"}],
        "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}],
    })).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    let raw = response.text().await.unwrap();
    assert!(raw.ends_with("data: [DONE]\n\n"));
    let events = frames(&raw);
    let deltas = events
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect::<String>();
    assert_eq!(deltas, r#"{"file":"fixture"}"#);
    assert!(events
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "tool_calls"));
    assert_eq!(events.last().unwrap()["usage"]["total_tokens"], 55);
    let row = last_request(&client, &gw).await;
    assert_eq!(row["attempts"][0]["id"], "free");
    assert_eq!(row["attempts"][1]["id"], "plan");
    assert_eq!(row["output_tokens"], 30);

    let reply: Value = chat(&client, &gw, json!({
        "model":"plan-model", "messages":[
            {"role":"user","content":"Read"},
            {"role":"assistant","content":"","tool_calls":[{"id":"tool_a","type":"function","function":{"name":"read","arguments":deltas}}]},
            {"role":"tool","tool_call_id":"tool_a","content":"fixture result"},
        ],
    })).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        reply["choices"][0]["message"]["tool_calls"][0]["id"],
        "tool_a"
    );
    assert_eq!(reply["choices"][0]["finish_reason"], "tool_calls");
    let requests = plan.seen_bodies.lock().await;
    let first: Value = serde_json::from_str(&requests[0]).unwrap();
    assert_eq!(first["system"], "Use tools");
    assert_eq!(first["tools"][0]["input_schema"]["type"], "object");
    let result: Value = serde_json::from_str(&requests[1]).unwrap();
    assert_eq!(
        result["messages"][1]["content"][0]["input"],
        json!({"file":"fixture"})
    );
    assert_eq!(result["messages"][2]["content"][0]["tool_use_id"], "tool_a");
    assert_eq!(
        result["messages"][1]["content"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        result["messages"][2]["content"][0]["content"],
        "fixture result"
    );
}

#[tokio::test]
async fn native_chat_clean_eof_after_complete_finish_preserves_late_usage() {
    let (upstream, base, _) = spawn(vec![Step::Sse {
        body: concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fixture answer\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":4}}\n\n",
        )
        .into(),
    }])
    .await;
    let (client, gw) = gateway([source("free", &base, "free-model", "free_tier", "chat")]).await;
    let response = chat(
        &client,
        &gw,
        json!({
            "model":"free-model","stream":true,"stream_options":{"include_usage":true},
            "messages":[{"role":"user","content":"hello"}],
        }),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let body = response.text().await.unwrap();
    assert!(!body.contains("upstream_stream_error"), "{body}");
    assert_eq!(body.matches("fixture answer").count(), 1);
    assert_eq!(body.matches("data: [DONE]\n\n").count(), 1);
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert!(frames(&body).iter().any(|frame| {
        frame["usage"]["prompt_tokens"] == 17 && frame["usage"]["completion_tokens"] == 4
    }));
    let row = last_request(&client, &gw).await;
    assert_eq!(row["outcome"], "ok");
    assert_eq!(row["input_tokens"], 17);
    assert_eq!(row["output_tokens"], 4);
    assert_eq!(upstream.seen_bodies.lock().await.len(), 1);
}

#[tokio::test]
async fn native_chat_clean_eof_requires_all_expected_choice_indices() {
    let first = json!({"index":0,"delta":{},"finish_reason":"stop"});
    let second = json!({"index":1,"delta":{},"finish_reason":"length"});
    let outside = json!({"index":2,"delta":{},"finish_reason":"stop"});
    let malformed = json!({"index":"0","delta":{},"finish_reason":"stop"});
    let omitted = json!({"delta":{},"finish_reason":"stop"});
    for (label, n, finishes, tail, ok) in [
        ("all", 2, vec![first.clone(), second.clone()], "", true),
        (
            "omitted-content-index",
            2,
            vec![first.clone(), second.clone()],
            "",
            true,
        ),
        ("missing", 2, vec![first.clone()], "", false),
        (
            "duplicate",
            2,
            vec![first.clone(), first.clone()],
            "",
            false,
        ),
        (
            "outside",
            2,
            vec![first.clone(), outside.clone()],
            "",
            false,
        ),
        ("malformed", 2, vec![malformed, second.clone()], "", false),
        ("omitted-index", 1, vec![omitted.clone()], "", false),
        (
            "explicit-done-omitted-index",
            1,
            vec![omitted],
            "data: [DONE]\n\n",
            true,
        ),
        (
            "explicit-done",
            2,
            vec![first.clone(), outside],
            "data: [DONE]\n\n",
            true,
        ),
        (
            "truncated-tail",
            2,
            vec![first.clone(), second.clone()],
            "data: {\"choices\":[]}",
            false,
        ),
        (
            "error-tail",
            2,
            vec![first, second],
            "data: {\"error\":{\"message\":\"fixture\"}}\n\n",
            false,
        ),
    ] {
        let prefix = if label == "omitted-content-index" {
            r#"data: {"choices":[{"delta":{"content":"fixture answer"}}]}"#
        } else {
            r#"data: {"choices":[{"index":0,"delta":{"content":"fixture answer"}}]}"#
        };
        let finish = json!({"choices":finishes});
        let (upstream, base, _) = spawn(vec![Step::Sse {
            body: format!("{prefix}\n\ndata: {finish}\n\n{tail}"),
        }])
        .await;
        let (backup, backup_base, _) = spawn(vec![]).await;
        let (client, gw) = gateway([
            source("free", &base, "free-model", "free_tier", "chat"),
            source("plan", &backup_base, "plan-model", "subscription", "chat"),
        ])
        .await;
        let response = chat(
            &client,
            &gw,
            json!({
                "model":"group/free-model+plan-model","stream":true,"n":n,
                "messages":[{"role":"user","content":"hello"}],
            }),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
        assert_eq!(response.headers()["x-aubridge-provider"], "free", "{label}");
        let body = response.text().await.unwrap();
        assert_eq!(body.matches("fixture answer").count(), 1, "{label}");
        assert_eq!(
            body.contains("upstream_stream_error"),
            !ok,
            "{label}: {body}"
        );
        assert_eq!(body.matches("data: [DONE]\n\n").count(), 1, "{label}");
        assert!(body.ends_with("data: [DONE]\n\n"), "{label}");
        let row = last_request(&client, &gw).await;
        assert_eq!(
            row["outcome"],
            if ok { "ok" } else { "stream-error" },
            "{label}"
        );
        assert!(
            row["input_tokens"].is_null() && row["output_tokens"].is_null(),
            "{label}"
        );
        assert_eq!(upstream.seen_bodies.lock().await.len(), 1, "{label}");
        assert!(backup.seen_bodies.lock().await.is_empty(), "{label}");
    }
}

#[tokio::test]
async fn committed_chat_stream_ends_without_trying_another_model() {
    let (_, free, _) = spawn(vec![Step::Sse {
        body: "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"started\"}}]}\n\n".into(),
    }])
    .await;
    let (plan, paid, _) = spawn(Vec::new()).await;
    let (client, gw) = gateway([
        source("free", &free, "free-model", "free_tier", "chat"),
        source("plan", &paid, "plan-model", "subscription", "anthropic"),
    ])
    .await;
    let response = chat(&client, &gw, json!({
        "model":"group/free-model+plan-model","stream":true,"messages":[{"role":"user","content":"hello"}],
    })).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let body = response.text().await.unwrap();
    assert!(body.contains("started"));
    assert!(body.contains("upstream_stream_error"));
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert!(plan.seen_bodies.lock().await.is_empty());
    assert_eq!(last_request(&client, &gw).await["outcome"], "stream-error");
}

#[tokio::test]
async fn chat_requires_an_inference_key_and_returns_openai_errors() {
    let (client, gw) = gateway([]).await;
    let response = client
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth("aubridge-admin")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "authentication_error"
    );
    let response = chat(&client, &gw, json!({"model":"x","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
}

#[tokio::test]
async fn terminal_request_errors_skip_other_providers() {
    let (free, first, _) = spawn(vec![
        Step::Fail {
            status: 400,
            body: r#"{"error":{"code":"context_length_exceeded","message":"test-sensitive text"}}"#
                .into(),
        },
        Step::Fail {
            status: 422,
            body: r#"{"error":{"message":"field required"}}"#.into(),
        },
    ])
    .await;
    let (paid, second, _) = spawn(Vec::new()).await;
    let (client, gw) = gateway([
        source("free", &first, "free-model", "free_tier", "chat"),
        source("plan", &second, "plan-model", "subscription", "anthropic"),
    ])
    .await;
    for (index, code) in ["context_length_exceeded", "invalid_request"]
        .into_iter()
        .enumerate()
    {
        let response = chat(&client, &gw, json!({
            "model":"group/free-model+plan-model","stream":true,"messages":[{"role":"user","content":"hello"}],
        })).send().await.unwrap();
        assert_eq!(response.status(), 400);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["code"], code);
        assert!(!error.to_string().contains("test-sensitive"));
        assert_eq!(last_request(&client, &gw).await["outcome"], code);
        assert_eq!(free.seen_bodies.lock().await.len(), index + 1);
        assert!(paid.seen_bodies.lock().await.is_empty());
    }
}

#[tokio::test]
async fn provider_specific_shape_refusal_can_still_switch() {
    let (_, first, _) = spawn(vec![Step::Fail {
        status: 400,
        body: r#"{"error":{"message":"Unknown parameter at this provider"}}"#.into(),
    }])
    .await;
    let (_, second, _) = spawn(vec![Step::Sse {
        body: ANTHROPIC_TOOL.into(),
    }])
    .await;
    let (client, gw) = gateway([
        source("free", &first, "free-model", "free_tier", "chat"),
        source("plan", &second, "plan-model", "subscription", "anthropic"),
    ])
    .await;
    let response = chat(&client, &gw, json!({
        "model":"group/free-model+plan-model","stream":true,"messages":[{"role":"user","content":"hello"}],
    })).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    assert!(response.text().await.unwrap().contains("tool_calls"));
    assert_eq!(last_request(&client, &gw).await["outcome"], "ok");
}

async fn seen(server: &support::replay::Replay, attempts: usize) {
    let start = std::time::Instant::now();
    while server.seen_bodies.lock().await.len() < attempts {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "upstream did not receive request"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn request_budget_reserves_last_fifteen_seconds_for_subscription() {
    // Advance the test clock explicitly after each real loopback send.
    let ticker = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let stall = Step::SseAfterDelay {
        body: CHAT_TOOL.into(),
        delay_ms: 65_000,
    };
    let (free, first, _) = spawn(vec![stall.clone(), stall.clone(), stall.clone()]).await;
    let (paid, second, _) = spawn(vec![stall]).await;
    let (id, mut free_manifest) = source("free", &first, "free-a", "free_tier", "chat");
    free_manifest
        .free_models
        .extend(["free-b".into(), "free-c".into()]);
    let (_, gw) = gateway([
        (id, free_manifest),
        source("plan", &second, "plan-model", "subscription", "anthropic"),
    ])
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = chat(
        &client,
        &gw,
        json!({
            "model":"group/plan-model+free-a+free-b+free-c","stream":true,"messages":[{"role":"user","content":"hello"}],
        }),
    );
    let running = tokio::spawn(async move { request.send().await.unwrap() });
    seen(&free, 1).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    seen(&free, 2).await;
    tokio::time::advance(Duration::from_secs(15)).await;
    seen(&paid, 1).await;
    assert_eq!(free.seen_bodies.lock().await.len(), 2);
    assert!(!running.is_finished());
    tokio::time::advance(Duration::from_secs(15)).await;
    let response = running.await.unwrap();
    assert_eq!(response.status(), 504);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "response_timeout");
    let row = last_request(&client, &gw).await;
    assert_eq!(row["skipped"][0]["model"], "free-c");
    assert_eq!(row["skipped"][0]["reason"], "response-budget");
    ticker.abort();
}

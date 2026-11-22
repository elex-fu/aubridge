//! The production HTTP ingress owns decoded size, encoding and stream leases.
use axum::{routing::post, Router};
use gateway::state::GatewayState;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn a_valid_long_request_larger_than_the_old_extractor_limit_reaches_the_provider() {
    let (upstream, server) = serve(
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: String| async move {
                    assert!(body.len() > 2 * 1024 * 1024);
                    (
                        [("content-type", "text/event-stream")],
                        include_str!("../../translate/tests/fixtures/chat_tool_call.sse"),
                    )
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
    )
    .await;
    let manifest = serde_json::from_value(json!({"id":"local","version":"1",
        "provider":{"base":format!("{upstream}/v1"),"auth":"bearer","protocol":"chat"},
        "free_models":["fixture-model"],"permissions":{"net":["127.0.0.1"]}}))
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let state = Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("local".into(), manifest)]),
            "fixture",
            BTreeMap::from([("local".into(), "synthetic-key".into())]),
        )
        .with_client(client.clone()),
    );
    let (base, gateway) = serve(gateway::app(state)).await;
    let response = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .json(
            &json!({"model":"fixture-model","stream":false,"max_tokens":16,
            "messages":[{"role":"user","content":"x".repeat(2*1024*1024+1024)}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "long valid body was rejected before its provider"
    );
    gateway.abort();
    server.abort();
}

fn manifest(base: &str, protocol: &str) -> manifest::schema::Manifest {
    serde_json::from_value(json!({"id":"local","version":"1",
        "provider":{"base":format!("{base}/v1"),"auth":"bearer","protocol":protocol},
        "free_models":["fixture-model"],"permissions":{"net":["127.0.0.1"]}}))
    .unwrap()
}

fn state(base: &str, protocol: &str) -> Arc<GatewayState> {
    Arc::new(
        GatewayState::with_keys(
            BTreeMap::from([("local".into(), manifest(base, protocol))]),
            "fixture",
            BTreeMap::from([("local".into(), "synthetic-key".into())]),
        )
        .with_client(reqwest::Client::builder().no_proxy().build().unwrap())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    )
}

fn compress(body: &[u8], encoding: &str) -> Vec<u8> {
    use std::io::Write;
    match encoding {
        "identity" => body.to_vec(),
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(body).unwrap();
            encoder.finish().unwrap()
        }
        "zstd" => zstd::stream::encode_all(body, 1).unwrap(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn compressed_messages_chat_and_responses_preserve_the_same_upstream_request() {
    use axum::http::HeaderMap;
    use std::sync::Mutex;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (path, protocol, body, fixture) in [
        (
            "/v1/messages",
            "chat",
            json!({"model":"fixture-model","stream":false,"max_tokens":16,
            "messages":[{"role":"user","content":"preserve this"}]}),
            include_str!("../../translate/tests/fixtures/chat_tool_call.sse"),
        ),
        (
            "/v1/chat/completions",
            "chat",
            json!({"model":"fixture-model","stream":true,"max_tokens":16,
            "messages":[{"role":"user","content":"preserve this"}]}),
            include_str!("../../translate/tests/fixtures/chat_tool_call.sse"),
        ),
        (
            "/v1/responses",
            "responses",
            json!({"model":"fixture-model","stream":true,"max_output_tokens":16,
            "input":"preserve this"}),
            include_str!("../../translate/tests/fixtures/codex_text.sse"),
        ),
    ] {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let upstream_path = if protocol == "responses" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let (upstream, server) = serve(Router::new().route(
            upstream_path,
            post({
                let seen = seen.clone();
                move |headers: HeaderMap, body: String| {
                    seen.lock().unwrap().push(body);
                    assert!(!headers.contains_key("content-encoding"));
                    async move { ([("content-type", "text/event-stream")], fixture) }
                }
            }),
        ))
        .await;
        let (base, gateway) = serve(gateway::app(state(&upstream, protocol))).await;
        for encoding in ["identity", "gzip", "zstd"] {
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth("fixture-ui-test")
                .header("content-type", "application/json")
                .header("content-encoding", encoding)
                .body(compress(body.to_string().as_bytes(), encoding))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let reply = response.text().await.unwrap();
            assert_eq!(status, 200, "{path} {encoding}: {reply}");
        }
        {
            let sent = seen.lock().unwrap();
            assert_eq!(sent.len(), 3);
            assert_eq!(sent[0], sent[1]);
            assert_eq!(sent[1], sent[2]);
        }
        for (encoding, expected, code) in [
            ("br", 415, "unsupported_content_encoding"),
            ("gzip", 400, "invalid_content_encoding"),
            ("zstd", 400, "invalid_content_encoding"),
        ] {
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth("fixture-ui-test")
                .header("content-encoding", encoding)
                .body("not compressed")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path} {encoding}");
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], code);
        }
        gateway.abort();
        server.abort();
    }
}

#[tokio::test]
async fn encoded_and_decoded_overflow_return_json_without_changing_provider_state() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = state("http://127.0.0.1:1", "chat");
    let before = state.snapshot();
    let (base, gateway) = serve(gateway::app(state.clone())).await;
    let address = base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer fixture-ui-test\r\nContent-Length: {}\r\n\r\n",32*1024*1024+1).as_bytes()).await.unwrap();
    let mut reply = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.read_to_string(&mut reply),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
    assert!(reply.contains("application/json"));
    assert!(reply.contains("request_too_large"));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let large = vec![0_u8; 32 * 1024 * 1024 + 1];
    for (encoding, path) in [("gzip", "/v1/messages"), ("zstd", "/v1/responses")] {
        let response = client
            .post(format!("{base}{path}"))
            .bearer_auth("fixture-ui-test")
            .header("content-encoding", encoding)
            .body(compress(&large, encoding))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "request_too_large");
    }
    assert_eq!(state.snapshot(), before);
    gateway.abort();
}

#[tokio::test]
async fn compressed_streams_hold_leases_until_disconnect_and_allow_immediate_reuse() {
    use axum::body::{Body, Bytes};
    use tokio::sync::Notify;
    let fixture = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");
    let split=fixture.find("data: {\"id\":\"chatcmpl-smoke-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\"").unwrap();
    let (prefix, suffix) = fixture.split_at(split);
    let release = Arc::new(Notify::new());
    let (upstream, server) = serve(Router::new().route(
        "/v1/chat/completions",
        post({
            let release = release.clone();
            move || {
                let release = release.clone();
                async move {
                    let source = futures_util::stream::unfold(
                        (0, release),
                        move |(phase, release)| async move {
                            match phase {
                                0 => Some((
                                    Ok::<_, std::io::Error>(Bytes::from_static(prefix.as_bytes())),
                                    (1, release),
                                )),
                                1 => {
                                    release.notified().await;
                                    Some((Ok(Bytes::from_static(suffix.as_bytes())), (2, release)))
                                }
                                _ => None,
                            }
                        },
                    );
                    (
                        [("content-type", "text/event-stream")],
                        Body::from_stream(source),
                    )
                }
            }
        }),
    ))
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (base, gateway) = serve(gateway::app(state(&upstream, "chat"))).await;
    let request = || {
        client
            .post(format!("{base}/v1/messages"))
            .bearer_auth("fixture-ui-test")
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(compress(
                json!({"model":"fixture-model","stream":true,"max_tokens":32,
            "messages":[{"role":"user","content":"OK"}]})
                .to_string()
                .as_bytes(),
                "gzip",
            ))
    };
    let mut first = request().send().await.unwrap();
    assert_eq!(first.status(), 200);
    first.chunk().await.unwrap().unwrap();
    let mut second = request().send().await.unwrap();
    assert_eq!(second.status(), 200);
    second.chunk().await.unwrap().unwrap();
    async fn page(client: &reqwest::Client, base: &str) -> serde_json::Value {
        client
            .get(format!("{base}/admin/gateway"))
            .bearer_auth("fixture-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    assert_eq!(
        page(&client, &base).await["request_limits"]["active_requests"],
        2
    );
    assert!(
        page(&client, &base).await["request_limits"]["reserved_bytes"]
            .as_u64()
            .unwrap()
            < 1024 * 1024,
        "small decoded bodies retained their maximum reservation"
    );
    drop(first);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while page(&client, &base).await["request_limits"]["active_requests"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut third = request().send().await.unwrap();
    assert_eq!(third.status(), 200);
    third.chunk().await.unwrap().unwrap();
    drop(second);
    drop(third);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while page(&client, &base).await["request_limits"]["active_requests"] != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let final_page = page(&client, &base).await;
    assert_eq!(final_page["request_limits"]["reserved_bytes"], 0);
    assert_eq!(
        final_page["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["outcome"] == "cancelled")
            .count(),
        3
    );
    gateway.abort();
    server.abort();
}

#[tokio::test]
async fn unary_compaction_uses_authenticated_compressed_ingress() {
    let client = reqwest::Client::new();
    let app = gateway::app(state("http://127.0.0.1:1", "responses"));
    let (base, task) = serve(app).await;
    let compressed = compress(br#"{"model":"fixture-model","input":"hello"}"#, "gzip");
    let result = client
        .post(format!("{base}/v1/responses/compact"))
        .header("content-encoding", "gzip")
        .body(compressed.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 401);
    let result = client
        .post(format!("{base}/v1/responses/compact"))
        .bearer_auth("fixture-codex")
        .header("content-encoding", "gzip")
        .header("content-type", "application/json")
        .body(compressed)
        .send()
        .await
        .unwrap();
    // Valid decoded JSON reached candidate dispatch instead of JSON extractor rejection.
    assert_eq!(result.status(), 502);
    task.abort();
}

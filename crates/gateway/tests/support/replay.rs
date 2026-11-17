//! 回放服务器(gateway/tests/support,M1 验收基础设施)。
//!
//! fixture 驱动的 fake Chat 上游:读语料回放 SSE;可注入失败序列
//! (429/5xx)与响应头,验证候选循环 failover 与状态转移。
//! 与 translate 回归集共用语料文件(单语料多消费者)。

use std::collections::VecDeque;

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::Router;
use tokio::sync::Mutex;

/// 一次回放步:失败(status + body)或成功(SSE 文本)。
#[derive(Clone)]
pub enum Step {
    Fail {
        status: u16,
        body: String,
    },
    Sse {
        body: String,
    },
    /// 带自定义响应头(被动采样验收:注入 x-codex-*/Retry-After 等)。
    FailWithHeaders {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
    },
    SseWithHeaders {
        body: String,
        headers: Vec<(String, String)>,
    },
    /// 延迟后回放 SSE(并发槽位验收:占住流)。
    SseAfterDelay {
        body: String,
        delay_ms: u64,
    },
}

/// 回放上游。多次 POST 依次消费 steps,并记录收到的请求头与请求体
/// 供断言。
#[derive(Default)]
pub struct Replay {
    pub steps: Mutex<VecDeque<Step>>,
    pub seen_headers: Mutex<Vec<HeaderMap>>,
    pub seen_bodies: Mutex<Vec<String>>,
}

/// 可选的 OAuth 刷新端点回放(`with_refresh`):POST /oauth/token
/// 恒返回给定 JSON(chatgpt 通道 401 → 刷新一次重试的验收侧)。
pub struct RefreshStub {
    pub response_json: String,
}

pub async fn spawn(
    steps: Vec<Step>,
) -> (std::sync::Arc<Replay>, String, tokio::task::JoinHandle<()>) {
    spawn_inner(steps, None).await
}

/// 回放 + OAuth 刷新端点(gateway chatgpt 通道测试)。
pub async fn spawn_with_refresh(
    steps: Vec<Step>,
    refresh_json: String,
) -> (std::sync::Arc<Replay>, String, tokio::task::JoinHandle<()>) {
    spawn_inner(
        steps,
        Some(RefreshStub {
            response_json: refresh_json,
        }),
    )
    .await
}

async fn spawn_inner(
    steps: Vec<Step>,
    refresh: Option<RefreshStub>,
) -> (std::sync::Arc<Replay>, String, tokio::task::JoinHandle<()>) {
    let replay = std::sync::Arc::new(Replay {
        steps: Mutex::new(steps.into()),
        seen_headers: Mutex::new(Vec::new()),
        seen_bodies: Mutex::new(Vec::new()),
    });
    let handler = {
        let replay = replay.clone();
        move |headers: HeaderMap, body: String| {
            let replay = replay.clone();
            async move {
                replay.seen_headers.lock().await.push(headers);
                replay.seen_bodies.lock().await.push(body);
                let step = replay.steps.lock().await.pop_front();
                let mut extra: Vec<(String, String)> = Vec::new();
                let mut delay_ms: Option<u64> = None;
                // 归一化成无头基型(WithHeaders 拆出额外头,AfterDelay
                // 拆出延迟),再统一建响应。
                enum Out {
                    Sse(String),
                    Fail(u16, String),
                    Gone,
                }
                let out = match step {
                    Some(Step::FailWithHeaders {
                        status,
                        body,
                        headers,
                    }) => {
                        extra = headers;
                        Out::Fail(status, body)
                    }
                    Some(Step::SseWithHeaders { body, headers }) => {
                        extra = headers;
                        Out::Sse(body)
                    }
                    Some(Step::SseAfterDelay { body, delay_ms: d }) => {
                        delay_ms = Some(d);
                        Out::Sse(body)
                    }
                    Some(Step::Fail { status, body }) => Out::Fail(status, body),
                    Some(Step::Sse { body }) => Out::Sse(body),
                    None => Out::Gone,
                };
                if let Some(ms) = delay_ms {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                let mut builder = match out {
                    Out::Sse(body) => Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(body))
                        .unwrap(),
                    Out::Fail(status, body) => Response::builder()
                        .status(StatusCode::from_u16(status).unwrap())
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                    Out::Gone => Response::builder()
                        .status(StatusCode::GONE)
                        .body(Body::from(r#"{"error":"replay exhausted"}"#))
                        .unwrap(),
                };
                for (k, v) in extra {
                    builder.headers_mut().insert(
                        axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        axum::http::HeaderValue::from_str(&v).unwrap(),
                    );
                }
                builder
            }
        }
    };
    // 三种上游路径都注册:Chat(/chat/completions)、Anthropic 透传
    // (/messages)、Responses(/responses);OAuth 刷新端点按需注册。
    let mut app = Router::new()
        .route("/chat/completions", post(handler.clone()))
        .route("/messages", post(handler.clone()))
        .route("/responses", post(handler));
    if let Some(stub) = refresh {
        let json = stub.response_json;
        app = app.route(
            "/oauth/token",
            post(move || {
                let json = json.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Body::from(json))
                        .unwrap()
                }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (replay, format!("http://{addr}"), task)
}

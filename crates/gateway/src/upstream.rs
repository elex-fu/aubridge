//! Attempts stay private until usable content is ready. Settlement follows
//! the end of the message, regardless of how many sources were tried.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use manifest::schema::{AuthKind, ProviderKind, WireProtocol};
use manifest::state::{ModelCooldown, ModelCooldownReason};
use proto::protocol::{
    AnthropicDelta, AnthropicEvent, ContentBlockStart, MessageDeltaBody, MessageStartBody,
    MessagesRequest, SystemContent, Usage,
};
use router::{Bucket, FailureDecision, UpstreamSignal};
use serde_json::Value;
use tokio::time::Instant;
use translate::chat_input::ChatInput;
use translate::{error_terminator, messages_request_to_chat, messages_request_to_responses};

use crate::chat;
use crate::openrouter;
use crate::route_plan::{self, Candidate, Credential};
use crate::state::{ConnectionVersion, GatewayState};
use crate::stream::{self, Frame, Reply, ReportedUsage, StartFailure};

const MAX_ATTEMPTS: usize = 4;
const FIRST_CONTENT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_BUDGET: Duration = Duration::from_secs(60);
const SUBSCRIPTION_RESERVE: Duration = Duration::from_secs(15);
const PROVIDER_BUSY: &str = "provider-busy";
const COUNT_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const SSE_KEEPALIVE: Duration = Duration::from_secs(5);
const UPSTREAM_ERROR_LIMIT: usize = 64 * 1024;
const KEY_RETRY_SECS: u64 = 60;

#[derive(Clone, Copy)]
enum Input<'a> {
    Messages(&'a str),
    Chat(&'a ChatInput),
    Responses(&'a translate::responses_input::ResponsesInput),
}

impl Input<'_> {
    fn allows(self, protocol: WireProtocol) -> bool {
        match (self, protocol) {
            (Self::Chat(input), WireProtocol::Anthropic) => !input.native_only,
            (Self::Chat(input), WireProtocol::Responses) => input.supports_responses(),
            (Self::Responses(input), protocol) => {
                protocol == WireProtocol::Responses || input.compatible
            }
            _ => true,
        }
    }
}

pub async fn forward(
    state: Arc<GatewayState>,
    req: &MessagesRequest,
    client_headers: &HeaderMap,
    body: &str,
) -> Response<Body> {
    forward_input(state, req, client_headers, Input::Messages(body)).await
}

pub(crate) async fn forward_chat(
    state: Arc<GatewayState>,
    input: &ChatInput,
    client_headers: &HeaderMap,
) -> Response<Body> {
    forward_input(state, &input.request, client_headers, Input::Chat(input)).await
}

pub(crate) async fn forward_responses(
    state: Arc<GatewayState>,
    input: &translate::responses_input::ResponsesInput,
    client_headers: &HeaderMap,
) -> Response<Body> {
    forward_input(
        state,
        &input.request,
        client_headers,
        Input::Responses(input),
    )
    .await
}

async fn forward_input(
    state: Arc<GatewayState>,
    req: &MessagesRequest,
    client_headers: &HeaderMap,
    input: Input<'_>,
) -> Response<Body> {
    let now = unix_now();
    let started = Instant::now();
    let response_deadline = started + RESPONSE_BUDGET;
    let streaming = req.stream != Some(false);
    let mut log = RequestLog::new(state.clone(), req, client_headers, now, started);
    if router::guard::tool_loop_detected(req) {
        log.finish("guard");
        return loop_guard_answer(&req.model, streaming, input);
    }

    let needs_tools = matches!(input, Input::Responses(responses) if responses.has_tools);
    let hosted_request =
        matches!(input, Input::Chat(chat) if !openrouter::supports(chat.original()));
    let mut plan = route_plan::plan(
        &state,
        req,
        client_headers,
        now,
        true,
        needs_tools,
        |manifest, model| {
            if !input.allows(manifest.provider.protocol) {
                return Some(route_plan::Block::new("native-protocol-required", None));
            }
            (openrouter::applies(manifest) && (hosted_request || !crate::catalog::free_id(model)))
                .then(|| route_plan::Block::new(openrouter::RESTRICTED, None))
        },
    );
    if !plan.recognized_model {
        let error = ClientError::ModelNotFound;
        log.finish(error.code());
        return error.response(input);
    }
    let rejected_by_free_policy = plan.ordered.is_empty() && plan.has_block(openrouter::RESTRICTED);
    let deferred_context = plan.prefer_context(req);
    log.skipped = std::mem::take(&mut plan.skipped);
    let mut attempts = 0;
    let mut immediate = plan.ordered.iter();
    let mut deferred = VecDeque::new();
    while let Some((candidate, revisit)) = immediate
        .next()
        .map(|candidate| (candidate, false))
        .or_else(|| deferred.pop_front().map(|candidate| (candidate, true)))
    {
        if revisit {
            log.skipped
                .retain(|entry| !entry.matches(candidate, PROVIDER_BUSY));
        }
        if deferred_context {
            log.skipped.retain(|entry| !entry.context_for(candidate));
        }
        let id = &candidate.manifest.id;
        let now = unix_now();
        if let Some(block) = state.route_block(id, &candidate.model, now) {
            // A previous attempt may have exhausted this account for every model.
            log.skip(candidate, block);
            continue;
        }
        if let Some(block) = log.context_block(candidate) {
            log.skip(candidate, block);
            continue;
        }
        let reserve_subscription = plan.free_first
            && candidate.manifest.kind == ProviderKind::FreeTier
            && immediate
                .as_slice()
                .iter()
                .chain(deferred.iter().copied())
                .any(|fallback| {
                    fallback.manifest.kind == ProviderKind::Subscription
                        && log.context_block(fallback).is_none()
                        && state
                            .route_block(&fallback.manifest.id, &fallback.model, now)
                            .is_none()
                });
        if attempts == MAX_ATTEMPTS || (reserve_subscription && attempts == MAX_ATTEMPTS - 1) {
            log.skip(candidate, route_plan::Block::new("retry-budget", None));
            continue;
        }
        let Some((deadline, cutoff)) =
            response_window(Instant::now(), response_deadline, reserve_subscription)
        else {
            log.skip(candidate, route_plan::Block::new("response-budget", None));
            continue;
        };
        let permit = if let Some(slot) = state.subscription_slot(id, candidate.manifest.kind) {
            let permit = if revisit {
                log.skip(candidate, route_plan::Block::new(PROVIDER_BUSY, None));
                let permit = tokio::time::timeout_at(deadline, slot.acquire_owned())
                    .await
                    .ok()
                    .and_then(Result::ok);
                log.skipped
                    .retain(|entry| !entry.matches(candidate, PROVIDER_BUSY));
                permit
            } else {
                match slot.try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(error) => {
                        if error == tokio::sync::TryAcquireError::NoPermits {
                            deferred.push_back(candidate);
                        }
                        log.skip(candidate, route_plan::Block::new(PROVIDER_BUSY, None));
                        continue;
                    }
                }
            };
            if let Some(block) = state
                .route_block(id, &candidate.model, unix_now())
                .or_else(|| log.context_block(candidate))
            {
                log.skip(candidate, block);
                continue;
            }
            match permit {
                Some(permit) => Some(permit),
                None => {
                    log.skip(candidate, route_plan::Block::new(PROVIDER_BUSY, None));
                    continue;
                }
            }
        } else {
            None
        };
        if Instant::now() >= deadline {
            log.skip(
                candidate,
                route_plan::Block::new("first-content-timeout", None),
            );
            continue;
        }
        let response = tokio::time::timeout_at(
            deadline,
            send(
                &state,
                candidate,
                req,
                client_headers,
                input,
                |connection| {
                    if Instant::now() >= deadline {
                        return Err("first-content-timeout");
                    }
                    log.start(candidate, connection);
                    Ok(())
                },
            ),
        )
        .await
        .unwrap_or(Err("first-content-timeout"));
        attempts += usize::from(log.active.is_some());
        let response = match response {
            Ok(sent) => sent,
            Err(reason) if log.active.is_none() => {
                log.skip(candidate, route_plan::Block::new(reason, None));
                continue;
            }
            Err(reason) => {
                log.note_breaker_failure();
                log.end_attempt(reason, false);
                continue;
            }
        };
        let status = response.status();
        log.active_mut().status = status.as_u16();
        if status != StatusCode::OK {
            let headers = response.headers().clone();
            let body = tokio::time::timeout_at(
                deadline,
                stream::read_limited(response, UPSTREAM_ERROR_LIMIT),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            if let Some(error) = log.reject(
                candidate,
                req,
                status.as_u16(),
                &headers,
                FailureBody::Http(&String::from_utf8_lossy(&body)),
            ) {
                return error.response(input);
            }
            continue;
        }
        log.active_mut().headers = Some(response.headers().clone());
        let protocol = candidate.manifest.provider.protocol;
        let chat_native = match input {
            Input::Chat(chat) if protocol == WireProtocol::Chat => Some(chat),
            _ => None,
        };
        if !streaming && chat_native.is_some() {
            let result = tokio::time::timeout_at(
                deadline,
                stream::native_chat_json(response, &mut log.active_mut().usage),
            )
            .await
            .unwrap_or(Err("first-content-timeout".into()));
            match result {
                Ok(body) => {
                    log.content_ready();
                    log.finish("ok");
                    return bytes_response(body, id);
                }
                Err(failure) => {
                    if let Some(error) = log.reject_before_content(candidate, req, failure) {
                        return error.response(input);
                    }
                    continue;
                }
            }
        }
        let mut reply = Reply::new(response, protocol, &candidate.model, permit);
        if let Some(chat) = chat_native {
            reply = reply.native_chat(chat.include_usage, chat.choices());
        }
        if matches!(input, Input::Responses(_)) {
            reply = reply.for_responses();
        }
        let held = match reply
            .hold_first(&mut log.active_mut().usage, deadline)
            .await
        {
            Ok(held) => held,
            Err(failure) => {
                if let Some(error) = log.reject_before_content(candidate, req, failure) {
                    return error.response(input);
                }
                continue;
            }
        };
        if streaming {
            let encoder = Projection::for_input(input, &candidate.model, now, protocol);
            log.content_ready();
            return streaming_response(id, held, reply, log, encoder);
        }
        let result = tokio::time::timeout_at(
            cutoff,
            stream::whole_message(held, &mut reply, &mut log.active_mut().usage),
        )
        .await
        .unwrap_or(Err("response-timeout"));
        match result {
            Ok(message) => {
                let message = match input {
                    Input::Chat(_) => chat::completion(
                        &message,
                        &log.active_mut().usage,
                        protocol,
                        &candidate.model,
                        now,
                    ),
                    Input::Messages(_) | Input::Responses(_) => message,
                };
                log.content_ready();
                log.finish("ok");
                return json_response(message, id);
            }
            Err(reason) => {
                log.note_breaker_failure();
                log.end_attempt(reason, false);
            }
        }
    }

    if log.context_failure.is_some() {
        let error = ClientError::ContextLength;
        log.finish(error.code());
        return error.response(input);
    }
    if Instant::now() >= response_deadline {
        log.finish("response-timeout");
        return ClientError::Timeout.response(input);
    }
    if let Some(wait) = plan.retry_after(&state, unix_now()) {
        log.finish("rate-limited");
        return ClientError::RateLimit(wait).response(input);
    }
    if rejected_by_free_policy {
        log.finish(openrouter::RESTRICTED);
        return ClientError::FreeRouteRestricted.response(input);
    }
    log.finish("no-candidate");
    if matches!(input, Input::Chat(_) | Input::Responses(_)) {
        return chat::error_response(
            StatusCode::BAD_GATEWAY,
            "No compatible upstream succeeded",
            "server_error",
            Some("no_available_provider"),
        );
    }
    if streaming {
        anthropic_error("no candidate provider succeeded")
    } else {
        (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({
                "type": "error",
                "error": {"type":"api_error", "message":"no candidate provider succeeded"},
            })),
        )
            .into_response()
    }
}

fn response_window(
    now: Instant,
    end: Instant,
    reserve_subscription: bool,
) -> Option<(Instant, Instant)> {
    let cutoff = if reserve_subscription {
        end - SUBSCRIPTION_RESERVE
    } else {
        end
    };
    (now < cutoff).then(|| ((now + FIRST_CONTENT_TIMEOUT).min(cutoff), cutoff))
}

pub(crate) async fn count_tokens(
    state: &GatewayState,
    req: &MessagesRequest,
    headers: &HeaderMap,
    body: &str,
) -> Response<Body> {
    let plan = route_plan::plan(state, req, headers, unix_now(), false, false, |_, _| None);
    let candidate = plan.ordered.first();
    if let Some(candidate) =
        candidate.filter(|c| c.manifest.provider.protocol == WireProtocol::Anthropic)
    {
        if let Some(count) = native_count(state, candidate, headers, body).await {
            return axum::Json(count).into_response();
        }
    }
    let mut input = Cow::Borrowed(req);
    if let Some(candidate) = candidate {
        if candidate.model != req.model {
            input.to_mut().model.clone_from(&candidate.model);
        }
        if candidate.manifest.provider.protocol != WireProtocol::Anthropic {
            strip_messages_attribution(&mut input);
        }
    }
    let estimated = match candidate.map(|c| c.manifest.provider.protocol) {
        Some(WireProtocol::Chat) => {
            translate::count::estimate_serialized(&messages_request_to_chat(&input))
        }
        Some(WireProtocol::Responses) => {
            translate::count::estimate_serialized(&messages_request_to_responses(&input))
        }
        _ => translate::count::estimate_tokens(&input),
    };
    axum::Json(serde_json::json!({"input_tokens": estimated, "estimated": true})).into_response()
}

#[derive(serde::Serialize)]
pub(crate) struct ConnectionCheck {
    status: &'static str,
    model: Option<String>,
    upstream_status: Option<u16>,
}

pub(crate) async fn check_provider(
    state: &GatewayState,
    id: &str,
    requested: Option<&str>,
) -> Option<ConnectionCheck> {
    let manifest = state.manifest(id)?;
    let model = requested
        .map(str::to_owned)
        .or_else(|| manifest.free_models.first().cloned());
    let mut result = ConnectionCheck {
        status: "model-unavailable",
        model: model.clone(),
        upstream_status: None,
    };
    let Some(model) = model.filter(|model| manifest.free_models.contains(model)) else {
        return Some(result);
    };
    let _permit = if let Some(slot) = state.subscription_slot(id, manifest.kind) {
        let Ok(permit) = slot.try_acquire_owned() else {
            result.status = "provider-busy";
            return Some(result);
        };
        Some(permit)
    } else {
        None
    };
    let candidate = Candidate {
        codex: state.is_chatgpt(&manifest),
        manifest,
        model: model.clone(),
    };
    let request = MessagesRequest {
        model,
        messages: vec![proto::protocol::InboundMessage {
            role: "user".into(),
            content: proto::protocol::InboundContent::Text("Reply with OK.".into()),
        }],
        system: None,
        max_tokens: 64,
        tools: None,
        tool_choice: None,
        stream: Some(true),
        metadata: None,
        thinking: None,
        output_config: None,
    };
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let body = serde_json::to_string(&request).ok()?;
    let response = tokio::time::timeout_at(
        deadline,
        send(
            state,
            &candidate,
            &request,
            &HeaderMap::new(),
            Input::Messages(&body),
            |_| Ok(()),
        ),
    )
    .await
    .unwrap_or(Err("first-content-timeout"));
    result.status = match response {
        Err(reason) => reason,
        Ok(response) if response.status() != StatusCode::OK => {
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            result.upstream_status = Some(status);
            let body = tokio::time::timeout_at(
                deadline,
                stream::read_limited(response, UPSTREAM_ERROR_LIMIT),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            classify_http_failure(
                FailureSource::from(&candidate),
                status,
                &headers,
                FailureBody::Http(&String::from_utf8_lossy(&body)),
                unix_now(),
            )
            .reason
        }
        Ok(response) => {
            result.upstream_status = Some(200);
            let headers = response.headers().clone();
            let mut reply = Reply::new(
                response,
                candidate.manifest.provider.protocol,
                &candidate.model,
                None,
            );
            let mut usage = ReportedUsage::default();
            match reply.hold_first(&mut usage, deadline).await {
                Ok(_) => tokio::time::timeout_at(deadline, async {
                    while reply.next(&mut usage).await.is_some() {}
                    if reply.failed {
                        "stream-error"
                    } else {
                        "ok"
                    }
                })
                .await
                .unwrap_or("response-timeout"),
                Err(StartFailure::Stream(reason)) => reason,
                Err(StartFailure::EmbeddedHttp { status, body }) => {
                    classify_http_failure(
                        FailureSource::from(&candidate),
                        status,
                        &headers,
                        FailureBody::Embedded(&body),
                        unix_now(),
                    )
                    .reason
                }
            }
        }
    };
    Some(result)
}

async fn native_count(
    state: &GatewayState,
    candidate: &Candidate,
    headers: &HeaderMap,
    body: &str,
) -> Option<Value> {
    let mut request: Value = serde_json::from_str(body).ok()?;
    request["model"] = Value::String(candidate.model.clone());
    let request = request.as_object_mut()?;
    request.remove("stream");
    request.remove("max_tokens");
    let id = &candidate.manifest.id;
    let _permit = match state.subscription_slot(id, candidate.manifest.kind) {
        Some(slot) => Some(slot.try_acquire_owned().ok()?),
        None => None,
    };
    if state
        .route_block(id, &candidate.model, unix_now())
        .is_some()
    {
        return None;
    }
    let (credential, _) = state.dispatch_credential(&candidate.manifest).ok()?;
    let response = build_post(
        state,
        candidate,
        &credential,
        headers,
        "/messages/count_tokens",
    )
    .timeout(COUNT_TIMEOUT)
    .json(request)
    .send()
    .await
    .ok()?
    .error_for_status()
    .ok()?;
    let count: Value = response.json().await.ok()?;
    count["input_tokens"].as_u64().map(|_| count)
}

async fn send(
    state: &GatewayState,
    candidate: &Candidate,
    req: &MessagesRequest,
    headers: &HeaderMap,
    input: Input<'_>,
    on_dispatch: impl FnOnce(DispatchedConnection) -> Result<(), &'static str>,
) -> Result<reqwest::Response, &'static str> {
    let mut request = if req.model == candidate.model {
        Cow::Borrowed(req)
    } else {
        let mut request = req.clone();
        request.model.clone_from(&candidate.model);
        Cow::Owned(request)
    };
    if matches!(input, Input::Messages(_))
        && candidate.manifest.provider.protocol != WireProtocol::Anthropic
    {
        strip_messages_attribution(&mut request);
    }
    let (path, body) = match (candidate.manifest.provider.protocol, input) {
        (WireProtocol::Chat, Input::Chat(chat)) => (
            "/chat/completions",
            serde_json::to_string(&chat.for_chat(&candidate.model)),
        ),
        (WireProtocol::Anthropic, Input::Chat(chat)) => (
            "/messages",
            chat.for_anthropic(&request)
                .and_then(|body| serde_json::to_string(&body)),
        ),
        (WireProtocol::Chat, Input::Responses(responses)) => {
            let body =
                serde_json::to_value(messages_request_to_chat(&request)).and_then(|mut body| {
                    if responses.parallel_tools_disabled() && request.tools.is_some() {
                        body["parallel_tool_calls"] = Value::Bool(false);
                    }
                    serde_json::to_string(&body)
                });
            ("/chat/completions", body)
        }
        (WireProtocol::Anthropic, Input::Responses(responses)) => {
            let body = serde_json::to_value(request.as_ref()).and_then(|mut body| {
                if request.max_tokens == 0 {
                    body["max_tokens"] = serde_json::json!(4096);
                }
                if responses.parallel_tools_disabled()
                    && request.tools.is_some()
                    && !matches!(request.tool_choice, Some(proto::protocol::ToolChoice::None))
                {
                    if body["tool_choice"].is_null() {
                        body["tool_choice"] = serde_json::json!({"type":"auto"});
                    }
                    body["tool_choice"]["disable_parallel_tool_use"] = Value::Bool(true);
                }
                serde_json::to_string(&body)
            });
            ("/messages", body)
        }
        (WireProtocol::Anthropic, Input::Messages(original))
            if request.stream == Some(true) && req.model == candidate.model =>
        {
            ("/messages", Ok(original.to_string()))
        }
        (WireProtocol::Anthropic, Input::Messages(original)) => {
            let mut raw: Value = serde_json::from_str(original).map_err(|_| "encode-error")?;
            raw["stream"] = Value::Bool(true);
            raw["model"] = Value::String(candidate.model.clone());
            ("/messages", serde_json::to_string(&raw))
        }
        (WireProtocol::Chat, Input::Messages(_)) => (
            "/chat/completions",
            serde_json::to_string(&messages_request_to_chat(&request)),
        ),
        (WireProtocol::Responses, Input::Responses(responses)) => (
            "/responses",
            serde_json::to_string(&responses.for_native(&candidate.model)),
        ),
        (WireProtocol::Responses, _) => (
            "/responses",
            serde_json::to_string(&messages_request_to_responses(&request)),
        ),
    };
    let body = body.map_err(|_| "encode-error")?;
    let body = if openrouter::applies(&candidate.manifest) {
        openrouter::protect(&body, &candidate.model)?
    } else {
        body
    };
    let (credential, version) = state.dispatch_credential(&candidate.manifest)?;
    let chatgpt = if matches!(credential, Credential::ChatGpt) {
        Some(state.chatgpt_snapshot().await.ok_or("missing-credential")?)
    } else {
        None
    };
    on_dispatch(DispatchedConnection {
        version,
        uses_key: matches!(credential, Credential::Key(_)),
    })?;
    if let Some(tokens) = chatgpt {
        send_chatgpt(
            state,
            &format!("{}{path}", candidate.manifest.provider.base),
            body,
            tokens,
        )
        .await
    } else {
        build_post(state, candidate, &credential, headers, path)
            .body(body)
            .send()
            .await
            .map_err(|_| "network-error")
    }
}

// Keep Anthropic's per-request attribution out of the translated prefix.
fn strip_messages_attribution(request: &mut Cow<'_, MessagesRequest>) {
    let Some(system) = &request.system else {
        return;
    };
    let cleaned = match system {
        SystemContent::Text(text) => attribution_remainder(text).map(str::to_owned),
        SystemContent::Blocks(blocks) => {
            let mut changed = false;
            let mut kept = Vec::with_capacity(blocks.len());
            for (index, block) in blocks.iter().enumerate() {
                let tail = if block.kind == "text" {
                    attribution_remainder(&block.text)
                } else {
                    None
                };
                if let Some(tail) = tail.filter(|tail| index == 0 || tail.is_empty()) {
                    changed = true;
                    if !tail.is_empty() {
                        kept.push(tail);
                    }
                } else {
                    kept.push(block.text.as_str());
                }
            }
            changed.then(|| kept.join("\n"))
        }
    };
    if let Some(text) = cleaned {
        request.to_mut().system = (!text.is_empty()).then_some(SystemContent::Text(text));
    }
}

fn attribution_remainder(text: &str) -> Option<&str> {
    let (line, tail) = text.split_once('\n').unwrap_or((text, ""));
    let fields = line
        .strip_prefix("x-anthropic-billing-header:")?
        .trim()
        .strip_suffix(';')?;
    let mut has_version = false;
    for field in fields.split(';') {
        let (key, value) = field.trim().split_once('=')?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return None;
        }
        has_version |= key == "cc_version" && !value.is_empty();
    }
    has_version.then_some(tail)
}

fn build_post(
    state: &GatewayState,
    candidate: &Candidate,
    credential: &Credential,
    headers: &HeaderMap,
    path: &str,
) -> reqwest::RequestBuilder {
    let base = candidate.manifest.provider.base.trim_end_matches('/');
    let mut request = state
        .http()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json");
    if let Credential::Key(key) = credential {
        request = match candidate.manifest.provider.auth {
            AuthKind::Bearer => request.bearer_auth(key),
            AuthKind::ApiKey => request.header("x-api-key", key),
        };
    }
    if candidate.manifest.provider.protocol == WireProtocol::Anthropic {
        let version = headers
            .get("anthropic-version")
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("2023-06-01"));
        request = request.header("anthropic-version", version);
        if let Some(beta) = headers.get("anthropic-beta") {
            request = request.header("anthropic-beta", beta.clone());
        }
    }
    request
}

async fn send_chatgpt(
    state: &GatewayState,
    url: &str,
    body: String,
    tokens: crate::chatgpt::LoginSnapshot,
) -> Result<reqwest::Response, &'static str> {
    let response = crate::chatgpt::responses_builder(state.http(), url, &tokens)
        .body(body.clone())
        .send()
        .await
        .map_err(|_| "network-error")?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok(response);
    }
    let Some(fresh) = state.chatgpt_refresh_snapshot(tokens).await else {
        return Ok(response);
    };
    crate::chatgpt::responses_builder(state.http(), url, &fresh)
        .body(body)
        .send()
        .await
        .map_err(|_| "network-error")
}

enum Projection {
    Chat(chat::Encoder),
    Responses(crate::responses::Encoder),
}

impl Projection {
    fn for_input(input: Input<'_>, model: &str, now: u64, from: WireProtocol) -> Option<Self> {
        match input {
            Input::Chat(chat) if from != WireProtocol::Chat => Some(Self::Chat(
                chat::Encoder::new(model, now, from, chat.include_usage),
            )),
            Input::Responses(responses) if from != WireProtocol::Responses => {
                Some(Self::Responses(crate::responses::Encoder::new(
                    model,
                    now,
                    from,
                    responses.tool_names.clone(),
                )))
            }
            _ => None,
        }
    }

    fn encode(&mut self, event: &Value, usage: &ReportedUsage) -> Option<Bytes> {
        match self {
            Self::Chat(encoder) => encoder.encode(event, usage),
            Self::Responses(encoder) => encoder.encode(event, usage),
        }
    }
}

fn streaming_response(
    provider: &str,
    held: VecDeque<Frame>,
    reply: Reply,
    log: RequestLog,
    encoder: Option<Projection>,
) -> Response<Body> {
    let stream = futures_util::stream::unfold(
        (held, reply, log, encoder),
        |(mut held, mut reply, mut log, mut encoder)| async move {
            let idle_deadline = Instant::now() + SSE_KEEPALIVE;
            loop {
                let frame = match held.pop_front() {
                    Some(frame) => frame,
                    None if reply.ended => return None,
                    None => match tokio::time::timeout_at(
                        idle_deadline,
                        reply.next(&mut log.active_mut().usage),
                    )
                    .await
                    {
                        Ok(frame) => frame?,
                        Err(_) => {
                            let comment = Bytes::from(proto::sse::frame_keepalive());
                            return Some((Ok(comment), (held, reply, log, encoder)));
                        }
                    },
                };
                let bytes = match encoder.as_mut() {
                    Some(encoder) => encoder.encode(&frame.event, &log.active_mut().usage),
                    None => Some(frame.bytes),
                };
                if matches!(&encoder, Some(Projection::Responses(encoder)) if encoder.failed) {
                    reply.ended = true;
                    reply.failed = true;
                    held.clear();
                }
                if reply.ended {
                    let incomplete = frame.event["type"] == "response.incomplete"
                        || matches!(&encoder, Some(Projection::Responses(encoder)) if encoder.incomplete.is_some());
                    let outcome = if reply.failed {
                        if let Some(StartFailure::EmbeddedHttp {
                            status: status @ (402 | 429),
                            body,
                        }) = reply.take_embedded_failure()
                        {
                            log.note_stream_limit(status, &body);
                        }
                        "stream-error"
                    } else if incomplete {
                        "incomplete"
                    } else {
                        "ok"
                    };
                    log.finish(outcome);
                }
                if let Some(bytes) = bytes {
                    return Some((Ok::<Bytes, Infallible>(bytes), (held, reply, log, encoder)));
                }
            }
        },
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("x-accel-buffering", "no")
        .header("x-aubridge-provider", provider)
        .body(Body::from_stream(stream))
        .expect("build streaming response")
}

fn bytes_response(body: Bytes, provider: &str) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .header("x-aubridge-provider", provider)
        .body(Body::from(body))
        .expect("build native Chat response")
}

fn json_response(message: Value, provider: &str) -> Response<Body> {
    let mut response = axum::Json(message).into_response();
    if let Ok(provider) = HeaderValue::from_str(provider) {
        response
            .headers_mut()
            .insert("x-aubridge-provider", provider);
    }
    response
}

struct DispatchedConnection {
    version: ConnectionVersion,
    uses_key: bool,
}

struct Attempt {
    started: Instant,
    id: String,
    model: String,
    kind: ProviderKind,
    protocol: WireProtocol,
    codex: bool,
    status: u16,
    headers: Option<HeaderMap>,
    usage: ReportedUsage,
    connection: Option<DispatchedConnection>,
    stream_limit: Option<HttpFailure>,
}

impl Attempt {
    fn connection_version(&self) -> Option<&ConnectionVersion> {
        self.connection
            .as_ref()
            .map(|connection| &connection.version)
    }
}

struct ContextFailure {
    estimated_input: u64,
    rejected_window: Option<u64>,
}

struct RequestLog {
    started: Instant,
    state: Arc<GatewayState>,
    affinity: Option<String>,
    decision: ledger::DecisionRecord,
    attempts: Vec<Value>,
    skipped: Vec<route_plan::Skipped>,
    usage: Vec<ledger::UsageRecord>,
    active: Option<Attempt>,
    context_failure: Option<ContextFailure>,
    finished: bool,
}

impl RequestLog {
    fn new(
        state: Arc<GatewayState>,
        req: &MessagesRequest,
        headers: &HeaderMap,
        now: u64,
        started: Instant,
    ) -> Self {
        let prefix = format!("{}-", state.token_key());
        let agent = crate::security::caller_key(headers)
            .and_then(|key| key.strip_prefix(&prefix).map(str::to_string))
            .unwrap_or_else(|| "-".to_string());
        Self {
            started,
            state,
            affinity: route_plan::session_key(req, headers),
            decision: ledger::DecisionRecord {
                ts: now,
                model: req.model.clone(),
                agent,
                ..Default::default()
            },
            attempts: Vec::new(),
            skipped: Vec::new(),
            usage: Vec::new(),
            active: None,
            context_failure: None,
            finished: false,
        }
    }

    fn start(&mut self, candidate: &Candidate, connection: DispatchedConnection) {
        self.active = Some(Attempt {
            started: Instant::now(),
            id: candidate.manifest.id.clone(),
            model: candidate.model.clone(),
            kind: candidate.manifest.kind,
            protocol: candidate.manifest.provider.protocol,
            codex: candidate.codex,
            status: 0,
            headers: None,
            usage: ReportedUsage::default(),
            connection: Some(connection),
            stream_limit: None,
        });
    }

    fn content_ready(&mut self) {
        self.decision.first_content_ms = Some(self.started.elapsed().as_millis() as u64);
    }

    fn active_mut(&mut self) -> &mut Attempt {
        self.active.as_mut().expect("started attempt")
    }

    fn note_breaker_failure(&self) {
        let attempt = self.active.as_ref().expect("started attempt");
        self.state
            .update_current_connection(&attempt.id, attempt.connection_version(), || {
                self.state.apply_breaker_failure(&attempt.id, unix_now());
            });
    }

    fn reject_before_content(
        &mut self,
        candidate: &Candidate,
        req: &MessagesRequest,
        failure: StartFailure,
    ) -> Option<ClientError> {
        match failure {
            StartFailure::EmbeddedHttp { status, body } => {
                let headers = self.active_mut().headers.clone().unwrap_or_default();
                self.reject(
                    candidate,
                    req,
                    status,
                    &headers,
                    FailureBody::Embedded(&body),
                )
            }
            StartFailure::Stream("context_length_exceeded") => {
                self.reject_client_error(candidate, req, ClientError::ContextLength)
            }
            StartFailure::Stream(reason) => {
                self.note_breaker_failure();
                self.end_attempt(reason, false);
                None
            }
        }
    }

    fn reject(
        &mut self,
        candidate: &Candidate,
        req: &MessagesRequest,
        status: u16,
        headers: &HeaderMap,
        body: FailureBody<'_>,
    ) -> Option<ClientError> {
        let now = unix_now();
        let failure =
            classify_http_failure(FailureSource::from(candidate), status, headers, body, now);
        if let Some(error) = failure.client_error {
            return self.reject_client_error(candidate, req, error);
        }
        let id = &candidate.manifest.id;
        let attempt = self.active.as_ref().expect("started attempt");
        self.state
            .update_current_connection(id, attempt.connection_version(), || {
                if status == 401
                    && attempt
                        .connection
                        .as_ref()
                        .is_some_and(|sent| sent.uses_key)
                {
                    self.state.set_key_retry_at(id, Some(now + KEY_RETRY_SECS));
                }
                failure.apply_health(&self.state, id, &candidate.model, now);
                if matches!(status, 408 | 500..=528 | 530..=599) {
                    self.state.apply_breaker_failure(id, now);
                }
            });
        self.end_attempt(failure.reason, false);
        None
    }

    fn note_stream_limit(&mut self, status: u16, body: &str) {
        let attempt = self.active_mut();
        let source = FailureSource {
            id: &attempt.id,
            kind: attempt.kind,
            protocol: attempt.protocol,
            codex: attempt.codex,
        };
        let headers = attempt.headers.as_ref().expect("stream response headers");
        attempt.stream_limit = Some(classify_http_failure(
            source,
            status,
            headers,
            FailureBody::Embedded(body),
            unix_now(),
        ));
    }

    fn reject_client_error(
        &mut self,
        candidate: &Candidate,
        req: &MessagesRequest,
        error: ClientError,
    ) -> Option<ClientError> {
        self.end_attempt(error.code(), false);
        if matches!(error, ClientError::ContextLength) {
            let failure = self.context_failure.get_or_insert_with(|| ContextFailure {
                estimated_input: translate::count::estimate_tokens(req) as u64,
                rejected_window: None,
            });
            failure.rejected_window = failure.rejected_window.max(candidate.context_window());
            return None;
        }
        self.finish(error.code());
        Some(error)
    }

    fn context_block(&self, candidate: &Candidate) -> Option<route_plan::Block> {
        let failure = self.context_failure.as_ref()?;
        let window = candidate.context_window();
        let fits = window.is_some_and(|window| {
            window >= failure.estimated_input
                && failure
                    .rejected_window
                    .is_none_or(|rejected| window > rejected)
        });
        (!fits).then(|| {
            route_plan::Block::context("context-fallback-window", window, failure.estimated_input)
        })
    }

    fn skip(&mut self, candidate: &Candidate, block: route_plan::Block) {
        route_plan::record_skip(
            &mut self.skipped,
            &candidate.manifest.id,
            &candidate.model,
            candidate.manifest.kind,
            block,
        );
    }

    fn end_attempt(&mut self, outcome: &str, chosen: bool) {
        let Some(mut attempt) = self.active.take() else {
            return;
        };
        let now = unix_now();
        let model = attempt
            .usage
            .model
            .take()
            .unwrap_or_else(|| attempt.model.clone());
        let mut entry = serde_json::json!({
            "id":attempt.id, "status":attempt.status, "model":model,
            "requested_model":attempt.model, "kind":attempt.kind, "chosen":chosen,
            "duration_ms":attempt.started.elapsed().as_millis() as u64,
        });
        if outcome != "ok" {
            entry["reason"] = Value::String(outcome.into());
        }
        self.attempts.push(entry);
        if chosen {
            self.decision.provider.clone_from(&attempt.id);
            self.decision.served_model = Some(model.clone());
            self.decision.input_tokens = attempt.usage.input;
            self.decision.output_tokens = attempt.usage.output;
        }
        self.state
            .update_current_connection(&attempt.id, attempt.connection_version(), || {
                if outcome == "ok" {
                    self.state.apply_success(&attempt.id, now);
                    self.state.apply_breaker_success(&attempt.id);
                    if attempt.kind == ProviderKind::Subscription {
                        if let Some(key) = &self.affinity {
                            self.state.record_affinity(
                                key,
                                &attempt.id,
                                attempt
                                    .usage
                                    .cache_read
                                    .saturating_add(attempt.usage.cache_write),
                            );
                        }
                    }
                } else if chosen && outcome == "stream-error" && attempt.stream_limit.is_none() {
                    self.state.apply_breaker_failure(&attempt.id, now);
                }
                if let Some(headers) = &attempt.headers {
                    observe_quota(
                        &self.state,
                        &attempt.id,
                        attempt.protocol,
                        attempt.codex,
                        headers,
                        now,
                    );
                }
                if let Some(failure) = &attempt.stream_limit {
                    // A window sample must not replace an explicit terminal limit.
                    failure.apply_health(&self.state, &attempt.id, &attempt.model, now);
                }
            });
        if attempt.usage.input.is_some() || attempt.usage.output.is_some() {
            let accounting = if outcome == "ok"
                && attempt.usage.input.is_some()
                && attempt.usage.output.is_some()
            {
                ledger::Accounting::Reported
            } else {
                ledger::Accounting::Partial
            };
            self.usage.push(ledger::UsageRecord {
                ts: self.decision.ts,
                agent: self.decision.agent.clone(),
                provider: attempt.id,
                model,
                input_tokens: attempt.usage.input,
                output_tokens: attempt.usage.output,
                accounting,
            });
        }
    }

    fn finish(&mut self, outcome: &str) {
        let chosen = self
            .active
            .as_ref()
            .is_some_and(|attempt| attempt.status == 200);
        self.end_attempt(outcome, chosen);
        self.decision.outcome = outcome.to_string();
        self.decision.duration_ms = Some(self.started.elapsed().as_millis() as u64);
        self.decision.attempts = serde_json::to_string(&self.attempts).unwrap_or_default();
        self.decision.skipped = serde_json::to_string(&self.skipped).ok();
        self.state.record_request(ledger::CompletedRequest {
            decision: std::mem::take(&mut self.decision),
            usage: std::mem::take(&mut self.usage),
        });
        self.finished = true;
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("cancelled");
        }
    }
}

struct HttpFailure {
    decision: FailureDecision,
    model_cooldown: Option<ModelCooldown>,
    reason: &'static str,
    client_error: Option<ClientError>,
}

impl HttpFailure {
    fn apply_health(&self, state: &GatewayState, id: &str, model: &str, now: u64) {
        if let Some(cooldown) = self.model_cooldown {
            state.cool_model(id, model, cooldown, now);
        } else {
            state.apply_decision(id, &self.decision, now);
        }
    }
}

struct FailureSource<'a> {
    id: &'a str,
    kind: ProviderKind,
    protocol: WireProtocol,
    codex: bool,
}

enum FailureBody<'a> {
    Http(&'a str),
    Embedded(&'a str),
}

impl<'a> From<&'a Candidate> for FailureSource<'a> {
    fn from(candidate: &'a Candidate) -> Self {
        Self {
            id: &candidate.manifest.id,
            kind: candidate.manifest.kind,
            protocol: candidate.manifest.provider.protocol,
            codex: candidate.codex,
        }
    }
}

#[derive(Clone, Copy)]
enum ClientError {
    InvalidRequest,
    ModelNotFound,
    FreeRouteRestricted,
    ContextLength,
    RequestTooLarge,
    RateLimit(u64),
    Timeout,
}

impl ClientError {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ModelNotFound => "model_not_found",
            Self::FreeRouteRestricted => openrouter::RESTRICTED,
            Self::ContextLength => "context_length_exceeded",
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimit(_) => "rate_limit_exceeded",
            Self::Timeout => "response_timeout",
        }
    }

    fn response(self, input: Input<'_>) -> Response<Body> {
        let (status, kind, message) = match self {
            Self::InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Upstream rejected the request as invalid",
            ),
            Self::ModelNotFound => (
                StatusCode::NOT_FOUND,
                if matches!(input, Input::Messages(_)) {
                    "not_found_error"
                } else {
                    "invalid_request_error"
                },
                "The requested model is not configured or was removed",
            ),
            Self::FreeRouteRestricted => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The built-in OpenRouter route accepts free models and client-side tools only",
            ),
            Self::ContextLength => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Prompt is too long for the model's context window",
            ),
            Self::RequestTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request is too large",
            ),
            Self::RateLimit(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Compatible providers are waiting for rate or quota limits to reset",
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "Gateway response deadline exceeded",
            ),
        };
        let mut response = match input {
            Input::Chat(_) | Input::Responses(_) => {
                chat::error_response(status, message, kind, Some(self.code()))
            }
            Input::Messages(_) => (
                status,
                axum::Json(
                    serde_json::json!({"type":"error", "error":{"type":kind, "message":message}}),
                ),
            )
                .into_response(),
        };
        if let Self::RateLimit(wait) = self {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, wait.into());
        }
        response
    }
}

fn classify_http_failure(
    source: FailureSource<'_>,
    status: u16,
    headers: &HeaderMap,
    body: FailureBody<'_>,
    now: u64,
) -> HttpFailure {
    let (body, is_http) = match body {
        FailureBody::Http(body) => (body, true),
        FailureBody::Embedded(body) => (body, false),
    };
    let payload: Value = serde_json::from_str(body).unwrap_or_default();
    let codex_body_reset = if is_http
        && source.codex
        && status == 429
        && payload["error"]["type"] == "usage_limit_reached"
    {
        payload["error"]["resets_at"]
            .as_u64()
            .filter(|reset| valid_quota_reset(*reset, now))
    } else {
        None
    };
    let sample = source.codex.then(|| sample_codex_windows(headers, now));
    let (anthropic_exhausted, anthropic_reset) =
        if status == 429 && source.protocol == WireProtocol::Anthropic {
            anthropic_exhausted_windows(headers, now)
        } else {
            (false, None)
        };
    let reset_at = if anthropic_exhausted {
        anthropic_reset
    } else {
        sample
            .as_ref()
            .filter(|sample| sample.exhausted)
            .and_then(|sample| sample.reset_at)
            .or(codex_body_reset)
            .or_else(|| sample.as_ref().and_then(|sample| sample.reset_at))
    }
    .or_else(|| {
        header_u64(headers, "x-ratelimit-reset")
            .map(|reset| {
                if reset >= 1_000_000_000_000 {
                    reset / 1000
                } else {
                    reset
                }
            })
            .filter(|reset| valid_quota_reset(*reset, now))
    });
    let error = payload.get("error").unwrap_or(&payload);
    let remaining_positive = header_u64(headers, "x-ratelimit-remaining").is_some_and(|n| n > 0)
        || error["remaining"]
            .as_u64()
            .or_else(|| payload["remaining"].as_u64())
            .is_some_and(|n| n > 0);
    let code = error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or_default();
    let scope = headers
        .get("x-ratelimit-scope")
        .and_then(|v| v.to_str().ok())
        .or_else(|| error["scope"].as_str())
        .or_else(|| error["metadata"]["scope"].as_str())
        .map(str::to_ascii_lowercase);
    // Kimi also uses this error type for concurrency limits. Require quota
    // wording before cooling the subscription for an account window.
    let subscription_quota_403 = status == 403
        && source.kind == ProviderKind::Subscription
        && error["type"] == "access_terminated_error"
        && error["message"].as_str().is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            !message.contains("concurrent")
                && (message.contains("usage limit") || message.contains("quota will reset"))
        });
    let window_exhausted = subscription_quota_403
        || anthropic_exhausted
        || codex_body_reset.is_some()
        || sample.as_ref().is_some_and(|sample| sample.exhausted);
    let lower = body.to_lowercase();
    let unavailable = !subscription_quota_403
        && matches!(status, 400 | 403 | 404)
        && (matches!(
            code,
            "model_not_found"
                | "model_not_supported"
                | "model_not_available"
                | "model_access_denied"
        ) || [
            "model not found",
            "model does not exist",
            "not a valid model id",
            "no endpoints found for",
        ]
        .iter()
        .any(|message| lower.contains(message)));
    let account_scoped = matches!(
        scope.as_deref(),
        Some("account" | "key" | "organization" | "project" | "global")
    ) || matches!(
        code,
        "account_quota_exceeded" | "account_quota_exhausted" | "account_rate_limit_exceeded"
    ) || lower.contains("free-models-per-day")
        || lower.contains("free-models-per-min")
        || window_exhausted;
    // OpenRouter's own free-tier limits are shared. A reported upstream
    // provider limit can be retried on a different configured model.
    let openrouter = source.id == "openrouter";
    let explicit_model = scope.as_deref() == Some("model")
        || matches!(
            code,
            "model_rate_limit_exceeded" | "model_quota_exceeded" | "model_quota_exhausted"
        )
        || (openrouter && error["metadata"]["provider_name"].as_str().is_some());
    let observed_status = match (status, code) {
        _ if subscription_quota_403 => 429,
        (
            400 | 403 | 422,
            "insufficient_balance" | "insufficient_credit" | "billing_hard_limit_reached",
        ) => 402,
        (
            400 | 403 | 422,
            "quota_exceeded" | "account_quota_exceeded" | "account_quota_exhausted",
        ) => 429,
        _ => status,
    };
    let client_error = upstream_client_error(status, observed_status, code, &lower, unavailable);
    let signal = UpstreamSignal {
        status: observed_status,
        body: lower,
        retry_after: retry_after(headers, now),
        reset_at,
        remaining_positive,
        window_exhausted,
        model_scoped: unavailable || (observed_status == 429 && explicit_model && !account_scoped),
        account_restricted: observed_status == 403
            && !explicit_model
            && !matches!(scope.as_deref(), Some("request" | "tool"))
            && explicit_account_denial(headers, error, body),
    };
    let decision = router::classify::classify(&signal, now);
    let model_cooldown = match decision.bucket {
        Some(Bucket::Other) if unavailable => Some(ModelCooldown {
            reason: ModelCooldownReason::ModelUnavailable,
            until: now + 60,
        }),
        Some(Bucket::QuotaUntil(reset)) if signal.model_scoped => Some(ModelCooldown {
            reason: ModelCooldownReason::ModelQuotaExhausted,
            until: reset.saturating_add(1),
        }),
        Some(Bucket::RateFor(secs))
            if observed_status == 429
                && !account_scoped
                && (signal.model_scoped
                    || (!openrouter && source.kind == ProviderKind::FreeTier)) =>
        {
            Some(ModelCooldown {
                reason: ModelCooldownReason::ModelRateLimit,
                until: now + secs.max(1),
            })
        }
        _ => None,
    };
    let reason = client_error
        .map(ClientError::code)
        .or_else(|| model_cooldown.map(|cooldown| route_plan::model_reason(cooldown.reason)))
        .unwrap_or(match (observed_status, &decision.bucket) {
            (401, _) => "credential-rejected",
            (403, _) => "access-denied",
            (_, Some(Bucket::Credit)) => "account-credit-exhausted",
            (_, Some(Bucket::QuotaUntil(_))) => "account-quota-exhausted",
            (_, Some(Bucket::RateFor(_))) => "account-rate-limit",
            (_, Some(Bucket::Overload)) => "provider-overloaded",
            _ => "upstream-error",
        });
    HttpFailure {
        decision,
        model_cooldown,
        reason,
        client_error,
    }
}

fn explicit_account_denial(headers: &HeaderMap, error: &Value, body: &str) -> bool {
    let body = body.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let html_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            value.trim().eq_ignore_ascii_case("text/html")
                || value.trim().eq_ignore_ascii_case("application/xhtml+xml")
        });
    if body.starts_with('<') || html_type {
        return false;
    }
    if ["code", "type"].into_iter().any(|field| {
        error[field].as_str().is_some_and(|code| {
            ["account_banned", "account_suspended", "account_disabled"]
                .into_iter()
                .any(|known| code.trim().eq_ignore_ascii_case(known))
        })
    }) {
        return true;
    }

    // Inspect the error message itself, without matching quoted or nested text.
    let message = error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .unwrap_or(body)
        .trim()
        .to_ascii_lowercase();
    let sentence_end = |tail: &str| {
        tail.is_empty() || tail.starts_with(['.', ',', ':', '!', '。', '，', '：', '！'])
    };
    if let Some(status) = ["account ", "your account ", "this account "]
        .into_iter()
        .find_map(|subject| message.strip_prefix(subject))
    {
        let status = ["has been ", "is ", "was "]
            .into_iter()
            .find_map(|verb| status.strip_prefix(verb))
            .unwrap_or(status);
        return ["banned", "suspended", "disabled"].into_iter().any(|word| {
            status.strip_prefix(word).is_some_and(|tail| {
                sentence_end(tail) || tail.starts_with(" due to ") || tail.starts_with(" until ")
            })
        });
    }
    let message = ["您的", "你的", "该"]
        .into_iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .unwrap_or(&message);
    ["账号", "帐号", "账户"]
        .into_iter()
        .find_map(|subject| message.strip_prefix(subject))
        .is_some_and(|status| {
            let status = ["已被", "已", "被"]
                .into_iter()
                .find_map(|verb| status.strip_prefix(verb))
                .unwrap_or(status);
            ["封禁", "停用", "禁用"]
                .into_iter()
                .any(|word| status.strip_prefix(word).is_some_and(sentence_end))
        })
}

fn upstream_client_error(
    status: u16,
    observed_status: u16,
    code: &str,
    body: &str,
    model_unavailable: bool,
) -> Option<ClientError> {
    if matches!(status, 400 | 413 | 422)
        && (matches!(code, "context_length_exceeded" | "context_window_exceeded")
            || [
                "context_length_exceeded",
                "prompt is too long",
                "maximum context length",
                "exceeds the model's context",
                "exceeds the context window",
                "input is too long for the model",
            ]
            .iter()
            .any(|part| body.contains(part)))
    {
        return Some(ClientError::ContextLength);
    }
    if status == 413 {
        return Some(ClientError::RequestTooLarge);
    }
    let provider_specific = model_unavailable
        || matches!(
            code,
            "unsupported_parameter" | "unsupported_value" | "model_not_supported"
        )
        || [
            "unknown parameter",
            "unrecognized request argument",
            "does not support tools",
            "does not support tool use",
            "schema exceeds the maximum nesting",
            "unknown field",
        ]
        .iter()
        .any(|part| body.contains(part));
    (matches!(observed_status, 400 | 422) && !provider_specific)
        .then_some(ClientError::InvalidRequest)
}

fn observe_quota(
    state: &GatewayState,
    id: &str,
    protocol: WireProtocol,
    codex: bool,
    headers: &HeaderMap,
    now: u64,
) {
    if codex {
        let sample = sample_codex_windows(headers, now);
        state.record_utilization(id, sample.percent_5h, sample.percent_7d);
        if sample.exhausted {
            state.apply_decision(
                id,
                &router::classify::FailureDecision {
                    bucket: Some(router::classify::Bucket::QuotaUntil(
                        sample.reset_at.unwrap_or(now + 900),
                    )),
                    sink: true,
                    consumes_retry: true,
                    refresh: false,
                    disable: false,
                },
                now,
            );
        } else if let Some(reset_at) = sample.reset_at {
            state.apply_reset_sample(id, reset_at);
        }
    } else if protocol == WireProtocol::Anthropic {
        let (util_5h, util_7d, reset_at) = sample_anthropic_utilization(headers);
        if util_5h.is_some() || util_7d.is_some() {
            state.record_utilization(id, util_5h, util_7d);
        }
        if let Some(reset_at) = reset_at {
            state.apply_reset_sample(id, reset_at);
        }
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// x-codex 窗口被动采样结果(§8.2)。
struct CodexSample {
    exhausted: bool,
    /// 受限窗口的重置时刻(unix 秒)。
    reset_at: Option<u64>,
    /// 5h(secondary)窗口利用率百分比(§8.2,写进源状态)。
    percent_5h: Option<f64>,
    /// 7d(primary)窗口利用率百分比。
    percent_7d: Option<f64>,
}

/// 解析 `x-codex-primary/secondary-used-percent` 与
/// `-reset-after-seconds`(primary=7d,secondary=5h,sub2api 归一化口径,
/// §8.2)。任一窗口 ≥100% 即耗尽;reset 取受限窗口中最早的重置
/// 时刻(双窗口都耗尽时休到较早的,下次请求重新采样)。
/// 非数值头(坏 percent 等)按未知处理,绝不臆断。
fn sample_codex_windows(headers: &HeaderMap, now: u64) -> CodexSample {
    let mut exhausted = false;
    let mut exhausted_reset: Option<u64> = None;
    let mut any_reset: Option<u64> = None;
    let mut percent_5h = None;
    let mut percent_7d = None;
    for (w, util) in [("primary", &mut percent_7d), ("secondary", &mut percent_5h)] {
        let percent = header_f64(headers, &format!("x-codex-{w}-used-percent"));
        let secs = header_u64(headers, &format!("x-codex-{w}-reset-after-seconds"));
        *util = percent;
        let Some(reset) = secs.map(|s| now.saturating_add(s)) else {
            continue;
        };
        any_reset = Some(any_reset.map_or(reset, |t| t.min(reset)));
        if percent.is_some_and(|p| p >= 100.0) {
            exhausted = true;
            exhausted_reset = Some(exhausted_reset.map_or(reset, |t| t.min(reset)));
        }
    }
    CodexSample {
        exhausted,
        reset_at: if exhausted {
            exhausted_reset
        } else {
            any_reset
        },
        percent_5h,
        percent_7d,
    }
}

fn valid_quota_reset(reset: u64, now: u64) -> bool {
    reset > now && reset - now <= 32 * 86_400
}

fn anthropic_exhausted_windows(headers: &HeaderMap, now: u64) -> (bool, Option<u64>) {
    let mut exhausted = false;
    let mut reset_at = None;
    for window in ["5h", "7d"] {
        let prefix = format!("anthropic-ratelimit-unified-{window}");
        let matches = |suffix: &str, expected: &str| {
            headers
                .get(format!("{prefix}-{suffix}"))
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case(expected))
        };
        if matches("status", "rejected")
            || matches("surpassed-threshold", "true")
            || header_f64(headers, &format!("{prefix}-utilization"))
                .is_some_and(|v| v.is_finite() && v >= 1.0)
        {
            exhausted = true;
            // Wait for every exhausted window with a known reset.
            reset_at = reset_at.max(
                header_u64(headers, &format!("{prefix}-reset"))
                    .filter(|reset| valid_quota_reset(*reset, now)),
            );
        }
    }
    (exhausted, reset_at)
}

/// Anthropic 协议成功响应的 unified 限流头被动采样(§8.2:利用率
/// 与 reset 时刻;头是 0–1 小数,换算百分比入状态)。头缺失 = 未知,
/// 不写。返回 (util_5h, util_7d, reset_at)。
fn sample_anthropic_utilization(headers: &HeaderMap) -> (Option<f64>, Option<f64>, Option<u64>) {
    let pct = |w: &str| {
        header_f64(
            headers,
            &format!("anthropic-ratelimit-unified-{w}-utilization"),
        )
        .map(|v| v * 100.0)
    };
    let reset_at = header_u64(headers, "anthropic-ratelimit-unified-5h-reset");
    (pct("5h"), pct("7d"), reset_at)
}

/// 整数值响应头(十进制;缺失/非数字返回 None)。
fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

fn retry_after(headers: &HeaderMap, now: u64) -> Option<u64> {
    let value = headers
        .get(axum::http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    value.parse().ok().or_else(|| {
        let at = httpdate::parse_http_date(value)
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs();
        at.checked_sub(now).filter(|&wait| wait > 0)
    })
}

/// 浮点数值响应头(x-codex used-percent 可为小数;缺失/非数字
/// 返回 None)。
fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

fn sse_response(events: &[AnthropicEvent], provider: &str) -> Response<Body> {
    let body = events
        .iter()
        .cloned()
        .flat_map(|event| Frame::translated(event).bytes)
        .collect::<Vec<_>>();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("x-aubridge-provider", provider)
        .body(Body::from(body))
        .expect("build sse response")
}

fn anthropic_error(msg: &str) -> Response<Body> {
    let events = vec![error_terminator(msg), AnthropicEvent::MessageStop];
    sse_response(&events, "none")
}

/// 循环护栏本地作答(§8.5):一条完整 assistant 文本消息(end_turn),
/// 让 agent 正常收尾换路。文本不指名 aubridge 或厂商(注入纪律),
/// usage 记零(没打上游,就没有 token)。
fn loop_guard_answer(model: &str, streaming: bool, input: Input<'_>) -> Response<Body> {
    let text =
        "同一工具调用已连续重复多轮,结果不会再变化。请检查调用参数是否正确,或换一种方法推进任务。";
    if !streaming {
        let message = serde_json::json!({
            "id": "msg_local_guard",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type":"text", "text":text}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": Usage::default(),
        });
        let message = if matches!(input, Input::Chat(_)) {
            chat::completion(
                &message,
                &local_usage(),
                WireProtocol::Anthropic,
                model,
                unix_now(),
            )
        } else {
            message
        };
        return json_response(message, "loop-guard");
    }
    let events = vec![
        AnthropicEvent::MessageStart {
            message: MessageStartBody {
                id: "msg_local_guard".into(),
                model: model.into(),
                role: "assistant".into(),
                usage: Usage::default(),
            },
        },
        AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: AnthropicDelta::TextDelta { text: text.into() },
        },
        AnthropicEvent::ContentBlockStop { index: 0 },
        AnthropicEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some("end_turn".into()),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
        AnthropicEvent::MessageStop,
    ];
    if let Some(mut encoder) =
        Projection::for_input(input, model, unix_now(), WireProtocol::Anthropic)
    {
        let usage = local_usage();
        let bytes = events
            .into_iter()
            .filter_map(|event| encoder.encode(&Frame::translated(event).event, &usage))
            .flatten()
            .collect::<Vec<_>>();
        return Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-aubridge-provider", "loop-guard")
            .body(Body::from(bytes))
            .expect("build local Chat stream");
    }
    sse_response(&events, "loop-guard")
}

fn local_usage() -> ReportedUsage {
    ReportedUsage {
        input: Some(0),
        output: Some(0),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_ignores_expired_and_invalid_dates() {
        let now = 1_700_000_000;
        let expired =
            httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(now - 1));
        let mut headers = HeaderMap::new();
        for value in [expired.as_str(), "not a date"] {
            headers.insert(axum::http::header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_after(&headers, now), None);
        }
    }

    #[test]
    fn codex_http_quota_body_respects_reset_precedence_and_trust_boundary() {
        let now = 1_700_000_000;
        let body = |kind, reset| format!(r#"{{"error":{{"type":"{kind}","resets_at":{reset}}}}}"#);
        let valid = body("usage_limit_reached", (now + 3600).to_string());
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-codex-primary-used-percent", "20"),
            ("x-codex-primary-reset-after-seconds", "60"),
            ("x-ratelimit-reset", "1700000120"),
            ("x-ratelimit-remaining", "12"),
            ("x-ratelimit-scope", "model"),
            ("retry-after", "15"),
        ] {
            headers.insert(key, HeaderValue::from_static(value));
        }
        let classify = |codex, status, body: FailureBody<'_>, headers: &HeaderMap| {
            classify_http_failure(
                FailureSource {
                    id: if codex { "chatgpt" } else { "custom" },
                    kind: ProviderKind::Subscription,
                    protocol: WireProtocol::Responses,
                    codex,
                },
                status,
                headers,
                body,
                now,
            )
        };

        let result = classify(true, 429, FailureBody::Http(&valid), &headers);
        assert_eq!(result.decision.bucket, Some(Bucket::QuotaUntil(now + 3600)));
        assert!(result.decision.sink);
        assert!(result.model_cooldown.is_none());

        for (codex, body) in [
            (false, FailureBody::Http(&valid)),
            (true, FailureBody::Embedded(&valid)),
        ] {
            assert_eq!(
                classify(codex, 429, body, &headers).decision.bucket,
                Some(Bucket::RateFor(15))
            );
        }
        assert_ne!(
            classify(true, 400, FailureBody::Http(&valid), &headers)
                .decision
                .bucket,
            Some(Bucket::QuotaUntil(now + 3600))
        );
        for invalid in [
            body("usage_limit_reached", now.to_string()),
            body("usage_limit_reached", (now + 32 * 86_400 + 1).to_string()),
            body("usage_limit_reached", format!(r#""{}""#, now + 3600)),
            body("rate_limit_exceeded", (now + 3600).to_string()),
            format!(
                r#"{{"type":"usage_limit_reached","resets_at":{}}}"#,
                now + 3600
            ),
            "{".to_string(),
        ] {
            assert_eq!(
                classify(true, 429, FailureBody::Http(&invalid), &headers)
                    .decision
                    .bucket,
                Some(Bucket::RateFor(15)),
                "{invalid}"
            );
        }

        headers.insert(
            "x-codex-secondary-used-percent",
            HeaderValue::from_static("100"),
        );
        headers.insert(
            "x-codex-secondary-reset-after-seconds",
            HeaderValue::from_static("240"),
        );
        let result = classify(true, 429, FailureBody::Http(&valid), &headers);
        assert_eq!(result.decision.bucket, Some(Bucket::QuotaUntil(now + 240)));
        assert!(result.decision.sink);
        assert!(result.model_cooldown.is_none());
    }
}

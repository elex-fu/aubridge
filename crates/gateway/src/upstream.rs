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
use crate::chatgpt::LoginSnapshot;
use crate::openrouter;
use crate::route_plan::{self, Candidate, Credential};
use crate::state::{ConnectionVersion, GatewayState};
use crate::stream::{self, Frame, Reply, ReportedUsage, StartFailure};
use credentials::quota::{observation_stamp, ObservationStamp};
use manifest::quota::{ObservationSource, QuotaWindow};

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
    Gemini(&'a translate::gemini_input::GeminiInput),
    Chat(&'a ChatInput),
    Responses(&'a translate::responses_input::ResponsesInput),
    Compact(&'a crate::compaction::CompactInput),
}

impl Input<'_> {
    fn allows(self, manifest: &manifest::schema::Manifest) -> bool {
        let protocol = manifest.provider.protocol;
        match (self, protocol) {
            (Self::Gemini(input), protocol) => input.supports_target(
                gemini_wire(protocol),
                crate::gemini::supports_chat_top_k(manifest),
            ),
            (Self::Compact(_), protocol) => protocol == WireProtocol::Responses,
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
    crate::diagnostics::recorded(
        forward_input(state, req, client_headers, Input::Messages(body)).await,
    )
}

fn gemini_wire(protocol: WireProtocol) -> translate::media::Wire {
    match protocol {
        WireProtocol::Chat => translate::media::Wire::Chat,
        WireProtocol::Responses => translate::media::Wire::Responses,
        WireProtocol::Anthropic => translate::media::Wire::Messages,
    }
}
pub(crate) async fn forward_gemini(
    state: Arc<GatewayState>,
    input: &translate::gemini_input::GeminiInput,
    headers: &HeaderMap,
) -> Response<Body> {
    crate::diagnostics::recorded(
        forward_input(state, &input.request, headers, Input::Gemini(input)).await,
    )
}
pub(crate) fn count_gemini(
    state: &GatewayState,
    input: &translate::gemini_input::GeminiInput,
    headers: &HeaderMap,
) -> Response<Body> {
    let req = &input.request;
    let valid =
        translate::media::check(req, translate::media::Wire::Messages, true).and_then(|_| {
            translate::media::validate_urls(req, |url| {
                reqwest::Url::parse(url).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some()
                        && u.username().is_empty()
                        && u.password().is_none()
                })
            })
        });
    if let Err(reason) = valid {
        return crate::gemini::error_response(StatusCode::BAD_REQUEST, reason);
    }
    let plan = route_plan::plan(
        state,
        req,
        headers,
        unix_now(),
        false,
        false,
        |manifest, _| {
            let wire = gemini_wire(manifest.provider.protocol);
            if !input.supports_target(wire, crate::gemini::supports_chat_top_k(manifest)) {
                return Some(route_plan::Block::new("native-protocol-required", None));
            }
            if crate::claude_cli::owns(manifest) {
                return Some(route_plan::Block::new("native-protocol-required", None));
            }
            translate::media::check(req, wire, false)
                .err()
                .map(|reason| route_plan::Block::new(reason, None))
        },
    );
    let Some(candidate) = plan.ordered.first() else {
        return crate::gemini::error_response(
            StatusCode::BAD_REQUEST,
            "No compatible model for token estimate",
        );
    };
    let mut req = req.clone();
    req.model.clone_from(&candidate.model);
    let wire = gemini_wire(candidate.manifest.provider.protocol);
    let value = match wire {
        translate::media::Wire::Chat => serde_json::to_value(messages_request_to_chat(&req)),
        translate::media::Wire::Responses => {
            serde_json::to_value(messages_request_to_responses(&req))
        }
        translate::media::Wire::Messages => serde_json::to_value(&req),
    };
    let value = value
        .ok()
        .and_then(|body| translate::media::encode(&req, wire, body).ok())
        .and_then(|body| {
            input
                .apply_target_options(
                    wire,
                    body,
                    crate::gemini::supports_chat_top_k(&candidate.manifest),
                )
                .ok()
        })
        .map(|mut body| {
            if wire == translate::media::Wire::Messages && req.max_tokens == 0 {
                let output = candidate
                    .manifest
                    .model_facts
                    .get(&candidate.model)
                    .and_then(|facts| facts.output)
                    .unwrap_or(4096)
                    .min(4096);
                body["max_tokens"] = serde_json::json!(output);
            }
            if wire == translate::media::Wire::Responses && req.max_tokens > 0 {
                body["max_output_tokens"] = serde_json::json!(req.max_tokens);
            }
            body["stream"] = Value::Bool(true);
            body
        });
    match value {Some(value)=>axum::Json(serde_json::json!({"totalTokens":translate::count::estimate_serialized(&value),"estimated":true})).into_response(),
        None=>crate::gemini::error_response(StatusCode::BAD_REQUEST,"Cannot estimate this request without losing content")}
}

pub(crate) async fn forward_chat(
    state: Arc<GatewayState>,
    input: &ChatInput,
    client_headers: &HeaderMap,
) -> Response<Body> {
    crate::diagnostics::recorded(
        forward_input(state, &input.request, client_headers, Input::Chat(input)).await,
    )
}

pub(crate) async fn forward_responses(
    state: Arc<GatewayState>,
    input: &translate::responses_input::ResponsesInput,
    client_headers: &HeaderMap,
) -> Response<Body> {
    crate::diagnostics::recorded(
        forward_input(
            state,
            &input.request,
            client_headers,
            Input::Responses(input),
        )
        .await,
    )
}

pub(crate) async fn forward_compact(
    state: Arc<GatewayState>,
    input: &crate::compaction::CompactInput,
    headers: &HeaderMap,
) -> Response<Body> {
    crate::diagnostics::recorded(
        forward_input(state, &input.input.request, headers, Input::Compact(input)).await,
    )
}

async fn forward_input(
    state: Arc<GatewayState>,
    req: &MessagesRequest,
    client_headers: &HeaderMap,
    input: Input<'_>,
) -> Response<Body> {
    let valid_url = |url: &str| {
        reqwest::Url::parse(url).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
    };
    let raw_urls = match input {
        Input::Chat(chat) => translate::media::validate_raw_urls(
            chat.original(),
            translate::media::Wire::Chat,
            valid_url,
        ),
        Input::Responses(responses) => translate::media::validate_raw_urls(
            responses.original(),
            translate::media::Wire::Responses,
            valid_url,
        ),
        Input::Compact(compact) => translate::media::validate_raw_urls(
            &compact.raw,
            translate::media::Wire::Responses,
            valid_url,
        ),
        _ => Ok(()),
    };
    if let Err(reason) = raw_urls {
        return ClientError::Attachment(reason).response(input);
    }
    if let Err(reason) = translate::media::validate_urls(req, valid_url) {
        return ClientError::Attachment(reason).response(input);
    }
    if let Err(reason) = translate::media::check(req, translate::media::Wire::Messages, true) {
        return ClientError::Attachment(reason).response(input);
    }
    let now = unix_now();
    let started = Instant::now();
    let response_deadline = started + RESPONSE_BUDGET;
    let streaming = req.stream != Some(false);
    let claude_continuation = crate::claude_cli::continuation(req);
    let mut log = RequestLog::new(state.clone(), req, client_headers, now, started);
    let raw = match input {
        Input::Responses(r) => Some(r.original()),
        Input::Compact(c) => Some(&c.raw),
        _ => None,
    };
    let compact = matches!(input, Input::Compact(_)) || raw.is_some_and(crate::compaction::trigger);
    if compact {
        log.decision.purpose = Some("compact".into());
    }
    let history_owner = match raw.map(|raw| state.history_owner(raw)).transpose() {
        Ok(owner) => owner.flatten(),
        Err(message) => {
            log.finish("checkpoint-rejected");
            return crate::compaction::error(StatusCode::CONFLICT, message);
        }
    };
    if claude_continuation
        && (!state.claude_cli.ready() || crate::claude_cli::mixed_continuation(req))
    {
        log.finish("claude-continuation-invalid");
        return ClientError::InvalidRequest.response(input);
    }
    if !compact && router::guard::tool_loop_detected(req) {
        log.finish("guard");
        return loop_guard_answer(&req.model, streaming, input);
    }

    let request_media = translate::media::has_media(req)
        || match input {
            Input::Chat(chat) => {
                translate::media::raw_has_images(chat.original(), translate::media::Wire::Chat)
                    || translate::media::raw_has_documents(
                        chat.original(),
                        translate::media::Wire::Chat,
                    )
            }
            Input::Responses(responses) => {
                translate::media::raw_has_images(
                    responses.original(),
                    translate::media::Wire::Responses,
                ) || translate::media::raw_has_documents(
                    responses.original(),
                    translate::media::Wire::Responses,
                )
            }
            Input::Compact(compact) => {
                translate::media::raw_has_images(&compact.raw, translate::media::Wire::Responses)
                    || translate::media::raw_has_documents(
                        &compact.raw,
                        translate::media::Wire::Responses,
                    )
            }
            _ => false,
        };
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
            if crate::claude_cli::owns(manifest)
                && matches!(input,Input::Gemini(gemini) if gemini.original().get("generationConfig").and_then(Value::as_object).is_some_and(|options|options.keys().any(|key| !matches!(key.as_str(),"maxOutputTokens"|"candidateCount"|"responseMimeType"))))
            {
                return Some(route_plan::Block::new("native-protocol-required", None));
            }
            if request_media && crate::claude_cli::owns(manifest) {
                return Some(route_plan::Block::new(translate::media::UNSUPPORTED, None));
            }
            let raw_document = match input {
                Input::Chat(chat) => translate::media::raw_has_documents(
                    chat.original(),
                    translate::media::Wire::Chat,
                ),
                Input::Responses(responses) => translate::media::raw_has_documents(
                    responses.original(),
                    translate::media::Wire::Responses,
                ),
                Input::Compact(compact) => translate::media::raw_has_documents(
                    &compact.raw,
                    translate::media::Wire::Responses,
                ),
                _ => false,
            };
            if raw_document && state.is_chatgpt(manifest) {
                return Some(route_plan::Block::new(translate::media::UNSUPPORTED, None));
            }
            let raw_image = match input {
                Input::Chat(chat) => {
                    translate::media::raw_has_images(chat.original(), translate::media::Wire::Chat)
                }
                Input::Responses(responses) => translate::media::raw_has_images(
                    responses.original(),
                    translate::media::Wire::Responses,
                ),
                Input::Compact(compact) => translate::media::raw_has_images(
                    &compact.raw,
                    translate::media::Wire::Responses,
                ),
                _ => false,
            };
            if raw_image
                && manifest
                    .model_facts
                    .get(model)
                    .is_some_and(|f| f.image_input == Some(false))
            {
                return Some(route_plan::Block::new(
                    translate::media::IMAGE_UNSUPPORTED,
                    None,
                ));
            }
            let wire = match manifest.provider.protocol {
                WireProtocol::Anthropic => translate::media::Wire::Messages,
                WireProtocol::Chat => translate::media::Wire::Chat,
                WireProtocol::Responses => translate::media::Wire::Responses,
            };
            let native = matches!(
                (manifest.provider.protocol, input),
                (WireProtocol::Chat, Input::Chat(_))
                    | (
                        WireProtocol::Responses,
                        Input::Responses(_) | Input::Compact(_)
                    )
                    | (WireProtocol::Anthropic, Input::Messages(_))
            );
            if let Err(reason) = translate::media::check(req, wire, native) {
                return Some(route_plan::Block::new(reason, None));
            }
            if claude_continuation && !crate::claude_cli::owns(manifest) {
                return Some(route_plan::Block::new("claude-continuation-source", None));
            }
            if (compact && manifest.provider.protocol != WireProtocol::Responses)
                || !input.allows(manifest)
            {
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
    if plan.ordered.is_empty() {
        if matches!(input, Input::Gemini(_)) && plan.has_block("native-protocol-required") {
            log.skipped = std::mem::take(&mut plan.skipped);
            log.finish("native-protocol-required");
            return crate::gemini::error_response(
                StatusCode::BAD_REQUEST,
                "The configured upstreams cannot represent these Gemini generation options",
            );
        }
        if request_media
            && (plan.has_block("native-protocol-required") || plan.has_block("tools-unsupported"))
        {
            log.skipped = std::mem::take(&mut plan.skipped);
            log.finish(translate::media::UNSUPPORTED);
            return ClientError::Attachment(translate::media::UNSUPPORTED).response(input);
        }
        let error = [
            ("model-not-listed", ClientError::ModelNotListed),
            ("output-limit-exceeded", ClientError::OutputLimit),
            ("reasoning-unsupported", ClientError::ReasoningUnsupported),
            (
                translate::media::UNSUPPORTED,
                ClientError::Attachment(translate::media::UNSUPPORTED),
            ),
            (
                translate::media::IMAGE_UNSUPPORTED,
                ClientError::Attachment(translate::media::IMAGE_UNSUPPORTED),
            ),
        ]
        .into_iter()
        .find(|(reason, _)| plan.has_block(reason));
        if let Some((reason, error)) = error {
            log.skipped = std::mem::take(&mut plan.skipped);
            log.finish(reason);
            return error.response(input);
        }
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
        let permit = if crate::claude_cli::owns(&candidate.manifest) {
            None
        } else if let Some(slot) = state.subscription_slot(id, candidate.manifest.kind) {
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
                DispatchIntent::Inference {
                    pin_login: compact || history_owner.is_some(),
                },
                |connection| {
                    if Instant::now() >= deadline {
                        return Err("first-content-timeout");
                    }
                    let subject = connection
                        .login
                        .as_ref()
                        .map(LoginSnapshot::compaction_subject)
                        .unwrap_or_else(|| connection.quota_subject.clone());
                    let owner = crate::compaction::owner(candidate, &subject);
                    if history_owner
                        .as_ref()
                        .is_some_and(|expected| expected != &owner)
                    {
                        return Err("checkpoint-owner-changed");
                    }
                    let operation = if matches!(input, Input::Compact(_)) {
                        "unary"
                    } else {
                        "v2"
                    };
                    let key = format!("{owner}:{}:{operation}", candidate.model);
                    if compact && !state.compact_ready(&key, unix_now()) {
                        return Err("compaction-backoff");
                    }
                    log.compaction_owner = Some(owner);
                    log.compaction_key = compact.then_some(key);
                    log.start(candidate, connection);
                    Ok(())
                },
            ),
        )
        .await
        .unwrap_or(Err("first-content-timeout"));
        attempts += usize::from(log.active.is_some());
        let mut response = match response {
            Ok(sent) => sent,
            Err(reason) if log.active.is_none() => {
                log.skip(candidate, route_plan::Block::new(reason, None));
                continue;
            }
            Err(reason)
                if reason.starts_with("claude-continuation-")
                    || reason == "claude-login-changed" =>
            {
                log.end_attempt(reason, false);
                log.finish(reason);
                return ClientError::InvalidRequest.response(input);
            }
            Err("provider-busy") => {
                log.end_attempt("provider-busy", false);
                continue;
            }
            Err(reason) if compact => {
                log.end_attempt(reason, false);
                continue;
            }
            Err(reason) => {
                log.note_breaker_failure();
                log.end_attempt(reason, false);
                continue;
            }
        };
        if let Some(login) = response.extensions_mut().remove::<LoginSnapshot>() {
            log.compaction_owner = Some(crate::compaction::owner(
                candidate,
                &login.compaction_subject(),
            ));
            if let Some(connection) = &mut log.active_mut().connection {
                connection.quota_subject = login.quota_subject();
                connection.login = Some(login);
            }
        }
        let status = response.status();
        log.active_mut().status = status.as_u16();
        if let Some(connection) = &mut log.active_mut().connection {
            connection.stamp = observation_stamp();
        }
        if compact && status != StatusCode::OK {
            log.active_mut().headers = Some(response.headers().clone());
            if matches!(status.as_u16(), 400 | 404 | 405 | 422) {
                log.end_attempt("compaction-unsupported", false);
            } else {
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
            }
            continue;
        }
        if compact {
            log.active_mut().headers = Some(response.headers().clone());
            let result = tokio::time::timeout_at(
                response_deadline,
                native_compaction(response, input, &mut log),
            )
            .await;
            match result {
                Ok(Ok((bytes, stream))) => {
                    log.content_ready();
                    log.finish("ok");
                    return Response::builder()
                        .header(
                            "content-type",
                            if stream {
                                "text/event-stream"
                            } else {
                                "application/json"
                            },
                        )
                        .header("x-aubridge-provider", id)
                        .body(Body::from(bytes))
                        .expect("compact response");
                }
                _ => {
                    log.end_attempt("compaction-invalid-response", false);
                    continue;
                }
            }
        }
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
                    Input::Gemini(_) => match crate::gemini::completion(
                        &message,
                        &log.active_mut().usage,
                        protocol,
                        &candidate.model,
                    ) {
                        Ok(message) => message,
                        Err(reason) => {
                            log.finish("stream-error");
                            return crate::gemini::error_response(StatusCode::BAD_GATEWAY, reason);
                        }
                    },
                    Input::Messages(_) | Input::Responses(_) | Input::Compact(_) => message,
                };
                reply.cli_delivered();
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

    if history_owner.is_some() {
        log.finish("checkpoint-owner-unavailable");
        return crate::compaction::error(StatusCode::CONFLICT, "Checkpoint owner is unavailable or the selected account changed; restore the original source and login");
    }
    if compact
        && log.skipped.iter().any(|entry| {
            plan.ordered
                .iter()
                .any(|candidate| entry.matches(candidate, "compaction-backoff"))
        })
    {
        log.finish("compaction-backoff");
        return crate::compaction::error(
            StatusCode::TOO_MANY_REQUESTS,
            "Compaction is backing off after an upstream failure; retry later",
        );
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
    if matches!(input, Input::Gemini(_)) {
        return crate::gemini::error_response(
            StatusCode::BAD_GATEWAY,
            "No compatible upstream succeeded",
        );
    }
    if matches!(
        input,
        Input::Chat(_) | Input::Responses(_) | Input::Compact(_)
    ) {
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
    let validation =
        translate::media::check(req, translate::media::Wire::Messages, true).and_then(|_| {
            translate::media::validate_urls(req, |url| {
                reqwest::Url::parse(url).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                })
            })
        });
    if let Err(reason) = validation {
        return ClientError::Attachment(reason).response(Input::Messages(body));
    }
    let plan = route_plan::plan(
        state,
        req,
        headers,
        unix_now(),
        false,
        false,
        |manifest, _| {
            if translate::media::has_media(req) && crate::claude_cli::owns(manifest) {
                return Some(route_plan::Block::new(translate::media::UNSUPPORTED, None));
            }
            let wire = match manifest.provider.protocol {
                WireProtocol::Anthropic => translate::media::Wire::Messages,
                WireProtocol::Chat => translate::media::Wire::Chat,
                WireProtocol::Responses => translate::media::Wire::Responses,
            };
            translate::media::check(req, wire, wire == translate::media::Wire::Messages)
                .err()
                .map(|reason| route_plan::Block::new(reason, None))
        },
    );
    let candidate = plan.ordered.first();
    if let Some(candidate) = candidate.filter(|c| {
        c.manifest.provider.protocol == WireProtocol::Anthropic
            && !crate::claude_cli::owns(&c.manifest)
    }) {
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
    let protocol = candidate.map(|c| c.manifest.provider.protocol);
    let media_body = if translate::media::has_media(&input) {
        match protocol {
            Some(WireProtocol::Chat) => serde_json::to_value(messages_request_to_chat(&input))
                .ok()
                .and_then(|body| {
                    translate::media::encode(&input, translate::media::Wire::Chat, body).ok()
                }),
            Some(WireProtocol::Responses) => {
                serde_json::to_value(messages_request_to_responses(&input))
                    .ok()
                    .and_then(|body| {
                        translate::media::encode(&input, translate::media::Wire::Responses, body)
                            .ok()
                    })
            }
            _ => None,
        }
    } else {
        None
    };
    let estimated = if let Some(body) = media_body {
        translate::count::estimate_serialized(&body)
    } else if translate::media::has_media(&input) {
        translate::count::estimate_tokens(&input)
    } else {
        match protocol {
            Some(WireProtocol::Chat) => {
                translate::count::estimate_serialized(&messages_request_to_chat(&input))
            }
            Some(WireProtocol::Responses) => {
                translate::count::estimate_serialized(&messages_request_to_responses(&input))
            }
            _ => translate::count::estimate_tokens(&input),
        }
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
    let _permit = if crate::claude_cli::owns(&manifest) {
        None
    } else if let Some(slot) = state.subscription_slot(id, manifest.kind) {
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
    let mut checked_connection = None;
    let response = tokio::time::timeout_at(
        deadline,
        send(
            state,
            &candidate,
            &request,
            &HeaderMap::new(),
            Input::Messages(&body),
            DispatchIntent::SourceCheck,
            |connection| {
                checked_connection = Some(connection);
                Ok(())
            },
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
    if let Some(connection) = checked_connection {
        state.update_bound_connection(
            id,
            Some(&connection.version),
            connection.login.as_ref(),
            connection.cli_epoch.as_ref(),
            || {
                state.record_source_check(
                    id,
                    &connection.version,
                    &connection.quota_subject,
                    result.status,
                    &candidate.model,
                    unix_now(),
                );
            },
        );
    }
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
    state
        .cost_block(&candidate.manifest, &candidate.model, unix_now())
        .map_or(Some(()), |_| None)?;
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

enum DispatchIntent {
    Inference { pin_login: bool },
    SourceCheck,
}

async fn send(
    state: &GatewayState,
    candidate: &Candidate,
    req: &MessagesRequest,
    headers: &HeaderMap,
    input: Input<'_>,
    intent: DispatchIntent,
    on_dispatch: impl FnOnce(DispatchedConnection) -> Result<(), &'static str>,
) -> Result<reqwest::Response, &'static str> {
    let cli_generation = state.claude_cli.generation();
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
        (WireProtocol::Responses, Input::Compact(compact)) => (
            "/responses/compact",
            serde_json::to_string(&compact.native(&candidate.model)),
        ),
        (_, Input::Compact(_)) => return Err("native-protocol-required"),
        (protocol, Input::Gemini(input)) => {
            let wire = gemini_wire(protocol);
            let value = match protocol {
                WireProtocol::Chat => serde_json::to_value(messages_request_to_chat(&request)),
                WireProtocol::Responses => {
                    serde_json::to_value(messages_request_to_responses(&request))
                }
                WireProtocol::Anthropic => serde_json::to_value(request.as_ref()),
            }
            .map_err(|_| "encode-error")?;
            let mut value = input.apply_target_options(
                wire,
                value,
                crate::gemini::supports_chat_top_k(&candidate.manifest),
            )?;
            if protocol == WireProtocol::Responses && request.max_tokens > 0 {
                value["max_output_tokens"] = serde_json::json!(request.max_tokens);
            }
            if protocol == WireProtocol::Anthropic && request.max_tokens == 0 {
                let output = candidate
                    .manifest
                    .model_facts
                    .get(&candidate.model)
                    .and_then(|facts| facts.output)
                    .unwrap_or(4096)
                    .min(4096);
                value["max_tokens"] = serde_json::json!(output);
            }
            value["stream"] = Value::Bool(true);
            let path = match protocol {
                WireProtocol::Chat => "/chat/completions",
                WireProtocol::Responses => "/responses",
                WireProtocol::Anthropic => "/messages",
            };
            (path, serde_json::to_string(&value))
        }
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
    let native = matches!(
        (candidate.manifest.provider.protocol, input),
        (WireProtocol::Chat, Input::Chat(_))
            | (WireProtocol::Responses, Input::Responses(_))
            | (WireProtocol::Anthropic, Input::Messages(_))
    );
    let body = if !native && translate::media::has_media(&request) {
        let wire = match candidate.manifest.provider.protocol {
            WireProtocol::Chat => translate::media::Wire::Chat,
            WireProtocol::Responses => translate::media::Wire::Responses,
            WireProtocol::Anthropic => translate::media::Wire::Messages,
        };
        let value = serde_json::from_str(&body).map_err(|_| "encode-error")?;
        let value = translate::media::encode(&request, wire, value)?;
        serde_json::to_string(&value).map_err(|_| "encode-error")?
    } else {
        body
    };
    let body = if openrouter::applies(&candidate.manifest) {
        openrouter::protect(&body, &candidate.model)?
    } else {
        body
    };
    if crate::claude_cli::owns(&candidate.manifest) {
        if state.provider_paused(crate::claude_cli::ID) {
            return Err("provider-paused");
        }
        state.refresh_claude_cli().await;
    }
    let (credential, version) = state.dispatch_credential(&candidate.manifest)?;
    let chatgpt = if matches!(credential, Credential::ChatGpt) {
        Some(state.chatgpt_snapshot().await.ok_or("missing-credential")?)
    } else {
        None
    };
    let cli_binding = if matches!(credential, Credential::ClaudeCli) {
        Some(state.claude_cli.binding().ok_or("missing-credential")?)
    } else {
        None
    };
    // Login refresh can await: reject a changed connection and recheck the
    // current cost facts immediately before dispatch. Explicit checks are scoped separately.
    let (_, current_version) = state.dispatch_credential(&candidate.manifest)?;
    if !Arc::ptr_eq(&version, &current_version) {
        return Err(route_plan::PROVIDER_CHANGED);
    }
    if matches!(intent, DispatchIntent::Inference { .. }) {
        if let Some(reason) = state.cost_block(&candidate.manifest, &candidate.model, unix_now()) {
            return Err(reason);
        }
    }
    on_dispatch(DispatchedConnection {
        version,
        uses_key: matches!(credential, Credential::Key(_)),
        quota_subject: match &credential {
            Credential::Key(key) => crate::state::quota::api_subject(&candidate.manifest, key),
            Credential::ChatGpt => chatgpt
                .as_ref()
                .map(LoginSnapshot::quota_subject)
                .unwrap_or_default(),
            Credential::ClaudeCli => cli_binding
                .as_ref()
                .map(|(subject, _)| subject.clone())
                .unwrap_or_default(),
            Credential::Local => crate::state::quota::local_subject(&candidate.manifest),
        },
        cli_epoch: cli_binding.map(|(_, epoch)| epoch),
        login: chatgpt.clone(),
        stamp: observation_stamp(),
    })?;
    if matches!(credential, Credential::ClaudeCli) {
        let slot = state
            .subscription_slot(&candidate.manifest.id, candidate.manifest.kind)
            .ok_or("provider-busy")?;
        return state
            .claude_cli
            .send(&request, headers, slot, cli_generation, || {
                state.is_serving() && !state.provider_paused(&candidate.manifest.id)
            })
            .await;
    }
    if let Some(tokens) = chatgpt {
        send_chatgpt(
            state,
            &upstream_url(&candidate.manifest.provider.base, path),
            body,
            tokens,
            matches!(intent, DispatchIntent::Inference { pin_login: true }),
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

async fn native_compaction(
    response: reqwest::Response,
    input: Input<'_>,
    log: &mut RequestLog,
) -> Result<(Vec<u8>, bool), &'static str> {
    let owner = log.compaction_owner.clone().ok_or("missing-owner")?;
    if matches!(input, Input::Compact(_)) {
        let bytes = stream::read_limited(response, 16 * 1024 * 1024)
            .await
            .map_err(|_| "invalid-json")?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid-json")?;
        if !value["error"].is_null() {
            return Err("upstream-compact-error");
        }
        let items = value["output"].as_array().ok_or("missing-output")?;
        let checkpoints: Vec<_> = items
            .iter()
            .filter(|item| crate::compaction::is_checkpoint(item))
            .collect();
        if checkpoints.len() != 1 {
            return Err("invalid-checkpoint-count");
        }
        log.active_mut().usage.observe(
            manifest::schema::WireProtocol::Responses,
            &serde_json::json!({"response":&value}),
        );
        log.state
            .save_checkpoint(checkpoints[0], &owner, unix_now())?;
        return Ok((bytes.to_vec(), false));
    }
    let mut reply = Reply::new(response, WireProtocol::Responses, "", None).for_responses();
    let mut output = Vec::new();
    let mut checkpoint = None;
    let mut completed = false;
    let mut terminal_checkpoints = Vec::new();
    while let Some(frame) = reply.next(&mut log.active_mut().usage).await {
        if output.len() + frame.bytes.len() > 16 * 1024 * 1024 {
            return Err("response-too-large");
        }
        if frame.event["type"] == "response.output_item.done"
            && crate::compaction::is_v2_checkpoint(&frame.event["item"])
        {
            if checkpoint.is_some() {
                return Err("multiple-checkpoints");
            }
            checkpoint = Some(frame.event["item"].clone());
        }
        if frame.event["type"] == "response.completed" {
            completed = true;
            if let Some(items) = frame.event["response"]["output"].as_array() {
                terminal_checkpoints.extend(
                    items
                        .iter()
                        .filter(|item| crate::compaction::is_checkpoint(item))
                        .cloned(),
                );
            }
        }
        output.extend_from_slice(&frame.bytes);
    }
    if reply.failed || !completed {
        return Err("incomplete-compaction");
    }
    let checkpoint = checkpoint.ok_or("missing-checkpoint")?;
    if terminal_checkpoints
        .iter()
        .any(|item| item["encrypted_content"] != checkpoint["encrypted_content"])
    {
        return Err("conflicting-terminal-checkpoint");
    }
    log.state.save_checkpoint(&checkpoint, &owner, unix_now())?;
    Ok((output, true))
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

fn upstream_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path == "/responses/compact" && base.ends_with("/responses") {
        format!("{base}/compact")
    } else {
        format!("{base}{path}")
    }
}

fn build_post(
    state: &GatewayState,
    candidate: &Candidate,
    credential: &Credential,
    headers: &HeaderMap,
    path: &str,
) -> reqwest::RequestBuilder {
    let mut request = state
        .http()
        .post(upstream_url(&candidate.manifest.provider.base, path))
        .header("content-type", "application/json");
    if let Credential::Key(key) = credential {
        request = match candidate.manifest.provider.auth {
            AuthKind::ClaudeCli => request,
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
    pin_login: bool,
) -> Result<reqwest::Response, &'static str> {
    let response = crate::chatgpt::responses_builder(state.http(), url, &tokens)
        .body(body.clone())
        .send()
        .await
        .map_err(|_| "network-error")?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok(response);
    }
    let expected_subject = tokens.compaction_subject();
    let Some(fresh) = state.chatgpt_refresh_snapshot(tokens).await else {
        return Ok(response);
    };
    if pin_login && fresh.compaction_subject() != expected_subject {
        return Ok(response);
    }
    let mut response = crate::chatgpt::responses_builder(state.http(), url, &fresh)
        .body(body)
        .send()
        .await
        .map_err(|_| "network-error")?;
    response.extensions_mut().insert(fresh);
    Ok(response)
}

enum Projection {
    Gemini(translate::gemini_output::GeminiEncoder, WireProtocol),
    Chat(chat::Encoder),
    Responses(crate::responses::Encoder),
}

impl Projection {
    fn for_input(input: Input<'_>, model: &str, now: u64, from: WireProtocol) -> Option<Self> {
        match input {
            Input::Gemini(_) => Some(Self::Gemini(
                translate::gemini_output::GeminiEncoder::new(model),
                from,
            )),
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
            Self::Gemini(encoder, from) => {
                if event.is_null() {
                    return None;
                }
                if let Some(model) = usage.model.as_deref() {
                    encoder.set_model(model);
                }
                encoder.set_usage(crate::gemini::usage(usage, *from));
                let frames = encoder.push_raw(event);
                if frames.is_empty() {
                    None
                } else {
                    Some(Bytes::from(
                        frames
                            .iter()
                            .map(translate::gemini_output::frame)
                            .collect::<String>(),
                    ))
                }
            }
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
            loop {
                if reply.ended && held.is_empty() {
                    return None;
                }
                if matches!(&encoder, Some(Projection::Gemini(..)))
                    && Instant::now() >= log.started + RESPONSE_BUDGET
                {
                    let error = crate::gemini::error(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Gateway response deadline exceeded",
                    );
                    reply.abort();
                    held.clear();
                    log.finish("stream-error");
                    return Some((
                        Ok(Bytes::from(translate::gemini_output::frame(&error))),
                        (held, reply, log, encoder),
                    ));
                }
                let mut idle_deadline = Instant::now() + SSE_KEEPALIVE;
                if matches!(&encoder, Some(Projection::Gemini(..))) {
                    idle_deadline = idle_deadline.min(log.started + RESPONSE_BUDGET);
                }
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
                            if matches!(&encoder, Some(Projection::Gemini(..))) {
                                continue;
                            }
                            let comment = Bytes::from(proto::sse::frame_keepalive());
                            return Some((Ok(comment), (held, reply, log, encoder)));
                        }
                    },
                };
                let mut checkpoints = Vec::new();
                if matches!(
                    frame.event["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                ) && crate::compaction::is_checkpoint(&frame.event["item"])
                    && (frame.event["type"] == "response.output_item.done"
                        || !frame.event["item"]["encrypted_content"].is_null())
                {
                    checkpoints.push(&frame.event["item"]);
                }
                if frame.event["type"] == "response.completed" {
                    if let Some(items) = frame.event["response"]["output"].as_array() {
                        checkpoints.extend(
                            items
                                .iter()
                                .filter(|item| crate::compaction::is_checkpoint(item)),
                        );
                    }
                }
                for checkpoint in checkpoints {
                    let saved = log
                        .compaction_owner
                        .as_deref()
                        .ok_or("Checkpoint owner missing")
                        .and_then(|owner| log.state.save_checkpoint(checkpoint, owner, unix_now()));
                    if saved.is_err() {
                        reply.ended = true;
                        reply.failed = true;
                        held.clear();
                        log.finish("checkpoint-store-error");
                        return Some((
                            Ok(Bytes::from(crate::responses::stream_failure(
                                "resp_failed",
                                0,
                                false,
                            ))),
                            (held, reply, log, encoder),
                        ));
                    }
                }
                let bytes = match encoder.as_mut() {
                    Some(encoder) => encoder.encode(&frame.event, &log.active_mut().usage),
                    None => Some(frame.bytes),
                };
                if matches!(&encoder, Some(Projection::Responses(encoder)) if encoder.failed)
                    || matches!(&encoder,Some(Projection::Gemini(encoder,_)) if encoder.failed())
                {
                    reply.abort();
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
                    if outcome == "ok" {
                        reply.cli_delivered();
                    }
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
    quota_subject: String,
    login: Option<LoginSnapshot>,
    cli_epoch: Option<Arc<()>>,
    stamp: ObservationStamp,
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
    price_snapshot: Option<ledger::PriceSnapshot>,
    benchmark_snapshot: Option<ledger::PriceSnapshot>,
}

impl Attempt {
    fn login(&self) -> Option<&LoginSnapshot> {
        self.connection
            .as_ref()
            .and_then(|connection| connection.login.as_ref())
    }
    fn cli_epoch(&self) -> Option<&Arc<()>> {
        self.connection
            .as_ref()
            .and_then(|connection| connection.cli_epoch.as_ref())
    }
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
    compaction_owner: Option<String>,
    compaction_key: Option<String>,
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
        let identity = crate::session_identity::extract(headers, Some(req));
        Self {
            started,
            state,
            affinity: route_plan::session_key(req, headers),
            decision: ledger::DecisionRecord {
                session_id: identity.session,
                parent_session_id: identity.parent,
                purpose: identity.purpose,
                ts: now,
                model: req.model.clone(),
                agent,
                request_id: headers
                    .get(crate::diagnostics::REQUEST_ID)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                request_path: headers
                    .get(crate::diagnostics::REQUEST_PATH)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                request_bytes: headers
                    .get(crate::diagnostics::REQUEST_BYTES)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok()),
                ..Default::default()
            },
            attempts: Vec::new(),
            skipped: Vec::new(),
            usage: Vec::new(),
            active: None,
            context_failure: None,
            finished: false,
            compaction_owner: None,
            compaction_key: None,
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
            benchmark_snapshot: self
                .state
                .benchmark_snapshot(&candidate.manifest.id, &candidate.model),
            price_snapshot: self
                .state
                .served_price_snapshot(&candidate.manifest, &candidate.model),
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
        self.state.update_bound_connection(
            &attempt.id,
            attempt.connection_version(),
            attempt.login(),
            attempt.cli_epoch(),
            || {
                self.state.apply_breaker_failure(&attempt.id, unix_now());
            },
        );
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
        self.state.update_bound_connection(
            id,
            attempt.connection_version(),
            attempt.login(),
            attempt.cli_epoch(),
            || {
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
            },
        );
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
        if let Some(key) = self.compaction_key.take() {
            if outcome != "cancelled" {
                self.state.compact_result(&key, outcome == "ok", unix_now());
            }
        }

        let Some(mut attempt) = self.active.take() else {
            return;
        };
        let now = unix_now();
        let model = attempt
            .usage
            .model
            .take()
            .unwrap_or_else(|| attempt.model.clone());
        let cache_read = attempt
            .usage
            .cache_read_reported
            .then_some(attempt.usage.cache_read);
        let cache_write = attempt
            .usage
            .cache_write_reported
            .then_some(attempt.usage.cache_write);
        let price = attempt
            .price_snapshot
            .as_ref()
            .filter(|price| price.model == model)
            .cloned();
        let benchmark = attempt
            .benchmark_snapshot
            .as_ref()
            .filter(|price| price.model == model && price.provider == attempt.id)
            .cloned();
        let upstream_request_id = attempt
            .headers
            .as_ref()
            .and_then(|headers| {
                headers
                    .get("x-request-id")
                    .or_else(|| headers.get("request-id"))
            })
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                value.len() <= 128
                    && !value.is_empty()
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
                    })
            });
        let mut entry = serde_json::json!({
            "id":attempt.id, "status":attempt.status, "model":model,
            "requested_model":attempt.model, "kind":attempt.kind, "chosen":chosen,
            "duration_ms":attempt.started.elapsed().as_millis() as u64,
            "cache_read_tokens":cache_read,"cache_write_tokens":cache_write,"upstream_request_id":upstream_request_id,
            "price_snapshot":price,"benchmark_snapshot":benchmark,
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
            self.decision.cache_read_tokens = cache_read;
            self.decision.cache_write_tokens = cache_write;
            self.decision.input_includes_cache = Some(attempt.protocol != WireProtocol::Anthropic);
            self.decision.price_snapshot = price.clone();
            self.decision.benchmark_snapshot = benchmark.clone();
        }
        self.state.update_bound_connection(
            &attempt.id,
            attempt.connection_version(),
            attempt.login(),
            attempt.cli_epoch(),
            || {
                if outcome == "ok" {
                    self.state.apply_success(&attempt.id, now);
                    if let Some(connection) = &attempt.connection {
                        self.state.record_source_success(
                            &attempt.id,
                            &connection.version,
                            &connection.quota_subject,
                            now,
                        );
                    }
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
                if let (Some(headers), Some(connection)) = (&attempt.headers, &attempt.connection) {
                    observe_quota(
                        &self.state,
                        &attempt.id,
                        attempt.protocol,
                        attempt.codex,
                        headers,
                        connection.stamp,
                        &connection.quota_subject,
                    );
                }
                if let Some(failure) = &attempt.stream_limit {
                    // A window sample must not replace an explicit terminal limit.
                    failure.apply_health(&self.state, &attempt.id, &attempt.model, now);
                }
            },
        );
        if attempt.usage.input.is_some()
            || attempt.usage.output.is_some()
            || cache_read.is_some()
            || cache_write.is_some()
        {
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
                request_id: self.decision.request_id.clone(),
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                input_includes_cache: Some(attempt.protocol != WireProtocol::Anthropic),
                price_snapshot: price,
                benchmark_snapshot: benchmark,
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
    Attachment(&'static str),
    InvalidRequest,
    ModelNotFound,
    ModelNotListed,
    OutputLimit,
    ReasoningUnsupported,
    FreeRouteRestricted,
    ContextLength,
    RequestTooLarge,
    RateLimit(u64),
    Timeout,
}

impl ClientError {
    fn code(self) -> &'static str {
        match self {
            Self::Attachment(reason) => reason,
            Self::InvalidRequest => "invalid_request",
            Self::ModelNotFound => "model_not_found",
            Self::ModelNotListed => "model_not_listed",
            Self::OutputLimit => "output_limit_exceeded",
            Self::ReasoningUnsupported => "reasoning_unsupported",
            Self::FreeRouteRestricted => openrouter::RESTRICTED,
            Self::ContextLength => "context_length_exceeded",
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimit(_) => "rate_limit_exceeded",
            Self::Timeout => "response_timeout",
        }
    }

    fn response(self, input: Input<'_>) -> Response<Body> {
        let (status, kind, message) = match self {
            Self::Attachment(reason) => (StatusCode::BAD_REQUEST, "invalid_request_error", reason),
            Self::ModelNotListed => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model-not-listed: vendor directory does not currently list the configured model",
            ),
            Self::OutputLimit => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Requested output exceeds the model's published output limit",
            ),
            Self::ReasoningUnsupported => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "The model does not support the requested reasoning mode",
            ),
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
            Input::Gemini(_) => crate::gemini::error_response(status, message),
            Input::Chat(_) | Input::Responses(_) | Input::Compact(_) => {
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
    stamp: ObservationStamp,
    subject: &str,
) {
    let now = stamp.at;
    if codex {
        let sample = sample_codex_windows(headers, now);
        state.commit_passive_quota(id, subject, codex_observations(headers, stamp, &sample));
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
        let (short, week, _) = sample_anthropic_utilization(headers);
        let windows = [("5h", 18_000, short), ("7d", 604_800, week)]
            .into_iter()
            .filter_map(|(window, span, used)| {
                let used = used?;
                let reset = header_u64(
                    headers,
                    &format!("anthropic-ratelimit-unified-{window}-reset"),
                );
                QuotaWindow::new(
                    span,
                    used,
                    reset,
                    stamp.at,
                    stamp.sequence,
                    ObservationSource::ResponseHeaders,
                )
            })
            .collect();
        state.commit_passive_quota(id, subject, windows);
    }
}

fn codex_observations(
    headers: &HeaderMap,
    stamp: ObservationStamp,
    sample: &CodexSample,
) -> Vec<QuotaWindow> {
    [
        ("primary", 604_800, sample.percent_7d),
        ("secondary", 18_000, sample.percent_5h),
    ]
    .into_iter()
    .filter_map(|(window, fallback, used)| {
        let used = used?;
        let span = header_u64(headers, &format!("x-codex-{window}-window-minutes"))
            .and_then(|minutes| minutes.checked_mul(60))
            .unwrap_or(fallback);
        let reset = header_u64(headers, &format!("x-codex-{window}-reset-after-seconds"))
            .map(|seconds| stamp.at.saturating_add(seconds));
        QuotaWindow::new(
            span,
            used,
            reset,
            stamp.at,
            stamp.sequence,
            ObservationSource::ResponseHeaders,
        )
    })
    .collect()
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
        } else if matches!(input, Input::Gemini(_)) {
            match crate::gemini::completion(
                &message,
                &local_usage(),
                WireProtocol::Anthropic,
                model,
            ) {
                Ok(value) => value,
                Err(reason) => {
                    return crate::gemini::error_response(StatusCode::BAD_GATEWAY, reason)
                }
            }
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

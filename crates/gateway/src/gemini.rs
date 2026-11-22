//! Google ingress uses the existing production dispatcher, without local HTTP forwarding.
use crate::{security, state::GatewayState, stream::ReportedUsage};
use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use manifest::schema::WireProtocol;
use serde_json::{json, Value};
use std::sync::Arc;

// Google topK is not a standard Chat parameter. Only the documented official
// OpenRouter target can receive it; a provider id alone proves no capability.
pub(crate) fn supports_chat_top_k(manifest: &manifest::schema::Manifest) -> bool {
    manifest.provider.protocol == WireProtocol::Chat
        && manifest.provider.base.trim_end_matches('/') == "https://openrouter.ai/api/v1"
}

pub(crate) fn error(status: StatusCode, message: &str) -> Value {
    let name = match status.as_u16() {
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        500 => "INTERNAL",
        502 | 503 => "UNAVAILABLE",
        408 | 504 => "DEADLINE_EXCEEDED",
        _ => "INVALID_ARGUMENT",
    };
    json!({"error":{"code":status.as_u16(),"message":message,"status":name}})
}
pub(crate) fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    (status, axum::Json(error(status, message))).into_response()
}
pub(crate) fn usage(usage: &ReportedUsage, from: WireProtocol) -> Value {
    let mut value = json!({});
    let input = usage.input_with_cache(from);
    if let Some(n) = input {
        value["promptTokenCount"] = json!(n);
    }
    if let Some(n) = usage.output {
        value["candidatesTokenCount"] = json!(n);
    }
    if let (Some(i), Some(o)) = (input, usage.output) {
        value["totalTokenCount"] = json!(i.saturating_add(o));
    }
    if usage.cache_read_reported {
        value["cachedContentTokenCount"] = json!(usage.cache_read);
    }
    value
}
pub(crate) fn completion(
    message: &Value,
    usage_report: &ReportedUsage,
    from: WireProtocol,
    model: &str,
) -> Result<Value, &'static str> {
    let mut encoder = translate::gemini_output::GeminiEncoder::new(
        usage_report.model.as_deref().unwrap_or(model),
    );
    if let Some(model) = usage_report.model.as_deref() {
        encoder.set_model(model);
    }
    encoder.set_usage(usage(usage_report, from));
    // Reuse the encoder's canonical event path for buffered production responses.
    encoder.push_raw(&json!({"type":"message_start","message":message}));
    if let Some(blocks) = message["content"].as_array() {
        for (index, block) in blocks.iter().enumerate() {
            encoder.push_raw(
                &json!({"type":"content_block_start","index":index,"content_block":block}),
            );
            encoder.push_raw(&json!({"type":"content_block_stop","index":index}));
        }
    }
    encoder
        .push_raw(&json!({"type":"message_delta","delta":{"stop_reason":message["stop_reason"]}}));
    encoder.push_raw(&json!({"type":"message_stop"}));
    encoder.response()
}
pub(crate) fn models(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "Missing or invalid gateway key");
    }
    let models = state
        .models()
        .iter()
        .map(|id| {
            json!({"name":format!("models/{id}"),"displayName":id,
        "supportedGenerationMethods":["generateContent","streamGenerateContent","countTokens"]})
        })
        .collect::<Vec<_>>();
    axum::Json(json!({"models":models})).into_response()
}
pub(crate) async fn route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    path: String,
    body: String,
) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "Missing or invalid gateway key");
    }
    if !state.is_serving() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "Gateway is paused");
    }
    let Some((model, operation)) = path.rsplit_once(':') else {
        return error_response(StatusCode::NOT_FOUND, "Unknown model action");
    };
    if !matches!(
        operation,
        "generateContent" | "streamGenerateContent" | "countTokens"
    ) {
        return error_response(StatusCode::NOT_FOUND, "Unknown model action");
    }
    if !state.models().iter().any(|id| id == model) {
        return error_response(StatusCode::NOT_FOUND, "Model is not configured");
    }
    let input = match translate::gemini_input::GeminiInput::parse(model, operation, &body) {
        Ok(input) => input,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    if operation == "countTokens" {
        return crate::upstream::count_gemini(&state, &input, &headers);
    }
    crate::upstream::forward_gemini(state, &input, &headers).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::schema::Manifest;
    fn source(id: &str, base: &str, protocol: &str) -> Manifest {
        serde_json::from_value(
            json!({"id":id,"version":"1","kind":"free_tier","free_models":["m"],
            "provider":{"base":base,"auth":"bearer","protocol":protocol}}),
        )
        .unwrap()
    }
    #[test]
    fn chat_top_k_requires_official_openrouter_target_and_preserves_explicit_value() {
        let input=translate::gemini_input::GeminiInput::parse("m","streamGenerateContent",
            r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"temperature":1,"topP":0.95,"topK":64,"thinkingConfig":{"includeThoughts":true}}}"#).unwrap();
        for (id, base, protocol, expected) in [
            ("openrouter", "https://openrouter.ai/api/v1", "chat", true),
            ("custom", "https://openrouter.ai/api/v1/", "chat", true),
            ("openrouter", "http://openrouter.ai/api/v1", "chat", false),
            (
                "openrouter",
                "https://example.invalid/api/v1",
                "chat",
                false,
            ),
            (
                "openrouter",
                "https://openrouter.ai/api/v1/else",
                "chat",
                false,
            ),
            (
                "openrouter",
                "https://openrouter.ai/api/v1",
                "responses",
                false,
            ),
            (
                "openrouter",
                "https://openrouter.ai:443/api/v1",
                "chat",
                false,
            ),
        ] {
            let manifest = source(id, base, protocol);
            assert_eq!(
                supports_chat_top_k(&manifest),
                expected,
                "{base}/{protocol}"
            );
            let wire = if protocol == "responses" {
                translate::media::Wire::Responses
            } else {
                translate::media::Wire::Chat
            };
            assert_eq!(
                input.supports_target(wire, supports_chat_top_k(&manifest)),
                expected,
                "{base}"
            );
            let encoded = input.apply_target_options(
                wire,
                json!({"model":"m"}),
                supports_chat_top_k(&manifest),
            );
            assert_eq!(encoded.is_ok(), expected, "{base}");
            if let Ok(encoded) = encoded {
                assert_eq!(encoded["top_k"], 64);
                assert_eq!(encoded["temperature"], 1);
                assert_eq!(encoded["top_p"], 0.95);
                assert!(encoded.get("thinking").is_none());
                assert!(encoded.get("reasoning").is_none());
            }
        }
        let encoded = input
            .apply_target_options(translate::media::Wire::Messages, json!({}), false)
            .unwrap();
        assert_eq!(encoded["top_k"], 64);
        assert!(encoded.get("thinking").is_none());
    }
}

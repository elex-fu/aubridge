//! Cost restrictions for the built-in free source. Client route hints cannot
//! replace the model or the price ceiling selected by the gateway.

use manifest::schema::{Manifest, WireProtocol};
use serde_json::{json, Value};

pub(crate) const RESTRICTED: &str = "free-route-restricted";
const DISABLED_PLUGINS: &[&str] = &[
    "web",
    "file-parser",
    "response-healing",
    "pareto-router",
    "fusion",
];

pub(crate) fn applies(manifest: &Manifest) -> bool {
    manifest.id == "openrouter" && manifest.provider.protocol == WireProtocol::Chat
}

pub(crate) fn supports(body: &Value) -> bool {
    let tools = &body["tools"];
    let choice = &body["tool_choice"];
    let plugins = &body["plugins"];
    let modalities = &body["modalities"];
    (tools.is_null()
        || tools
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| tool["type"] == "function")))
        && (choice.is_null()
            || matches!(choice.as_str(), Some("auto" | "none" | "required"))
            || choice["type"] == "function")
        && (plugins.is_null()
            || plugins.as_array().is_some_and(|plugins| {
                plugins
                    .iter()
                    .all(|plugin| plugin["enabled"] == false && plugin["id"].is_string())
            }))
        && (modalities.is_null()
            || modalities
                .as_array()
                .is_some_and(|modalities| modalities.iter().all(|mode| mode == "text")))
        && (body["provider"].is_null() || body["provider"].is_object())
        && ["web_search_options", "audio", "image_config"]
            .iter()
            .all(|field| body[*field].is_null())
        && body["messages"].as_array().is_some_and(|messages| {
            messages.iter().all(|message| {
                message["content"].as_array().is_none_or(|parts| {
                    parts.iter().all(|part| {
                        matches!(
                            part["type"].as_str(),
                            Some("text" | "image_url" | "refusal")
                        )
                    })
                })
            })
        })
}

pub(crate) fn protect(body: &str, model: &str) -> Result<String, &'static str> {
    let mut body: Value = serde_json::from_str(body).map_err(|_| "encode-error")?;
    if !crate::catalog::free_id(model) || !supports(&body) {
        return Err(RESTRICTED);
    }
    // OpenRouter can apply account defaults even when a client omits plugins.
    // Forward only explicit opt-outs and disable known plugins for this route.
    let disabled = DISABLED_PLUGINS
        .iter()
        .copied()
        .chain(
            body["plugins"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|plugin| plugin["id"].as_str()),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|id| json!({"id":id,"enabled":false}))
        .collect();
    let object = body.as_object_mut().ok_or("encode-error")?;
    object.retain(|field, _| {
        matches!(
            field.as_str(),
            "model"
                | "messages"
                | "stream"
                | "stream_options"
                | "provider"
                | "tools"
                | "tool_choice"
                | "functions"
                | "function_call"
                | "parallel_tool_calls"
                | "max_tokens"
                | "max_completion_tokens"
                | "temperature"
                | "top_p"
                | "top_k"
                | "min_p"
                | "top_a"
                | "frequency_penalty"
                | "presence_penalty"
                | "repetition_penalty"
                | "seed"
                | "logit_bias"
                | "logprobs"
                | "top_logprobs"
                | "n"
                | "stop"
                | "response_format"
                | "reasoning"
                | "reasoning_effort"
                | "include_reasoning"
        )
    });
    body["model"] = Value::String(model.into());
    body["provider"]["max_price"] = json!({"prompt":0,"completion":0,"request":0,"image":0});
    body["plugins"] = Value::Array(disabled);
    serde_json::to_string(&body).map_err(|_| "encode-error")
}

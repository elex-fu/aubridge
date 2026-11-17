//! OpenAI response projection for non-Chat upstreams.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, Bytes};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use manifest::schema::WireProtocol;
use serde_json::{json, Value};

use crate::stream::ReportedUsage;

pub(crate) const DONE: &str = "data: [DONE]\n\n";

pub(crate) fn error(message: &str, kind: &str, code: Option<&str>) -> Value {
    json!({"error":{"message": message, "type": kind, "param": null, "code": code}})
}

pub(crate) fn error_response(
    status: StatusCode,
    message: &str,
    kind: &str,
    code: Option<&str>,
) -> Response<Body> {
    (status, axum::Json(error(message, kind, code))).into_response()
}

pub(crate) fn stream_error() -> Bytes {
    let error = error(
        "Upstream stream interrupted",
        "server_error",
        Some("upstream_stream_error"),
    );
    Bytes::from(format!("data: {error}\n\n{DONE}"))
}

fn data_frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn completion_id(value: &Value, created: u64) -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = value["id"].as_str().filter(|id| !id.is_empty());
    match id {
        Some(id) if id.starts_with("chatcmpl-") => id.into(),
        Some(id) => format!("chatcmpl-{id}"),
        None => format!(
            "chatcmpl-aubridge-{created}-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ),
    }
}

fn finish_reason(value: &Value) -> Option<&str> {
    value.as_str().map(|reason| match reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        reason => reason,
    })
}

fn usage_json(usage: &ReportedUsage, from: WireProtocol) -> Option<Value> {
    let input = usage.input_with_cache(from)?;
    let output = usage.output?;
    let mut value = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input.saturating_add(output),
    });
    if usage.cache_read > 0 {
        value["prompt_tokens_details"] = json!({"cached_tokens": usage.cache_read});
    }
    Some(value)
}

pub(crate) fn completion(
    message: &Value,
    usage: &ReportedUsage,
    from: WireProtocol,
    fallback_model: &str,
    created: u64,
) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools = Vec::new();
    if let Some(blocks) = message["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                Some("thinking") => {
                    reasoning.push_str(block["thinking"].as_str().unwrap_or_default())
                }
                Some("tool_use") => tools.push(json!({
                    "id": block["id"], "type": "function",
                    "function": {"name": block["name"], "arguments": block["input"].to_string()},
                })),
                _ => {}
            }
        }
    }
    let mut assistant = json!({"role":"assistant", "content": (!text.is_empty()).then_some(text)});
    if !tools.is_empty() {
        assistant["tool_calls"] = json!(tools);
    }
    if !reasoning.is_empty() {
        assistant["reasoning_content"] = json!(reasoning);
    }
    let mut response = json!({
        "id": completion_id(message, created),
        "object": "chat.completion",
        "created": created,
        "model": message["model"].as_str().unwrap_or(fallback_model),
        "choices": [{
            "index":0, "message":assistant, "finish_reason": finish_reason(&message["stop_reason"]),
        }],
    });
    if let Some(usage) = usage_json(usage, from) {
        response["usage"] = usage;
    }
    response
}

struct ToolDelta {
    index: usize,
    sent_arguments: bool,
}

pub(crate) struct Encoder {
    id: String,
    model: String,
    created: u64,
    from: WireProtocol,
    include_usage: bool,
    tools: BTreeMap<u64, ToolDelta>,
}

impl Encoder {
    pub(crate) fn new(model: &str, created: u64, from: WireProtocol, include_usage: bool) -> Self {
        Self {
            id: completion_id(&Value::Null, created),
            model: model.into(),
            created,
            from,
            include_usage,
            tools: BTreeMap::new(),
        }
    }

    pub(crate) fn encode(&mut self, event: &Value, usage: &ReportedUsage) -> Option<Bytes> {
        let block = &event["content_block"];
        let delta = &event["delta"];
        let block_index = event["index"].as_u64().unwrap_or_default();
        let output = match event["type"].as_str()? {
            "message_start" => {
                let message = &event["message"];
                self.id = completion_id(message, self.created);
                if let Some(model) = message["model"].as_str() {
                    self.model = model.into();
                }
                json!({"role": "assistant", "content": ""})
            }
            "content_block_start" => match block["type"].as_str()? {
                "text" => text_delta("content", &block["text"])?,
                "thinking" => text_delta("reasoning_content", &block["thinking"])?,
                "tool_use" => {
                    let index = self.tools.len();
                    let arguments = block["input"]
                        .as_object()
                        .filter(|input| !input.is_empty())
                        .map(|_| block["input"].to_string())
                        .unwrap_or_default();
                    self.tools.insert(
                        block_index,
                        ToolDelta {
                            index,
                            sent_arguments: !arguments.is_empty(),
                        },
                    );
                    json!({"tool_calls":[{
                        "index": index, "type": "function", "id": block["id"],
                        "function":{"name":block["name"], "arguments":arguments},
                    }]})
                }
                _ => return None,
            },
            "content_block_delta" => match delta["type"].as_str()? {
                "text_delta" => text_delta("content", &delta["text"])?,
                "thinking_delta" => text_delta("reasoning_content", &delta["thinking"])?,
                "input_json_delta" => {
                    let tool = self.tools.get_mut(&block_index)?;
                    let arguments = delta["partial_json"].as_str()?;
                    tool.sent_arguments |= !arguments.is_empty();
                    tool_arguments(tool.index, arguments)
                }
                _ => return None,
            },
            "content_block_stop" => {
                let tool = self.tools.get_mut(&block_index)?;
                if tool.sent_arguments {
                    return None;
                }
                tool.sent_arguments = true;
                tool_arguments(tool.index, "{}")
            }
            "message_delta" => {
                return Some(self.chunk(json!({}), Some(finish_reason(&delta["stop_reason"])?)))
            }
            "message_stop" => {
                let usage = self
                    .include_usage
                    .then(|| usage_json(usage, self.from))
                    .flatten();
                return Some(match usage {
                    Some(usage) => {
                        let mut value = self.envelope(json!([]));
                        value["usage"] = usage;
                        Bytes::from(format!("data: {value}\n\n{DONE}"))
                    }
                    None => Bytes::from_static(DONE.as_bytes()),
                });
            }
            "error" => return Some(stream_error()),
            _ => return None,
        };
        Some(self.chunk(output, None))
    }

    fn envelope(&self, choices: Value) -> Value {
        json!({
            "id": self.id, "object": "chat.completion.chunk",
            "created": self.created, "model": self.model, "choices": choices,
        })
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        let mut value = self.envelope(json!([{"index":0, "delta":delta, "finish_reason":finish}]));
        if self.include_usage {
            value["usage"] = Value::Null;
        }
        data_frame(&value)
    }
}

fn text_delta(field: &str, value: &Value) -> Option<Value> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(|text| json!({field: text}))
}

fn tool_arguments(index: usize, arguments: &str) -> Value {
    json!({"tool_calls":[{"index":index, "function":{"arguments":arguments}}]})
}

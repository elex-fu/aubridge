//! Project a Messages-shaped stream onto the stateless Responses protocol.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use manifest::schema::WireProtocol;
use serde_json::{json, Value};
use translate::responses_input::ClientTool;

use crate::stream::ReportedUsage;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

struct Item {
    index: usize,
    value: Value,
    content: String,
}

pub(crate) struct Encoder {
    id: String,
    model: String,
    created: u64,
    from: WireProtocol,
    sequence: u64,
    output: Vec<Value>,
    open: BTreeMap<u64, Item>,
    names: BTreeMap<String, ClientTool>,
    size: usize,
    pub(crate) failed: bool,
    pub(crate) incomplete: Option<&'static str>,
}

impl Encoder {
    pub(crate) fn new(
        model: &str,
        created: u64,
        from: WireProtocol,
        names: BTreeMap<String, ClientTool>,
    ) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            id: format!(
                "resp_aubridge_{created}_{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            model: model.into(),
            created,
            from,
            sequence: 0,
            output: Vec::new(),
            open: BTreeMap::new(),
            names,
            size: 0,
            failed: false,
            incomplete: None,
        }
    }

    pub(crate) fn encode(&mut self, event: &Value, usage: &ReportedUsage) -> Option<Bytes> {
        let mut bytes = String::new();
        let block_index = event["index"].as_u64().unwrap_or_default();
        match event["type"].as_str()? {
            "message_start" => {
                if let Some(model) = event["message"]["model"].as_str() {
                    self.model = model.into();
                }
                let response = self.envelope("in_progress", Value::Null);
                self.emit(&mut bytes, "response.created", json!({"response":response}));
            }
            "content_block_start" => self.start(block_index, &event["content_block"], &mut bytes),
            "content_block_delta" => {
                let delta = &event["delta"];
                let field = match delta["type"].as_str()? {
                    "text_delta" => "text",
                    "input_json_delta" => "partial_json",
                    _ => return None,
                };
                self.delta(block_index, delta[field].as_str()?, &mut bytes);
            }
            "content_block_stop" => self.close(block_index, &mut bytes),
            "message_delta" => {
                self.incomplete = match event["delta"]["stop_reason"].as_str() {
                    Some("max_tokens" | "model_context_window_exceeded") => {
                        Some("max_output_tokens")
                    }
                    Some("refusal") => Some("content_filter"),
                    _ => None,
                };
            }
            "message_stop" if !self.open.is_empty() => self.fail(&mut bytes),
            "message_stop" => {
                let usage = usage_value(usage, self.from);
                let (kind, response) = if let Some(reason) = self.incomplete {
                    let mut response = self.envelope("incomplete", usage);
                    response["incomplete_details"] = json!({"reason": reason});
                    ("response.incomplete", response)
                } else {
                    ("response.completed", self.envelope("completed", usage))
                };
                self.emit(&mut bytes, kind, json!({"response":response}));
            }
            "error" => self.fail(&mut bytes),
            _ => {}
        }
        (!bytes.is_empty()).then(|| Bytes::from(bytes))
    }

    fn start(&mut self, block_index: u64, block: &Value, bytes: &mut String) {
        let index = self.output.len();
        let prefix = self.id.strip_prefix("resp_").unwrap_or(&self.id);
        let value = match block["type"].as_str() {
            Some("text") => json!({
                "id":format!("msg_{prefix}_{index}"), "type":"message",
                "status":"in_progress", "role":"assistant", "content":[],
            }),
            Some("tool_use") => {
                let (Some(alias), Some(call_id)) = (block["name"].as_str(), block["id"].as_str())
                else {
                    self.fail(bytes);
                    return;
                };
                let tool = self.names.get(alias);
                let (kind, id_prefix, field) = if tool.is_some_and(|tool| tool.custom) {
                    ("custom_tool_call", "ctc", "input")
                } else {
                    ("function_call", "fc", "arguments")
                };
                let mut value = json!({
                    "id":format!("{id_prefix}_{prefix}_{index}"), "type":kind,
                    "status":"in_progress", "call_id":call_id,
                    "name":tool.map_or(alias, |tool| tool.name.as_str()), field:"",
                });
                if let Some(namespace) = tool.and_then(|tool| tool.namespace.as_deref()) {
                    value["namespace"] = json!(namespace);
                }
                value
            }
            _ => return,
        };
        if self.open.contains_key(&block_index) {
            self.fail(bytes);
            return;
        }
        self.emit(
            bytes,
            "response.output_item.added",
            json!({"output_index":index,"item":value}),
        );
        let text = value["type"] == "message";
        if text {
            self.emit(bytes, "response.content_part.added", json!({
                "item_id":value["id"], "output_index":index,"content_index":0,"part":text_part(""),
            }));
        }
        self.output.push(Value::Null);
        self.open.insert(
            block_index,
            Item {
                index,
                value,
                content: String::new(),
            },
        );
        if text {
            self.delta(
                block_index,
                block["text"].as_str().unwrap_or_default(),
                bytes,
            );
        } else if block["input"]
            .as_object()
            .is_some_and(|input| !input.is_empty())
        {
            self.delta(block_index, &block["input"].to_string(), bytes);
        }
    }

    fn delta(&mut self, block_index: u64, delta: &str, bytes: &mut String) {
        let Some(item) = self.open.get_mut(&block_index) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.size = self.size.saturating_add(delta.len());
        if self.size > OUTPUT_LIMIT {
            self.fail(bytes);
            return;
        }
        item.content.push_str(delta);
        let event = json!({"item_id":item.value["id"],"output_index":item.index,"delta":delta});
        match item.value["type"].as_str() {
            Some("message") => {
                let mut event = event;
                event["content_index"] = json!(0);
                self.emit(bytes, "response.output_text.delta", event);
            }
            Some("function_call") => {
                self.emit(bytes, "response.function_call_arguments.delta", event)
            }
            _ => {}
        }
    }

    fn close(&mut self, block_index: u64, bytes: &mut String) {
        let Some(Item {
            index,
            mut value,
            content,
        }) = self.open.remove(&block_index)
        else {
            return;
        };
        match value["type"].as_str() {
            Some("message") => {
                let part = text_part(&content);
                self.emit(
                    bytes,
                    "response.output_text.done",
                    json!({
                        "item_id":value["id"],"output_index":index,"content_index":0,"text":content,
                    }),
                );
                self.emit(
                    bytes,
                    "response.content_part.done",
                    json!({
                        "item_id":value["id"],"output_index":index,"content_index":0,"part":part,
                    }),
                );
                value["content"] = json!([part]);
            }
            Some("function_call" | "custom_tool_call") => {
                let arguments = if content.is_empty() { "{}" } else { &content };
                let Some(decoded) = serde_json::from_str::<Value>(arguments)
                    .ok()
                    .filter(Value::is_object)
                else {
                    self.fail(bytes);
                    return;
                };
                if value["type"] == "custom_tool_call" {
                    let Some(input) = decoded["input"].as_str() else {
                        self.fail(bytes);
                        return;
                    };
                    self.emit(bytes, "response.custom_tool_call_input.delta", json!({
                        "item_id":value["id"],"call_id":value["call_id"],"output_index":index,"delta":input,
                    }));
                    self.emit(
                        bytes,
                        "response.custom_tool_call_input.done",
                        json!({
                            "item_id":value["id"],"output_index":index,"input":input,
                        }),
                    );
                    value["input"] = json!(input);
                } else {
                    self.emit(bytes, "response.function_call_arguments.done", json!({
                        "item_id":value["id"],"output_index":index,"arguments":arguments,"name":value["name"],
                    }));
                    value["arguments"] = json!(arguments);
                }
            }
            _ => return,
        }
        value["status"] = json!("completed");
        self.emit(
            bytes,
            "response.output_item.done",
            json!({"output_index":index,"item":value}),
        );
        self.output[index] = value;
    }

    fn envelope(&self, status: &str, usage: Value) -> Value {
        let end_turn = !self.output.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            )
        });
        json!({"id":self.id,"object":"response","created_at":self.created,"model":self.model,
            "status":status,"output":self.output,"usage":usage,"store":false,"error":null,
            "incomplete_details":null,"end_turn":end_turn})
    }

    fn fail(&mut self, bytes: &mut String) {
        self.failed = true;
        bytes.push_str(&stream_failure(&self.id, self.sequence, false));
    }

    fn emit(&mut self, bytes: &mut String, kind: &str, mut event: Value) {
        event["type"] = json!(kind);
        event["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        bytes.push_str(&proto::sse::frame_event(kind, &event.to_string()));
    }
}

pub(crate) fn stream_failure(id: &str, sequence: u64, context_exceeded: bool) -> String {
    let (code, message) = if context_exceeded {
        (
            "context_length_exceeded",
            "Prompt is too long for the model's context window",
        )
    } else {
        ("upstream_stream_error", "Upstream stream interrupted")
    };
    let event = json!({"type":"response.failed","sequence_number":sequence,"response":{
        "id":id,"object":"response","status":"failed",
        "error":{"code":code,"message":message},
    }});
    proto::sse::frame_event("response.failed", &event.to_string())
}

fn text_part(text: &str) -> Value {
    json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]})
}

fn usage_value(usage: &ReportedUsage, protocol: WireProtocol) -> Value {
    let (Some(input), Some(output)) = (usage.input_with_cache(protocol), usage.output) else {
        return Value::Null;
    };
    json!({"input_tokens":input,"output_tokens":output,"total_tokens":input.saturating_add(output),
        "input_tokens_details":{"cached_tokens":usage.cache_read,"cache_write_tokens":usage.cache_write}})
}

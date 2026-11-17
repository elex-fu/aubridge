//! Chat input plus the Messages view used by routing and cross-wire forwarding.

use std::collections::HashSet;

use proto::protocol::{
    InboundBlock, InboundContent, InboundMessage, MessagesRequest, SystemContent, ToolChoice,
    ToolDefinition, ToolResultContent,
};
use serde_json::{json, Map, Value};

pub struct ChatInput {
    pub request: MessagesRequest,
    pub include_usage: bool,
    pub native_only: bool,
    raw: Value,
    anthropic_options: Map<String, Value>,
}

impl ChatInput {
    pub fn parse(body: &str) -> Result<Self, &'static str> {
        let raw: Value = serde_json::from_str(body).map_err(|_| "Invalid JSON request")?;
        let object = raw.as_object().ok_or("Expected a Chat request object")?;
        let model = required_text(&raw["model"], "A model is required")?;
        let messages = raw["messages"]
            .as_array()
            .filter(|messages| !messages.is_empty())
            .ok_or("A nonempty messages array is required")?;
        let stream = optional_bool(&raw["stream"], "stream must be a boolean")?.unwrap_or(false);
        if !raw["stream_options"].is_null() && !raw["stream_options"].is_object() {
            return Err("stream_options must be an object");
        }
        let include_usage = stream
            && optional_bool(
                &raw["stream_options"]["include_usage"],
                "include_usage must be a boolean",
            )?
            .unwrap_or(false);
        let max_tokens = positive_integer(&raw["max_completion_tokens"])?
            .or(positive_integer(&raw["max_tokens"])?)
            .unwrap_or_default();
        let mut native_only = positive_integer(&raw["n"])?.is_some_and(|n| n != 1)
            || raw["store"] == true
            || object.iter().any(|(field, value)| {
                !value.is_null()
                    && !matches!(
                        field.as_str(),
                        "model"
                            | "messages"
                            | "stream"
                            | "stream_options"
                            | "tools"
                            | "tool_choice"
                            | "parallel_tool_calls"
                            | "max_tokens"
                            | "max_completion_tokens"
                            | "n"
                            | "temperature"
                            | "top_p"
                            | "stop"
                            | "user"
                            | "metadata"
                            | "store"
                    )
            });

        let mut system = Vec::new();
        let mut translated = Vec::new();
        let mut calls = HashSet::new();
        for message in messages {
            let role = required_text(&message["role"], "Each message needs a role")?;
            let content = content_blocks(&message["content"], &mut native_only)?;
            if message.get("name").is_some_and(|value| !value.is_null()) {
                native_only = true;
            }
            match role {
                "system" | "developer" => {
                    let text = text_of(content);
                    if !text.is_empty() {
                        native_only |= !translated.is_empty();
                        system.push(text);
                    }
                }
                "user" => {
                    native_only |= content.is_empty();
                    translated.push(InboundMessage {
                        role: "user".into(),
                        content: InboundContent::Blocks(content),
                    });
                }
                "assistant" => {
                    let mut content = content;
                    if let Some(items) =
                        optional_array(&message["tool_calls"], "tool_calls must be an array")?
                    {
                        for call in items {
                            if call["type"] != "function" {
                                native_only = true;
                                continue;
                            }
                            let id = required_text(&call["id"], "A tool call ID is required")?;
                            let function = &call["function"];
                            let name = required_text(&function["name"], "A tool name is required")?;
                            let arguments = function["arguments"]
                                .as_str()
                                .ok_or("Tool arguments must be a string")?;
                            let input = serde_json::from_str::<Value>(arguments)
                                .ok()
                                .filter(Value::is_object);
                            native_only |= input.is_none() || !calls.insert(id);
                            content.push(InboundBlock::ToolUse {
                                id: id.into(),
                                name: name.into(),
                                input: input.unwrap_or_else(|| json!({})),
                            });
                        }
                    }
                    native_only |= content.is_empty() || !message["function_call"].is_null();
                    translated.push(InboundMessage {
                        role: "assistant".into(),
                        content: InboundContent::Blocks(content),
                    });
                }
                "tool" => {
                    let id =
                        required_text(&message["tool_call_id"], "A tool result needs its call ID")?;
                    native_only |= !calls.remove(id);
                    push_tool_result(&mut translated, id, text_of(content));
                }
                _ => native_only = true,
            }
        }
        // A raw Chat request can use native-only roles. Other protocols need a
        // conversation, including for a system-only request.
        native_only |= translated.is_empty();
        let tools = tools_from_chat(&raw["tools"], &mut native_only)?;
        let parallel_tools = optional_bool(
            &raw["parallel_tool_calls"],
            "parallel_tool_calls must be a boolean",
        )?;
        let disable_parallel_tool_use =
            (parallel_tools == Some(false) && tools.is_some()).then_some(true);
        let tool_choice = choice_from_chat(
            &raw["tool_choice"],
            disable_parallel_tool_use,
            &mut native_only,
        )?;
        let anthropic_options = anthropic_options(&raw)?;
        let request = MessagesRequest {
            model: model.into(),
            messages: translated,
            system: (!system.is_empty()).then(|| SystemContent::Text(system.join("\n\n"))),
            max_tokens,
            tools,
            tool_choice,
            stream: Some(stream),
            metadata: None,
            thinking: None,
            output_config: None,
        };
        Ok(Self {
            request,
            include_usage,
            native_only,
            raw,
            anthropic_options,
        })
    }

    pub fn supports_responses(&self) -> bool {
        !self.native_only && self.anthropic_options.is_empty()
    }

    pub fn choices(&self) -> u64 {
        self.raw["n"].as_u64().unwrap_or(1)
    }

    pub fn original(&self) -> &Value {
        &self.raw
    }

    pub fn for_chat(&self, model: &str) -> Value {
        let mut body = self.raw.clone();
        body["model"] = Value::String(model.into());
        body["stream"] = Value::Bool(self.request.stream == Some(true));
        if self.request.stream == Some(true) {
            body["stream_options"]["include_usage"] = Value::Bool(true);
        } else if let Some(body) = body.as_object_mut() {
            body.remove("stream_options");
        }
        body
    }

    pub fn for_anthropic(&self, request: &MessagesRequest) -> serde_json::Result<Value> {
        let mut body = serde_json::to_value(request)?;
        body["stream"] = Value::Bool(true);
        if request.max_tokens == 0 {
            body["max_tokens"] = json!(4096);
        }
        for (field, value) in &self.anthropic_options {
            body[field] = value.clone();
        }
        Ok(body)
    }
}

fn required_text<'a>(value: &'a Value, message: &'static str) -> Result<&'a str, &'static str> {
    value
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or(message)
}

fn optional_array<'a>(
    value: &'a Value,
    message: &'static str,
) -> Result<Option<&'a [Value]>, &'static str> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_array()
            .map(|items| Some(items.as_slice()))
            .ok_or(message)
    }
}

fn optional_bool(value: &Value, message: &'static str) -> Result<Option<bool>, &'static str> {
    if value.is_null() {
        Ok(None)
    } else {
        value.as_bool().map(Some).ok_or(message)
    }
}

fn positive_integer(value: &Value) -> Result<Option<i64>, &'static str> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_i64()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or("Expected a positive integer")
    }
}

fn content_blocks(
    content: &Value,
    native_only: &mut bool,
) -> Result<Vec<InboundBlock>, &'static str> {
    // Chat clients use an empty text placeholder beside tool calls. Messages
    // upstreams such as Kimi reject it as an empty text content block.
    let text_block =
        |text: &str| (!text.is_empty()).then(|| InboundBlock::Text { text: text.into() });
    if content.is_null() {
        Ok(Vec::new())
    } else if let Some(text) = content.as_str() {
        Ok(text_block(text).into_iter().collect())
    } else if let Some(parts) = content.as_array() {
        let mut blocks = Vec::new();
        for part in parts {
            if part["type"] == "text" {
                blocks.extend(text_block(
                    part["text"].as_str().ok_or("Invalid text content")?,
                ));
            } else {
                *native_only = true;
            }
        }
        Ok(blocks)
    } else {
        Err("Invalid message content")
    }
}

fn text_of(blocks: Vec<InboundBlock>) -> String {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            InboundBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_tool_result(messages: &mut Vec<InboundMessage>, id: &str, content: String) {
    let block = InboundBlock::ToolResult {
        tool_use_id: id.into(),
        content: ToolResultContent::Text(content),
        is_error: false,
    };
    if let Some(InboundMessage {
        role,
        content: InboundContent::Blocks(blocks),
    }) = messages.last_mut()
    {
        if role == "user" && matches!(blocks.last(), Some(InboundBlock::ToolResult { .. })) {
            blocks.push(block);
            return;
        }
    }
    messages.push(InboundMessage {
        role: "user".into(),
        content: InboundContent::Blocks(vec![block]),
    });
}

fn tools_from_chat(
    value: &Value,
    native_only: &mut bool,
) -> Result<Option<Vec<ToolDefinition>>, &'static str> {
    let Some(tools) = optional_array(value, "tools must be an array")? else {
        return Ok(None);
    };
    let mut definitions = Vec::new();
    for tool in tools {
        if tool["type"] != "function" {
            *native_only = true;
            continue;
        }
        let function = &tool["function"];
        let name = required_text(&function["name"], "A tool name is required")?;
        let schema = &function["parameters"];
        *native_only |= function["strict"] == true || (!schema.is_object() && !schema.is_null());
        definitions.push(ToolDefinition {
            name: name.into(),
            description: function["description"].as_str().unwrap_or_default().into(),
            input_schema: if schema.is_null() {
                json!({"type":"object"})
            } else {
                schema.clone()
            },
        });
    }
    Ok((!definitions.is_empty()).then_some(definitions))
}

fn choice_from_chat(
    value: &Value,
    disable_parallel_tool_use: Option<bool>,
    native_only: &mut bool,
) -> Result<Option<ToolChoice>, &'static str> {
    Ok(match value.as_str() {
        Some("auto") => Some(ToolChoice::Auto {
            disable_parallel_tool_use,
        }),
        Some("required") => Some(ToolChoice::Any {
            disable_parallel_tool_use,
        }),
        Some("none") => Some(ToolChoice::None),
        _ if value.is_null() => disable_parallel_tool_use.map(|disabled| ToolChoice::Auto {
            disable_parallel_tool_use: Some(disabled),
        }),
        _ if value["type"] == "function" => Some(ToolChoice::Tool {
            disable_parallel_tool_use,
            name: required_text(
                &value["function"]["name"],
                "A named tool choice needs its name",
            )?
            .into(),
        }),
        _ => {
            *native_only = true;
            None
        }
    })
}

fn anthropic_options(raw: &Value) -> Result<Map<String, Value>, &'static str> {
    let mut options = Map::new();
    for field in ["temperature", "top_p"] {
        if !raw[field].is_null() {
            raw[field]
                .as_f64()
                .ok_or("Sampling options must be numbers")?;
            options.insert(field.into(), raw[field].clone());
        }
    }
    if let Some(stop) = raw["stop"].as_str() {
        options.insert("stop_sequences".into(), json!([stop]));
    } else if let Some(stops) =
        optional_array(&raw["stop"], "stop must be a string or an array of strings")?
    {
        if !stops.iter().all(Value::is_string) {
            return Err("stop must contain strings");
        }
        if !stops.is_empty() {
            options.insert("stop_sequences".into(), Value::Array(stops.to_vec()));
        }
    }
    Ok(options)
}

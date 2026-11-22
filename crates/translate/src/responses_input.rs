//! Stateless Responses input and its compatible Messages view.

use std::collections::{BTreeMap, BTreeSet};

use proto::protocol::{
    InboundBlock, InboundContent, InboundMessage, MessagesRequest, SystemContent, ToolChoice,
    ToolDefinition, ToolResultContent,
};
use serde_json::{json, Value};

#[derive(Clone, PartialEq, Eq)]
pub struct ClientTool {
    pub name: String,
    pub namespace: Option<String>,
    pub custom: bool,
}

pub struct ResponsesInput {
    pub request: MessagesRequest,
    pub tool_names: BTreeMap<String, ClientTool>,
    pub compatible: bool,
    pub has_tools: bool,
    raw: Value,
}

impl ResponsesInput {
    pub fn parse(body: &str) -> Result<Self, &'static str> {
        let raw: Value = serde_json::from_str(body).map_err(|_| "Invalid JSON request")?;
        let object = raw
            .as_object()
            .ok_or("Expected a Responses request object")?;
        let model = raw["model"]
            .as_str()
            .filter(|model| !model.trim().is_empty())
            .ok_or("A model is required")?;
        if raw["stream"] != true {
            return Err("This Responses endpoint requires stream: true");
        }
        if (!raw["store"].is_null() && raw["store"] != false)
            || (!raw["background"].is_null() && raw["background"] != false)
            || ["previous_response_id", "conversation", "prompt"]
                .iter()
                .any(|field| !raw[*field].is_null())
        {
            return Err("Send complete input history with store: false over HTTP");
        }
        let max_tokens = if raw["max_output_tokens"].is_null() {
            0
        } else {
            raw["max_output_tokens"]
                .as_i64()
                .filter(|max| *max > 0)
                .ok_or("Expected a positive max_output_tokens")?
        };
        if !raw["parallel_tool_calls"].is_null() && !raw["parallel_tool_calls"].is_boolean() {
            return Err("parallel_tool_calls must be a boolean");
        }
        let tools = raw["tools"].as_array();
        if tools.is_none() && !raw["tools"].is_null() {
            return Err("tools must be an array");
        }
        let has_tools = tools.is_some_and(|tools| !tools.is_empty())
            || raw["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "additional_tools"));
        crate::media::validate_raw(&raw, crate::media::Wire::Responses)?;
        let mut bridge = Bridge::default();
        let fields_supported = object.iter().all(|(field, value)| {
            value.is_null()
                || matches!(
                    field.as_str(),
                    "model"
                        | "input"
                        | "instructions"
                        | "tools"
                        | "tool_choice"
                        | "stream"
                        | "store"
                        | "max_output_tokens"
                        | "parallel_tool_calls"
                        | "include"
                        | "reasoning"
                        | "text"
                        | "prompt_cache_key"
                        | "client_metadata"
                        | "background"
                )
        }) && (raw["text"]["format"].is_null()
            || raw["text"]["format"]["type"] == "text")
            && (raw["include"].is_null()
                || raw["include"].as_array().is_some_and(|values| {
                    values
                        .iter()
                        .all(|value| value == "reasoning.encrypted_content")
                }));
        let mut compatible = fields_supported
            && !raw["input"].as_array().into_iter().flatten().any(|item| {
                [&item["content"], &item["output"]].iter().any(|parts| {
                    parts.as_array().into_iter().flatten().any(|part| {
                        part["type"] == "input_image"
                            && part["detail"]
                                .as_str()
                                .is_some_and(|detail| detail != "auto")
                    })
                })
            });
        if let Some(instructions) = raw["instructions"].as_str() {
            bridge.instructions.push(instructions.into());
        } else if !raw["instructions"].is_null() {
            compatible = false;
        }
        for tool in tools.into_iter().flatten() {
            compatible &= bridge.add_tool(tool, None).is_some();
        }
        match &raw["input"] {
            Value::String(text) if !text.is_empty() => bridge.text("user", text.clone()),
            Value::Array(items) if !items.is_empty() => {
                for item in items {
                    compatible &= bridge.add_item(item).is_some();
                }
            }
            _ => return Err("A nonempty input string or array is required"),
        }
        let choice = bridge.choice(&raw["tool_choice"]);
        compatible &= choice.is_some() && bridge.pending.is_empty() && !bridge.messages.is_empty();
        let request = MessagesRequest {
            model: model.into(),
            messages: bridge.messages,
            system: (!bridge.instructions.is_empty())
                .then(|| SystemContent::Text(bridge.instructions.join("\n\n"))),
            max_tokens,
            tools: (!bridge.tools.is_empty()).then_some(bridge.tools),
            tool_choice: choice.flatten(),
            stream: Some(true),
            metadata: None,
            thinking: None,
            output_config: None,
        };
        crate::media::check(&request, crate::media::Wire::Responses, true)?;
        Ok(Self {
            request,
            tool_names: bridge.names,
            compatible,
            has_tools,
            raw,
        })
    }

    /// Native Responses passthrough body.
    ///
    /// The public Responses API accepts the shorthand `"input": "..."` form,
    /// but the ChatGPT Codex backend rejects it with `400 invalid_request`;
    /// Codex CLI always sends the item-array shape. Normalize the shorthand
    /// here so both accepted client shapes reach the upstream unchanged in
    /// meaning. Array inputs are forwarded verbatim.
    pub fn for_native(&self, model: &str) -> Value {
        let mut body = self.raw.clone();
        body["model"] = json!(model);
        body["store"] = json!(false);
        if let Some(text) = body["input"].as_str().map(str::to_owned) {
            body["input"] = json!([{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            }]);
        }
        body
    }

    pub fn original(&self) -> &Value {
        &self.raw
    }

    pub fn parallel_tools_disabled(&self) -> bool {
        self.raw["parallel_tool_calls"] == false
    }
}

#[derive(Default)]
struct Bridge {
    instructions: Vec<String>,
    messages: Vec<InboundMessage>,
    tools: Vec<ToolDefinition>,
    names: BTreeMap<String, ClientTool>,
    pending: BTreeSet<String>,
}

impl Bridge {
    fn add_tool(&mut self, tool: &Value, namespace: Option<&str>) -> Option<()> {
        let name = tool["name"].as_str()?;
        if tool["type"] == "namespace" && namespace.is_none() {
            let members = tool["tools"].as_array()?;
            return members
                .iter()
                .try_for_each(|member| self.add_tool(member, Some(name)));
        }
        let parameters = match tool["type"].as_str()? {
            "function" if tool["strict"] != true => {
                tool["parameters"].as_object()?;
                tool["parameters"].clone()
            }
            "custom" if tool["format"].is_null() || tool["format"]["type"] == "text" => {
                json!({"type":"object", "properties":{"input":{"type":"string"}}, "required":["input"], "additionalProperties":false})
            }
            _ => return None,
        };
        let alias = self.register(name, namespace, tool["type"] == "custom")?;
        if self.tools.iter().any(|tool| tool.name == alias) {
            return None;
        }
        self.tools.push(ToolDefinition {
            name: alias,
            description: tool["description"].as_str().unwrap_or_default().into(),
            input_schema: parameters,
        });
        Some(())
    }

    fn add_item(&mut self, item: &Value) -> Option<()> {
        let kind = item["type"].as_str().unwrap_or("message");
        match kind {
            "message" => {
                let role = item["role"].as_str()?;
                let content = blocks_content(&item["content"])?;
                match role {
                    "developer" | "system" => {
                        let text = content
                            .iter()
                            .map(|b| match b {
                                InboundBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()?;
                        self.instructions.push(text.join("\n"));
                    }
                    "user" | "assistant" => {
                        for block in content {
                            self.push(role, block);
                        }
                    }
                    _ => return None,
                }
            }
            "function_call" | "custom_tool_call" => {
                let name = item["name"].as_str()?;
                let namespace = item["namespace"]
                    .as_str()
                    .filter(|namespace| !namespace.is_empty());
                let custom = kind == "custom_tool_call";
                let alias = self.register(name, namespace, custom)?;
                let id = item["call_id"].as_str()?.to_string();
                let input = if custom {
                    json!({"input":item["input"].as_str()?})
                } else {
                    let value: Value = serde_json::from_str(item["arguments"].as_str()?).ok()?;
                    value.as_object()?;
                    value
                };
                if id.is_empty() || !self.pending.insert(id.clone()) {
                    return None;
                }
                self.push(
                    "assistant",
                    InboundBlock::ToolUse {
                        id,
                        name: alias,
                        input,
                    },
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = item["call_id"].as_str()?;
                let output = blocks_content(&item["output"])?;
                if !self.pending.remove(id) {
                    return None;
                }
                self.push(
                    "user",
                    InboundBlock::ToolResult {
                        tool_use_id: id.into(),
                        content: ToolResultContent::from_blocks(output),
                        is_error: false,
                    },
                );
            }
            // Opaque and model-specific. The raw native request still has it.
            "reasoning" => {}
            _ => return None,
        }
        Some(())
    }

    fn register(&mut self, name: &str, namespace: Option<&str>, custom: bool) -> Option<String> {
        let alias = match namespace {
            Some(namespace) => format!("{namespace}__{name}"),
            None => name.into(),
        };
        if name.is_empty()
            || alias.len() > 64
            || !alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        let client = ClientTool {
            name: name.into(),
            namespace: namespace.map(str::to_owned),
            custom,
        };
        if self
            .names
            .get(&alias)
            .is_some_and(|existing| existing != &client)
        {
            return None;
        }
        self.names.insert(alias.clone(), client);
        Some(alias)
    }

    fn choice(&self, value: &Value) -> Option<Option<ToolChoice>> {
        Some(match value.as_str() {
            Some("auto") => Some(ToolChoice::Auto {
                disable_parallel_tool_use: None,
            }),
            Some("required") => Some(ToolChoice::Any {
                disable_parallel_tool_use: None,
            }),
            Some("none") => Some(ToolChoice::None),
            _ if value.is_null() => None,
            _ if matches!(value["type"].as_str(), Some("function" | "custom")) => {
                let name = value["name"].as_str()?;
                let namespace = value["namespace"].as_str();
                let alias = self
                    .names
                    .iter()
                    .find(|(_, tool)| {
                        tool.name == name
                            && tool.namespace.as_deref() == namespace
                            && tool.custom == (value["type"] == "custom")
                    })?
                    .0;
                Some(ToolChoice::Tool {
                    disable_parallel_tool_use: None,
                    name: alias.clone(),
                })
            }
            _ => return None,
        })
    }

    fn text(&mut self, role: &str, text: String) {
        if !text.is_empty() {
            self.push(role, InboundBlock::Text { text });
        }
    }

    fn push(&mut self, role: &str, block: InboundBlock) {
        if let Some(last) = self.messages.last_mut().filter(|last| last.role == role) {
            if let InboundContent::Blocks(blocks) = &mut last.content {
                blocks.push(block);
                return;
            }
        }
        self.messages.push(InboundMessage {
            role: role.into(),
            content: InboundContent::Blocks(vec![block]),
        });
    }
}

fn blocks_content(content: &Value) -> Option<Vec<InboundBlock>> {
    if let Some(text) = content.as_str() {
        return Some(vec![InboundBlock::Text { text: text.into() }]);
    }
    content
        .as_array()?
        .iter()
        .map(|part| match part["type"].as_str()? {
            "input_text" | "output_text" | "text" => Some(InboundBlock::Text {
                text: part["text"].as_str()?.into(),
            }),
            "refusal" => Some(InboundBlock::Text {
                text: part["refusal"].as_str()?.into(),
            }),
            _ => crate::media::parse_part(part, crate::media::Wire::Responses)
                .ok()
                .flatten(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(body: Value) -> ResponsesInput {
        ResponsesInput::parse(&body.to_string()).unwrap()
    }

    #[test]
    fn native_string_input_becomes_message_items() {
        let input = parsed(json!({"model": "m", "stream": true, "input": "hi"}));
        assert_eq!(
            input.for_native("real"),
            json!({
                "model": "real",
                "stream": true,
                "store": false,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}],
                }],
            })
        );
    }

    #[test]
    fn native_array_input_is_forwarded_verbatim() {
        let items = json!([{"type": "message", "role": "user", "content": "hi"}]);
        let input = parsed(json!({"model": "m", "stream": true, "input": items}));
        assert_eq!(
            input.for_native("real"),
            json!({"model": "real", "stream": true, "store": false, "input": items})
        );
    }
}

//! Portable Google input. Opaque Google state cannot cross provider boundaries.
use crate::media::{self, Wire};
use proto::protocol::*;
use serde_json::{json, Value};
use std::collections::HashSet;

pub struct GeminiInput {
    pub request: MessagesRequest,
    raw: Value,
    options: Value,
}

struct Call {
    id: String,
    name: String,
    answered: bool,
}

impl GeminiInput {
    pub fn parse(model: &str, operation: &str, body: &str) -> Result<Self, &'static str> {
        if model.is_empty()
            || !matches!(
                operation,
                "generateContent" | "streamGenerateContent" | "countTokens"
            )
        {
            return Err("Invalid Gemini model or operation");
        }
        let raw: Value = serde_json::from_str(body).map_err(|_| "Invalid JSON request")?;
        let original = raw.clone();
        let raw = unwrap_count_request(model, operation, raw)?;
        let object = raw.as_object().ok_or("Expected a Gemini request object")?;
        for (key, value) in object {
            if !matches!(
                key.as_str(),
                "contents" | "systemInstruction" | "tools" | "toolConfig" | "generationConfig"
            ) && meaningful(value)
            {
                return Err("Unsupported Gemini request option");
            }
        }
        let options = raw.get("generationConfig").cloned().unwrap_or(json!({}));
        validate_options(&options)?;
        let contents = raw["contents"]
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or("A nonempty contents array is required")?;
        // Reserve explicit IDs first so deterministic generated IDs never collide with later calls.
        let mut ids = HashSet::new();
        for c in contents {
            for p in parts(c)? {
                if let Some(id) = p["functionCall"]["id"].as_str() {
                    if !id.is_empty() {
                        ids.insert(id.to_owned());
                    }
                }
            }
        }
        let mut serial = 0usize;
        let mut seen = HashSet::new();
        let mut pending: Vec<Call> = Vec::new();
        let mut messages = Vec::new();
        for content in contents {
            let role = match content
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
            {
                "user" => "user",
                "model" => "assistant",
                _ => return Err("Invalid Gemini content role"),
            };
            if role == "assistant" && pending.iter().any(|c| !c.answered) {
                return Err("Unanswered Gemini function call batch");
            }
            if role == "assistant" {
                pending.clear();
            }
            let mut blocks = Vec::new();
            for part in parts(content)? {
                if part.get("thoughtSignature").is_some() || part.get("thought_signature").is_some()
                {
                    // The SDK inserts this public absence marker after an unsigned call.
                    // Only the recorded call position is portable; real signatures stay opaque.
                    let absence_marker = role == "assistant"
                        && part["thoughtSignature"] == "skip_thought_signature_validator"
                        && part["functionCall"].is_object()
                        && part.as_object().is_some_and(|object| {
                            object.keys().all(|key| {
                                matches!(key.as_str(), "functionCall" | "thoughtSignature")
                            })
                        });
                    if !absence_marker {
                        return Err("Google thoughtSignature cannot cross providers");
                    }
                }
                let present = [
                    "text",
                    "inlineData",
                    "fileData",
                    "functionCall",
                    "functionResponse",
                ]
                .iter()
                .filter(|key| part.get(**key).is_some())
                .count();
                if present != 1 {
                    return Err("Unsupported or ambiguous Gemini part");
                }
                // F-3: unsigned Google reasoning cannot become signed Anthropic history.
                if part["thought"] == true {
                    if role != "assistant" || !part["text"].is_string() {
                        return Err("Thought text requires model role");
                    }
                    continue;
                }
                if let Some(call) = part.get("functionCall") {
                    if role != "assistant" {
                        return Err("functionCall requires model role");
                    }
                    let name = text(&call["name"])?.to_owned();
                    let args = call.get("args").cloned().unwrap_or(json!({}));
                    if !args.is_object() {
                        return Err("functionCall args must be an object");
                    }
                    let id = if let Some(id) = call.get("id") {
                        text(id)?.to_owned()
                    } else {
                        loop {
                            serial += 1;
                            let id = format!("gemini_call_{serial}");
                            if ids.insert(id.clone()) {
                                break id;
                            }
                        }
                    };
                    if !seen.insert(id.clone()) {
                        return Err("Duplicate Gemini function call id");
                    }
                    pending.push(Call {
                        id: id.clone(),
                        name: name.clone(),
                        answered: false,
                    });
                    blocks.push(InboundBlock::ToolUse {
                        id,
                        name,
                        input: args,
                    });
                } else if let Some(response) = part.get("functionResponse") {
                    if role != "user" {
                        return Err("functionResponse requires user role");
                    }
                    let name = text(&response["name"])?;
                    let call = if let Some(id) = response.get("id") {
                        let id = text(id)?;
                        pending
                            .iter_mut()
                            .find(|c| c.id == id && c.name == name && !c.answered)
                    } else {
                        pending.iter_mut().find(|c| c.name == name && !c.answered)
                    }
                    .ok_or("Orphan, duplicate or mismatched Gemini functionResponse")?;
                    call.answered = true;
                    let result = response
                        .get("response")
                        .filter(|v| v.is_object())
                        .ok_or("functionResponse response must be an object")?;
                    if response.get("parts").is_some() {
                        return Err("Unsupported functionResponse media parts");
                    }
                    blocks.push(InboundBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        content: ToolResultContent::Text(result.to_string()),
                        is_error: result.get("error").is_some(),
                    });
                } else {
                    if pending.iter().any(|c| !c.answered) && role == "user" {
                        return Err("Unanswered Gemini function call batch");
                    }
                    blocks.push(portable_part(part)?);
                }
            }
            if blocks.is_empty() {
                continue;
            }
            messages.push(InboundMessage {
                role: role.into(),
                content: InboundContent::Blocks(blocks),
            });
        }
        if pending.iter().any(|c| !c.answered) {
            return Err("Unanswered Gemini function call batch");
        }
        if messages.is_empty() {
            return Err("No portable Gemini contents");
        }
        let system = if let Some(system) = raw.get("systemInstruction") {
            let mut blocks = Vec::new();
            for part in parts(system)? {
                if part.get("thoughtSignature").is_some()
                    || part.get("thought_signature").is_some()
                    || part
                        .as_object()
                        .is_none_or(|p| p.keys().any(|k| k != "text"))
                {
                    return Err("System instruction must contain text only");
                }
                blocks.push(InboundTextBlock {
                    kind: "text".into(),
                    text: part["text"].as_str().ok_or("Invalid system text")?.into(),
                });
            }
            Some(SystemContent::Blocks(blocks))
        } else {
            None
        };
        let tools = parse_tools(&raw)?;
        let tool_choice = parse_choice(&raw, tools.as_deref().unwrap_or_default())?;
        let request = MessagesRequest {
            model: model.into(),
            messages,
            system,
            max_tokens: options["maxOutputTokens"].as_i64().unwrap_or_default(),
            tools,
            tool_choice,
            stream: Some(operation == "streamGenerateContent"),
            metadata: None,
            thinking: None,
            output_config: None,
        };
        media::check(&request, Wire::Messages, true)?;
        Ok(Self {
            request,
            raw: original,
            options,
        })
    }
    pub fn original(&self) -> &Value {
        &self.raw
    }
    pub fn supports(&self, wire: Wire) -> bool {
        self.supports_target(wire, false)
    }
    pub fn supports_target(&self, wire: Wire, chat_top_k: bool) -> bool {
        self.apply_target_options(wire, json!({}), chat_top_k)
            .is_ok()
    }
    pub fn apply_options(&self, wire: Wire, body: Value) -> Result<Value, &'static str> {
        self.apply_target_options(wire, body, false)
    }
    /// Capability comes from the gateway's exact provider endpoint check, never the request.
    pub fn apply_target_options(
        &self,
        wire: Wire,
        mut body: Value,
        chat_top_k: bool,
    ) -> Result<Value, &'static str> {
        if !body.is_object() {
            return Err("Expected an upstream request object");
        }
        for (source, target) in [("temperature", "temperature"), ("topP", "top_p")] {
            if let Some(value) = self.options.get(source) {
                body[target] = value.clone();
            }
        }
        if let Some(value) = self.options.get("topK") {
            if wire != Wire::Messages && !(wire == Wire::Chat && chat_top_k) {
                return Err("Gemini topK is unsupported by this route");
            }
            body["top_k"] = value.clone();
        }
        if let Some(value) = self
            .options
            .get("stopSequences")
            .filter(|v| v.as_array().is_some_and(|v| !v.is_empty()))
        {
            if wire == Wire::Responses {
                return Err("Gemini stopSequences is unsupported by Responses");
            }
            body[if wire == Wire::Chat {
                "stop"
            } else {
                "stop_sequences"
            }] = value.clone();
        }
        Ok(body)
    }
}

fn meaningful(v: &Value) -> bool {
    !v.is_null() && v != &json!([]) && v != &json!({})
}
fn text(v: &Value) -> Result<&str, &'static str> {
    v.as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Expected a nonempty Gemini string")
}
fn parts(v: &Value) -> Result<&Vec<Value>, &'static str> {
    v["parts"]
        .as_array()
        .filter(|p| !p.is_empty())
        .ok_or("Gemini content requires parts")
}

fn validate_options(options: &Value) -> Result<(), &'static str> {
    for (key, value) in options
        .as_object()
        .ok_or("generationConfig must be an object")?
    {
        match key.as_str() {
            "maxOutputTokens" | "topK" if value.as_i64().is_some_and(|v| v > 0) => {}
            "temperature"
                if value
                    .as_f64()
                    .is_some_and(|v| v.is_finite() && (0.0..=2.0).contains(&v)) => {}
            "topP"
                if value
                    .as_f64()
                    .is_some_and(|v| v.is_finite() && (0.0..=1.0).contains(&v)) => {}
            "stopSequences"
                if value
                    .as_array()
                    .is_some_and(|v| v.iter().all(Value::is_string)) => {}
            "candidateCount" if value == 1 => {}
            "responseMimeType" if value == "text/plain" => {}
            // Visibility alone does not request a reasoning budget from another vendor.
            "thinkingConfig"
                if value.as_object().is_some_and(|config| {
                    config.len() == 1 && config.get("includeThoughts") == Some(&json!(true))
                }) => {}
            _ if !meaningful(value) => {}
            _ => return Err("Unsupported Gemini generationConfig option"),
        }
    }
    Ok(())
}

fn portable_part(part: &Value) -> Result<InboundBlock, &'static str> {
    if let Some(value) = part.get("text") {
        let value = value.as_str().ok_or("Invalid Gemini text")?.to_owned();
        return Ok(InboundBlock::Text { text: value });
    }
    if part.get("thought").is_some_and(meaningful) {
        return Err("Invalid Gemini thought part");
    }
    if let Some(data) = part.get("inlineData") {
        let mime = text(&data["mimeType"])?;
        let data = text(&data["data"])?;
        if mime == "text/plain" {
            return media::parse_part(
                &json!({"type":"input_file","file_data":format!("data:text/plain;base64,{data}")}),
                Wire::Responses,
            )?
            .ok_or("Invalid text document");
        }
        if !mime.starts_with("image/") {
            return Err("Unsupported Gemini inline media");
        }
        return Ok(InboundBlock::Image {
            source: MediaSource::Base64 {
                media_type: mime.into(),
                data: data.into(),
            },
        });
    }
    if let Some(file) = part.get("fileData") {
        let mime = text(&file["mimeType"])?;
        let url = text(&file["fileUri"])?;
        if !mime.starts_with("image/")
            || google_file_host(url)?
            || (!url.starts_with("https://") && !url.starts_with("http://"))
        {
            return Err("Opaque Google files cannot cross providers");
        }
        return Ok(InboundBlock::Image {
            source: MediaSource::Url { url: url.into() },
        });
    }
    Err("Unsupported Gemini part")
}

fn parse_tools(raw: &Value) -> Result<Option<Vec<ToolDefinition>>, &'static str> {
    let Some(groups) = raw.get("tools") else {
        return Ok(None);
    };
    let mut tools = Vec::new();
    let mut names = HashSet::new();
    for group in groups.as_array().ok_or("tools must be an array")? {
        if group
            .as_object()
            .is_none_or(|g| g.keys().any(|k| k != "functionDeclarations"))
        {
            return Err("Unsupported Google tool");
        }
        for decl in group["functionDeclarations"]
            .as_array()
            .ok_or("functionDeclarations must be an array")?
        {
            let name = text(&decl["name"])?.to_owned();
            if !names.insert(name.clone()) {
                return Err("Duplicate function declaration");
            }
            if decl.get("parameters").is_some() && decl.get("parametersJsonSchema").is_some() {
                return Err("Ambiguous function schema");
            }
            let schema = if let Some(schema) = decl.get("parametersJsonSchema") {
                schema.clone()
            } else {
                schema_to_json(
                    decl.get("parameters")
                        .unwrap_or(&json!({"type":"object","properties":{}})),
                )?
            };
            if !schema.is_object() {
                return Err("Invalid function schema");
            }
            if decl.get("behavior").is_some_and(meaningful) {
                return Err("Unsupported function behavior");
            }
            tools.push(ToolDefinition {
                name,
                description: decl["description"].as_str().unwrap_or_default().into(),
                input_schema: schema,
            });
        }
    }
    Ok(Some(tools))
}

// Schema keywords recurse; property names are user data and must remain byte-identical.
fn schema_to_json(schema: &Value) -> Result<Value, &'static str> {
    let mut out = schema
        .as_object()
        .ok_or("Invalid Google function schema")?
        .clone();
    if let Some(kind) = out.get_mut("type") {
        if let Some(value) = kind.as_str() {
            *kind = json!(value.to_ascii_lowercase());
        }
    }
    if let Some(properties) = out.get_mut("properties") {
        for value in properties
            .as_object_mut()
            .ok_or("Invalid schema properties")?
            .values_mut()
        {
            *value = schema_to_json(value)?;
        }
    }
    if let Some(items) = out.get_mut("items") {
        *items = schema_to_json(items)?;
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = out.get_mut(key) {
            for branch in branches.as_array_mut().ok_or("Invalid schema branches")? {
                *branch = schema_to_json(branch)?;
            }
        }
    }
    if out.remove("nullable") == Some(json!(true)) {
        if let Some(kind) = out.get("type").and_then(Value::as_str) {
            out.insert("type".into(), json!([kind, "null"]));
        }
    }
    Ok(Value::Object(out))
}

fn parse_choice(raw: &Value, tools: &[ToolDefinition]) -> Result<Option<ToolChoice>, &'static str> {
    let Some(config) = raw.get("toolConfig") else {
        return Ok(None);
    };
    if config
        .as_object()
        .is_none_or(|c| c.keys().any(|k| k != "functionCallingConfig"))
    {
        return Err("Unsupported toolConfig");
    }
    let call = &config["functionCallingConfig"];
    let names = call
        .get("allowedFunctionNames")
        .map(|v| v.as_array().ok_or("Invalid allowedFunctionNames"))
        .transpose()?;
    if let Some(names) = names {
        for name in names {
            if !tools
                .iter()
                .any(|tool| Some(tool.name.as_str()) == name.as_str())
            {
                return Err("Unknown allowed function name");
            }
        }
    }
    let choice = match call["mode"].as_str().unwrap_or("AUTO") {
        "AUTO" if names.is_none_or(Vec::is_empty) => ToolChoice::Auto {
            disable_parallel_tool_use: None,
        },
        "NONE" if names.is_none_or(Vec::is_empty) => ToolChoice::None,
        "ANY" if names.is_some_and(|n| n.len() == 1) => ToolChoice::Tool {
            name: text(&names.unwrap()[0])?.into(),
            disable_parallel_tool_use: None,
        },
        "ANY" if names.is_none_or(Vec::is_empty) => ToolChoice::Any {
            disable_parallel_tool_use: None,
        },
        _ => return Err("Unsupported Gemini function calling mode or subset"),
    };
    Ok(Some(choice))
}

fn unwrap_count_request(model: &str, operation: &str, raw: Value) -> Result<Value, &'static str> {
    let Some(inner) = raw.get("generateContentRequest") else {
        return Ok(raw);
    };
    if operation != "countTokens"
        || raw.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| key != "generateContentRequest" && key != "model")
        })
    {
        return Err("countTokens requires mutually exclusive contents or generateContentRequest");
    }
    let same_model = |value: &Value| -> bool {
        value.as_str().is_some_and(|value| {
            value.strip_prefix("models/").unwrap_or(value)
                == model.strip_prefix("models/").unwrap_or(model)
        })
    };
    if raw.get("model").is_some_and(|value| !same_model(value))
        || inner.get("model").is_some_and(|value| !same_model(value))
    {
        return Err("countTokens inner model differs from path model");
    }
    let mut inner = inner
        .as_object()
        .ok_or("generateContentRequest must be an object")?
        .clone();
    inner.remove("model");
    Ok(Value::Object(inner))
}

fn google_file_host(url: &str) -> Result<bool, &'static str> {
    let (_, tail) = url.split_once("://").ok_or("Invalid image URL")?;
    let authority = tail.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('%') || authority.contains('@') {
        return Err("Unsupported image URL authority");
    }
    let host = if authority.starts_with('[') {
        authority.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    Ok(host
        .trim_end_matches('.')
        .eq_ignore_ascii_case("generativelanguage.googleapis.com"))
}

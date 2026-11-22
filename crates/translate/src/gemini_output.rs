//! Canonical Anthropic events to Google's data-only SSE and whole responses.
use proto::{
    gemini::{Candidate, Content, GenerateContentResponse},
    protocol::*,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};

const MAX_BLOCKS: usize = 1024;
const MAX_ARGS: usize = 1024 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

struct Block {
    kind: Kind,
    closed: bool,
}
enum Kind {
    Text,
    Thought,
    Tool {
        id: String,
        name: String,
        input: Value,
        args: String,
        part: Option<Value>,
    },
}

pub struct GeminiEncoder {
    model: String,
    model_reported: bool,
    id: String,
    blocks: BTreeMap<usize, Block>,
    tools: VecDeque<usize>,
    parts: Vec<Value>,
    bytes: usize,
    arg_bytes: usize,
    reason: Option<String>,
    terminal: bool,
    error: Option<&'static str>,
    input: Option<u64>,
    output: Option<u64>,
    cache: Option<u64>,
    write: Option<u64>,
    usage_override: Option<Value>,
}

impl GeminiEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.into(),
            model_reported: false,
            id: String::new(),
            blocks: BTreeMap::new(),
            tools: VecDeque::new(),
            parts: Vec::new(),
            bytes: 0,
            arg_bytes: 0,
            reason: None,
            terminal: false,
            error: None,
            input: None,
            output: None,
            cache: None,
            write: None,
            usage_override: None,
        }
    }
    /// Gateway supplies only counts proven by the original upstream wire.
    pub fn set_usage(&mut self, usage: Value) {
        self.usage_override = Some(usage);
    }
    pub fn set_model(&mut self, model: &str) {
        if !model.is_empty() {
            self.model = model.into();
            self.model_reported = true;
        }
    }
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }
    pub fn push(&mut self, event: &AnthropicEvent) -> Vec<Value> {
        match serde_json::to_value(event) {
            Ok(raw) => self.push_raw(&raw),
            Err(_) => self.fail("Invalid canonical event"),
        }
    }
    pub fn push_raw(&mut self, raw: &Value) -> Vec<Value> {
        if self.terminal {
            return Vec::new();
        }
        self.observe_usage(raw);
        let event: AnthropicEvent = match serde_json::from_value(raw.clone()) {
            Ok(e) => e,
            Err(_) => return self.fail("Invalid canonical upstream event"),
        };
        match self.event(event) {
            Ok(parts) => parts,
            Err(message) => self.fail(message),
        }
    }
    pub fn finish(&mut self) -> Vec<Value> {
        if self.terminal {
            Vec::new()
        } else {
            self.fail("Upstream stream ended before message_stop")
        }
    }
    pub fn response(&self) -> Result<Value, &'static str> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.terminal {
            return Err("Incomplete upstream response");
        }
        Ok(self.chunk(self.parts.clone(), self.reason.clone(), self.usage()))
    }
    fn event(&mut self, event: AnthropicEvent) -> Result<Vec<Value>, &'static str> {
        let mut output = Vec::new();
        match event {
            AnthropicEvent::MessageStart { message } => {
                self.id = message.id;
                if !self.model_reported && !message.model.is_empty() {
                    self.model = message.model;
                }
            }
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if self.blocks.len() >= MAX_BLOCKS || self.blocks.contains_key(&index) {
                    return Err("Invalid or excessive upstream content blocks");
                }
                let (kind, part) = match content_block {
                    ContentBlockStart::Text { text } => {
                        (Kind::Text, (!text.is_empty()).then(|| json!({"text":text})))
                    }
                    ContentBlockStart::Thinking { thinking, .. } => (
                        Kind::Thought,
                        (!thinking.is_empty()).then(|| json!({"text":thinking,"thought":true})),
                    ),
                    ContentBlockStart::ToolUse { id, name, input } => {
                        // The canonical streaming emitter uses null before argument deltas.
                        // Only this start sentinel defaults; explicit delta JSON still validates at stop.
                        let input = if input.is_null() { json!({}) } else { input };
                        if id.is_empty() || name.is_empty() || !input.is_object() {
                            return Err("Invalid upstream function call");
                        }
                        let size = input.to_string().len();
                        if size > MAX_ARGS || self.arg_bytes.saturating_add(size) > MAX_BODY {
                            return Err("Function arguments exceeded limit");
                        }
                        self.arg_bytes += size;
                        self.tools.push_back(index);
                        (
                            Kind::Tool {
                                id,
                                name,
                                input,
                                args: String::new(),
                                part: None,
                            },
                            None,
                        )
                    }
                };
                self.blocks.insert(
                    index,
                    Block {
                        kind,
                        closed: false,
                    },
                );
                if let Some(part) = part {
                    self.emit(part, &mut output)?;
                }
            }
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.closed)
                    .ok_or("Delta for unknown or closed content block")?;
                let part = match (&mut block.kind, delta) {
                    (Kind::Text, AnthropicDelta::TextDelta { text }) => Some(json!({"text":text})),
                    (Kind::Thought, AnthropicDelta::ThinkingDelta { thinking }) => {
                        Some(json!({"text":thinking,"thought":true}))
                    }
                    (Kind::Thought, AnthropicDelta::SignatureDelta { .. }) => None,
                    (Kind::Tool { args, .. }, AnthropicDelta::InputJsonDelta { partial_json }) => {
                        if args.len().saturating_add(partial_json.len()) > MAX_ARGS {
                            return Err("Function arguments exceeded limit");
                        }
                        args.push_str(&partial_json);
                        None
                    }
                    _ => return Err("Mismatched upstream content delta"),
                };
                if let Some(part) = part {
                    self.emit(part, &mut output)?;
                }
            }
            AnthropicEvent::ContentBlockStop { index } => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.closed)
                    .ok_or("Stop for unknown or closed content block")?;
                if let Kind::Tool {
                    id,
                    name,
                    input,
                    args,
                    part,
                } = &mut block.kind
                {
                    let parsed = if args.is_empty() {
                        input.clone()
                    } else {
                        serde_json::from_str(args).map_err(|_| "Invalid function argument JSON")?
                    };
                    if !parsed.is_object() {
                        return Err("Function arguments must be an object");
                    }
                    *part = Some(json!({"functionCall":{"id":id,"name":name,"args":parsed}}));
                }
                block.closed = true;
                while let Some(index) = self.tools.front().copied() {
                    if !self.blocks[&index].closed {
                        break;
                    }
                    self.tools.pop_front();
                    let block = self
                        .blocks
                        .get_mut(&index)
                        .ok_or("Missing function block")?;
                    if let Kind::Tool { part, .. } = &mut block.kind {
                        if let Some(part) = part.take() {
                            self.emit(part, &mut output)?;
                        }
                    }
                }
            }
            AnthropicEvent::MessageDelta { delta, .. } => {
                if let Some(reason) = delta.stop_reason {
                    self.reason = Some(
                        match reason.as_str() {
                            "end_turn" | "stop_sequence" | "tool_use" => "STOP",
                            "max_tokens" => "MAX_TOKENS",
                            "refusal" => "SAFETY",
                            _ => return Err("Unsupported upstream finish reason"),
                        }
                        .into(),
                    );
                }
            }
            AnthropicEvent::MessageStop => {
                if self.blocks.values().any(|b| !b.closed) || self.reason.is_none() {
                    return Err("Incomplete upstream response");
                }
                self.terminal = true;
                output.push(self.chunk(Vec::new(), self.reason.clone(), self.usage()));
            }
            AnthropicEvent::Error { .. } => return Err("Upstream stream failed"),
            AnthropicEvent::Ping => {}
        }
        Ok(output)
    }
    fn emit(&mut self, part: Value, output: &mut Vec<Value>) -> Result<(), &'static str> {
        self.bytes = self.bytes.saturating_add(part.to_string().len());
        if self.bytes > MAX_BODY {
            return Err("Response exceeded size limit");
        }
        self.parts.push(part.clone());
        output.push(self.chunk(vec![part], None, None));
        Ok(())
    }
    fn chunk(
        &self,
        parts: Vec<Value>,
        finish_reason: Option<String>,
        usage_metadata: Option<Value>,
    ) -> Value {
        // All fields are JSON-compatible primitives; no vendor signature is synthesized.
        serde_json::to_value(GenerateContentResponse {
            candidates: vec![Candidate {
                content: Content {
                    role: "model".into(),
                    parts,
                },
                index: 0,
                finish_reason,
            }],
            usage_metadata,
            model_version: self.model.clone(),
            response_id: self.id.clone(),
        })
        .unwrap_or_else(
            |_| json!({"error":{"code":502,"status":"UNAVAILABLE","message":"Invalid response"}}),
        )
    }
    fn fail(&mut self, message: &'static str) -> Vec<Value> {
        self.terminal = true;
        self.error = Some(message);
        self.blocks.clear();
        self.tools.clear();
        self.parts.clear();
        vec![json!({"error":{"code":502,"status":"UNAVAILABLE","message":message}})]
    }
    fn observe_usage(&mut self, raw: &Value) {
        let usage = if raw["type"] == "message_start" {
            &raw["message"]["usage"]
        } else if raw["type"] == "message_delta" {
            &raw["usage"]
        } else {
            return;
        };
        self.input = self.input.max(usage["input_tokens"].as_u64());
        if raw["type"] != "message_start" {
            self.output = self.output.max(usage["output_tokens"].as_u64());
        }
        self.cache = self.cache.max(usage["cache_read_input_tokens"].as_u64());
        self.write = self
            .write
            .max(usage["cache_creation_input_tokens"].as_u64());
    }
    fn usage(&self) -> Option<Value> {
        if let Some(usage) = &self.usage_override {
            return usage
                .as_object()
                .filter(|m| !m.is_empty())
                .map(|_| usage.clone());
        }
        let mut usage = Map::new();
        let input = self.input.map(|n| {
            n.saturating_add(self.cache.unwrap_or_default())
                .saturating_add(self.write.unwrap_or_default())
        });
        if let Some(n) = input {
            usage.insert("promptTokenCount".into(), json!(n));
        }
        if let Some(n) = self.output {
            usage.insert("candidatesTokenCount".into(), json!(n));
        }
        if let Some(n) = self.cache {
            usage.insert("cachedContentTokenCount".into(), json!(n));
        }
        if let (Some(input), Some(output)) = (input, self.output) {
            usage.insert(
                "totalTokenCount".into(),
                json!(input.saturating_add(output)),
            );
        }
        (!usage.is_empty()).then_some(Value::Object(usage))
    }
}

pub fn frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

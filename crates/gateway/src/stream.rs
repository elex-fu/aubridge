//! Upstream SSE framing and message assembly.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::pin::Pin;

use axum::body::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use manifest::schema::WireProtocol;
use proto::{protocol::AnthropicEvent, sse};
use serde_json::Value;
use tokio::time::Instant;
use translate::{error_terminator, ChatToAnthropic, ResponsesToAnthropic};

const FRAME_LIMIT: usize = 2 * 1024 * 1024;
const WHOLE_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct ReportedUsage {
    pub model: Option<String>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_read_reported: bool,
    pub cache_write_reported: bool,
}

impl ReportedUsage {
    pub(crate) fn input_with_cache(&self, protocol: WireProtocol) -> Option<u64> {
        self.input.map(|input| {
            if protocol == WireProtocol::Anthropic {
                input
                    .saturating_add(self.cache_read)
                    .saturating_add(self.cache_write)
            } else {
                input
            }
        })
    }

    pub(crate) fn observe(&mut self, protocol: WireProtocol, event: &Value) {
        let (message, usage, input, output) = match protocol {
            WireProtocol::Chat => (event, &event["usage"], "prompt_tokens", "completion_tokens"),
            WireProtocol::Responses => (
                &event["response"],
                &event["response"]["usage"],
                "input_tokens",
                "output_tokens",
            ),
            WireProtocol::Anthropic => {
                let usage = match event["type"].as_str() {
                    Some("message_start") => &event["message"]["usage"],
                    Some("message_delta") => &event["usage"],
                    _ => return,
                };
                (&event["message"], usage, "input_tokens", "output_tokens")
            }
        };
        if let Some(model) = message["model"].as_str().filter(|model| !model.is_empty()) {
            self.model = Some(model.to_string());
        }
        // Counts remain unknown until the provider reports them. The translator's
        // wire-level default of zero must not be used for accounting.
        let count = |value: &Value| value.as_u64().filter(|n| i64::try_from(*n).is_ok());
        self.input = self.input.max(count(&usage[input]));
        if protocol != WireProtocol::Anthropic || event["type"] != "message_start" {
            self.output = self.output.max(count(&usage[output]));
        }
        let read = count(&usage["cache_read_input_tokens"])
            .or_else(|| count(&usage["input_tokens_details"]["cached_tokens"]))
            .or_else(|| count(&usage["prompt_tokens_details"]["cached_tokens"]));
        self.cache_read = self.cache_read.max(read.unwrap_or_default());
        self.cache_read_reported |= read.is_some();
        let nested_write = match protocol {
            WireProtocol::Chat => count(&usage["prompt_tokens_details"]["cache_write_tokens"]),
            WireProtocol::Responses => count(&usage["input_tokens_details"]["cache_write_tokens"]),
            WireProtocol::Anthropic => None,
        };
        let write = count(&usage["cache_creation_input_tokens"]).max(nested_write);
        self.cache_write = self.cache_write.max(write.unwrap_or_default());
        self.cache_write_reported |= write.is_some();
    }
}

struct FrameReader {
    source: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    buffer: Vec<u8>,
    scanned: usize,
}

impl FrameReader {
    async fn next(&mut self) -> Result<Option<Bytes>, &'static str> {
        loop {
            let start = self.scanned.saturating_sub(3);
            let end = self.buffer[start..]
                .iter()
                .enumerate()
                .find_map(|(offset, &byte)| {
                    if byte != b'\n' {
                        return None;
                    }
                    let offset = start + offset;
                    let after = &self.buffer[offset + 1..];
                    if after.starts_with(b"\n") {
                        Some(offset + 2)
                    } else if after.starts_with(b"\r\n") {
                        Some(offset + 3)
                    } else {
                        None
                    }
                });
            if let Some(end) = end {
                if end > FRAME_LIMIT {
                    return Err("upstream SSE frame too large");
                }
                self.scanned = 0;
                return Ok(Some(Bytes::from(
                    self.buffer.drain(..end).collect::<Vec<_>>(),
                )));
            }
            if self.buffer.len() > FRAME_LIMIT {
                return Err("upstream SSE frame too large");
            }
            self.scanned = self.buffer.len();
            match self.source.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(error)) if error.is_timeout() => return Err("upstream stream timed out"),
                Some(Err(_)) => return Err("upstream stream read failed"),
                None => {
                    let only_comments = std::str::from_utf8(&self.buffer).is_ok_and(|tail| {
                        tail.lines()
                            .all(|line| line.trim().is_empty() || line.starts_with(':'))
                    });
                    return if only_comments {
                        Ok(None)
                    } else {
                        Err("incomplete upstream SSE frame")
                    };
                }
            }
        }
    }
}

enum Translator {
    Native,
    NativeResponses,
    NativeChat {
        include_usage: bool,
        expected_choices: u64,
        finished: BTreeSet<u64>,
        eof_eligible: bool,
    },
    Chat(ChatToAnthropic),
    Responses(ResponsesToAnthropic),
}

impl Translator {
    fn new(protocol: WireProtocol, model: &str) -> Self {
        match protocol {
            WireProtocol::Anthropic => Self::Native,
            WireProtocol::Chat => Self::Chat(ChatToAnthropic::new(model.to_string())),
            WireProtocol::Responses => {
                Self::Responses(ResponsesToAnthropic::new(model.to_string()))
            }
        }
    }

    fn push(&mut self, event: Value) -> Vec<AnthropicEvent> {
        match self {
            Self::Chat(translator) => match serde_json::from_value(event) {
                Ok(chunk) => translator.push(&chunk),
                Err(_) => translator.push_bad_chunk("invalid Chat event"),
            },
            Self::Responses(translator) => match serde_json::from_value(event) {
                Ok(event) => translator.push(&event),
                Err(_) => translator.push_bad_event("invalid Responses event"),
            },
            Self::Native | Self::NativeChat { .. } | Self::NativeResponses => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<AnthropicEvent> {
        match self {
            Self::Chat(translator) => translator.finish(),
            Self::Responses(translator) => translator.finish(),
            Self::Native | Self::NativeChat { .. } | Self::NativeResponses => Vec::new(),
        }
    }
}

pub(crate) struct Frame {
    pub bytes: Bytes,
    pub event: Value,
    wire: WireProtocol,
}

impl Frame {
    fn chat_done(bytes: Bytes) -> Self {
        Self {
            bytes,
            event: serde_json::json!({"type":"message_stop"}),
            wire: WireProtocol::Chat,
        }
    }

    pub(crate) fn translated(event: AnthropicEvent) -> Self {
        let name = event.event_name();
        let mut event = serde_json::to_value(event).unwrap_or_default();
        if name == "message_start" {
            // SDKs build their streaming snapshot from this message envelope.
            let message = &mut event["message"];
            message["type"] = Value::String("message".into());
            message["content"] = Value::Array(Vec::new());
            message["stop_reason"] = Value::Null;
            message["stop_sequence"] = Value::Null;
        }
        let bytes = Bytes::from(sse::frame_event(name, &event.to_string()));
        Self {
            bytes,
            event,
            wire: WireProtocol::Anthropic,
        }
    }

    fn kind(&self) -> &str {
        self.event["type"].as_str().unwrap_or_default()
    }

    fn text_delta(&self) -> Option<&str> {
        if self.wire == WireProtocol::Chat {
            return self.event["choices"].as_array()?.iter().find_map(|choice| {
                let delta = &choice["delta"];
                delta["content"]
                    .as_str()
                    .or_else(|| delta["reasoning_content"].as_str())
            });
        }
        if self.wire == WireProtocol::Responses {
            return (self.kind() == "response.output_text.delta")
                .then(|| self.event["delta"].as_str())
                .flatten();
        }
        let delta = self.event.get("delta")?;
        match delta["type"].as_str()? {
            "text_delta" => delta["text"].as_str(),
            "thinking_delta" => delta["thinking"].as_str(),
            _ => None,
        }
    }

    fn has_content(&self, responses_client: bool) -> bool {
        if self.wire == WireProtocol::Chat {
            return chat_has_content(&self.event, "delta");
        }
        if self.wire == WireProtocol::Responses {
            return match self.kind() {
                "response.output_text.delta" | "response.refusal.delta" => {
                    nonblank(&self.event["delta"])
                }
                "response.output_item.added" | "response.output_item.done" => {
                    let item = &self.event["item"];
                    match item["type"].as_str() {
                        Some("function_call" | "custom_tool_call") => nonblank(&item["name"]),
                        Some("message") => item["content"].as_array().is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|part| nonblank(&part["text"]) || nonblank(&part["refusal"]))
                        }),
                        _ => false,
                    }
                }
                _ => false,
            };
        }
        match self.kind() {
            "content_block_start" => {
                let block = &self.event["content_block"];
                match block["type"].as_str() {
                    Some("text") => nonblank(&block["text"]),
                    Some("thinking") => !responses_client && nonblank(&block["thinking"]),
                    Some("tool_use") => nonblank(&block["name"]),
                    Some("server_tool_use") => !responses_client && nonblank(&block["name"]),
                    Some("redacted_thinking") => !responses_client && nonblank(&block["data"]),
                    _ => false,
                }
            }
            "content_block_delta" => {
                (!responses_client || self.event["delta"]["type"] == "text_delta")
                    && self
                        .text_delta()
                        .is_some_and(|text| !text.trim().is_empty())
            }
            "message_start" => self.event["message"]["content"]
                .as_array()
                .is_some_and(|content| !responses_client && !content.is_empty()),
            "message_delta" => self.event["delta"]["stop_reason"] == "refusal",
            _ => false,
        }
    }
}

fn nonblank(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.trim().is_empty())
}

fn chat_has_content(event: &Value, field: &str) -> bool {
    event["choices"].as_array().is_some_and(|choices| {
        choices.iter().any(|choice| {
            let message = &choice[field];
            ["content", "reasoning_content", "refusal"]
                .iter()
                .any(|key| nonblank(&message[*key]))
                || message["tool_calls"].as_array().is_some_and(|calls| {
                    calls.iter().any(|call| nonblank(&call["function"]["name"]))
                })
                || choice["finish_reason"] == "content_filter"
        })
    })
}

pub(crate) async fn read_limited(
    response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, &'static str> {
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| "stream-error")?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("reply-too-large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(bytes))
}

pub(crate) enum StartFailure {
    Stream(&'static str),
    EmbeddedHttp { status: u16, body: String },
}

impl From<&'static str> for StartFailure {
    fn from(reason: &'static str) -> Self {
        Self::Stream(reason)
    }
}

fn chat_error(event: &Value) -> Option<&Value> {
    event.get("error").filter(|error| !error.is_null())
}

fn chat_finish_failed(event: &Value) -> bool {
    event["choices"].as_array().is_some_and(|choices| {
        choices
            .iter()
            .any(|choice| choice["finish_reason"] == "error")
    })
}

fn embedded_http_failure(error: &Value) -> Option<StartFailure> {
    // A message may quote user input. Require an explicit structured signal
    // before treating a successful HTTP response as a typed failure.
    let status = ["code", "type"]
        .into_iter()
        .filter_map(|field| error[field].as_str())
        .find_map(|code| match code {
            "context_length_exceeded" | "context_window_exceeded" => Some(400),
            "invalid_api_key" | "authentication_error" => Some(401),
            "insufficient_balance" | "insufficient_credit" | "billing_hard_limit_reached" => {
                Some(402)
            }
            "rate_limit_error"
            | "rate_limit_exceeded"
            | "model_rate_limit_exceeded"
            | "account_rate_limit_exceeded"
            | "insufficient_quota"
            | "quota_exceeded"
            | "quota_exhausted"
            | "model_quota_exceeded"
            | "model_quota_exhausted"
            | "account_quota_exceeded"
            | "account_quota_exhausted" => Some(429),
            "overloaded_error" | "server_is_overloaded" => Some(529),
            _ => None,
        })
        .or_else(|| {
            ["status", "code"]
                .into_iter()
                .filter_map(|field| {
                    error[field]
                        .as_u64()
                        .or_else(|| error[field].as_str()?.parse().ok())
                })
                .find(|status| matches!(status, 401 | 402 | 429 | 529))
                .map(|status| status as u16)
        })?;
    Some(StartFailure::EmbeddedHttp {
        status,
        body: error.to_string(),
    })
}

pub(crate) async fn native_chat_json(
    response: reqwest::Response,
    usage: &mut ReportedUsage,
) -> Result<Bytes, StartFailure> {
    let bytes = read_limited(response, WHOLE_MESSAGE_LIMIT).await?;
    let event: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid-message")?;
    usage.observe(WireProtocol::Chat, &event);
    if let Some(error) = chat_error(&event) {
        return Err(embedded_http_failure(error).unwrap_or("empty-stream".into()));
    }
    if chat_finish_failed(&event) {
        return Err("stream-error".into());
    }
    if !chat_has_content(&event, "message") {
        return Err("empty-stream".into());
    }
    Ok(bytes)
}

pub(crate) struct Reply {
    cli_lease: Option<crate::claude_cli::Lease>,
    reader: FrameReader,
    protocol: WireProtocol,
    translator: Translator,
    pending: VecDeque<Frame>,
    detector: router::guard::TextLoopDetector,
    pub failed: bool,
    pub ended: bool,
    responses_client: bool,
    response_id: Option<String>,
    response_sequence: u64,
    response_context_exceeded: bool,
    embedded_failure: Option<StartFailure>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Reply {
    pub(crate) fn new(
        mut response: reqwest::Response,
        protocol: WireProtocol,
        model: &str,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        let cli_lease = response
            .extensions_mut()
            .remove::<crate::claude_cli::Lease>();
        Self {
            cli_lease,
            reader: FrameReader {
                source: Box::pin(response.bytes_stream()),
                buffer: Vec::new(),
                scanned: 0,
            },
            protocol,
            translator: Translator::new(protocol, model),
            pending: VecDeque::new(),
            detector: router::guard::TextLoopDetector::new(),
            failed: false,
            ended: false,
            responses_client: false,
            response_id: None,
            response_sequence: 0,
            response_context_exceeded: false,
            embedded_failure: None,
            _permit: permit,
        }
    }

    pub(crate) fn abort(&mut self) {
        self.failed = true;
        self.ended = true;
        self.pending.clear();
        self.reader.source = Box::pin(futures_util::stream::empty());
        self.reader.buffer.clear();
        self._permit.take();
        self.cli_lease.take();
    }

    pub(crate) fn cli_delivered(&self) {
        if let Some(lease) = &self.cli_lease {
            lease.delivered();
        }
    }

    pub(crate) fn native_chat(mut self, include_usage: bool, expected_choices: u64) -> Self {
        self.translator = Translator::NativeChat {
            include_usage,
            expected_choices,
            finished: BTreeSet::new(),
            eof_eligible: true,
        };
        self
    }

    pub(crate) fn for_responses(mut self) -> Self {
        self.responses_client = true;
        if self.protocol == WireProtocol::Responses {
            self.translator = Translator::NativeResponses;
        }
        self
    }

    pub(crate) fn take_embedded_failure(&mut self) -> Option<StartFailure> {
        self.embedded_failure.take()
    }

    pub(crate) async fn hold_first(
        &mut self,
        usage: &mut ReportedUsage,
        deadline: Instant,
    ) -> Result<VecDeque<Frame>, StartFailure> {
        tokio::time::timeout_at(deadline, async {
            let mut frames = VecDeque::new();
            let mut size = 0;
            while let Some(frame) = self.next(usage).await {
                if self.failed {
                    if self.response_context_exceeded {
                        return Err("context_length_exceeded".into());
                    }
                    return Err(self
                        .embedded_failure
                        .take()
                        .unwrap_or("stream-error".into()));
                }
                if self.ended {
                    return Err("empty-stream".into());
                }
                size += frame.bytes.len();
                if size > FRAME_LIMIT {
                    return Err("first-content-limit".into());
                }
                let has_content = frame.has_content(self.responses_client);
                frames.push_back(frame);
                if has_content {
                    return Ok(frames);
                }
            }
            Err("empty-stream".into())
        })
        .await
        .unwrap_or(Err("first-content-timeout".into()))
    }

    pub(crate) async fn next(&mut self, usage: &mut ReportedUsage) -> Option<Frame> {
        while !self.ended {
            if let Some(mut frame) = self.pending.pop_front() {
                match frame.kind() {
                    "error" => {
                        self.failed = true;
                        self.ended = true;
                        self.pending.clear();
                        // Some clients stop reading at the error event. Settle the
                        // upstream failure in the same chunk as its terminator.
                        if frame.wire == WireProtocol::Anthropic {
                            let stop = Frame::translated(AnthropicEvent::MessageStop);
                            let mut bytes =
                                Vec::with_capacity(frame.bytes.len() + stop.bytes.len());
                            bytes.extend_from_slice(&frame.bytes);
                            bytes.extend_from_slice(&stop.bytes);
                            frame.bytes = Bytes::from(bytes);
                        }
                    }
                    "message_stop" | "response.completed" | "response.incomplete" => {
                        self.ended = true
                    }
                    _ => {
                        if frame
                            .text_delta()
                            .is_some_and(|text| self.detector.push(text))
                        {
                            self.terminate("上游输出重复循环,已提前切断");
                        }
                    }
                }
                return Some(frame);
            }
            match self.reader.next().await {
                Ok(Some(bytes)) => self.push_frame(bytes, usage),
                Ok(None) => {
                    if matches!(
                        &self.translator,
                        Translator::NativeChat {
                            expected_choices, finished, eof_eligible: true, ..
                        } if *expected_choices > 0 && finished.len() as u64 == *expected_choices
                    ) {
                        self.pending.push_back(Frame::chat_done(Bytes::from_static(
                            crate::chat::DONE.as_bytes(),
                        )));
                        continue;
                    }
                    let events = self.translator.finish();
                    if events.is_empty() {
                        self.terminate("upstream stream ended without message_stop");
                    } else {
                        self.pending
                            .extend(events.into_iter().map(Frame::translated));
                    }
                }
                Err(message) => self.terminate(message),
            }
        }
        None
    }

    fn terminate(&mut self, message: &str) {
        self.pending.clear();
        let error = match self.translator {
            Translator::NativeChat { .. } => Frame {
                bytes: crate::chat::stream_error(),
                event: serde_json::json!({"type":"error"}),
                wire: WireProtocol::Chat,
            },
            Translator::NativeResponses => Frame {
                bytes: Bytes::from(crate::responses::stream_failure(
                    self.response_id
                        .as_deref()
                        .unwrap_or("resp_aubridge_failed"),
                    self.response_sequence,
                    self.response_context_exceeded,
                )),
                event: serde_json::json!({"type":"error"}),
                wire: WireProtocol::Responses,
            },
            _ => Frame::translated(error_terminator(message)),
        };
        self.pending.push_back(error);
    }

    fn push_frame(&mut self, bytes: Bytes, usage: &mut ReportedUsage) {
        let Ok(text) = std::str::from_utf8(&bytes) else {
            self.terminate("upstream SSE is not UTF-8");
            return;
        };
        let (mut events, _) = sse::parse(text);
        let native_responses = matches!(self.translator, Translator::NativeResponses);
        let native = native_responses || matches!(self.translator, Translator::Native);
        let Some(raw) = events.pop() else {
            if native || matches!(self.translator, Translator::NativeChat { .. }) {
                self.pending.push_back(Frame {
                    bytes,
                    event: Value::Null,
                    wire: self.protocol,
                });
            }
            return;
        };
        if !native && raw.data.trim() == "[DONE]" {
            if let Translator::NativeChat {
                finished,
                expected_choices,
                ..
            } = &self.translator
            {
                if finished.len() as u64 >= *expected_choices {
                    self.pending.push_back(Frame::chat_done(bytes));
                } else {
                    self.terminate("Upstream Chat ended without a finish reason");
                }
            } else {
                self.pending
                    .extend(self.translator.finish().into_iter().map(Frame::translated));
            }
            return;
        }
        let Ok(mut event) = serde_json::from_str::<Value>(&raw.data) else {
            self.terminate("upstream SSE contains invalid JSON");
            return;
        };
        let bytes = if self.protocol == WireProtocol::Responses
            && (event["type"] == "response.done"
                || (event.get("type").is_none() && raw.event.as_deref() == Some("response.done")))
        {
            let kind = match event["response"]["status"].as_str() {
                Some("completed") => "response.completed",
                Some("incomplete") => "response.incomplete",
                Some("failed" | "cancelled" | "canceled") => "response.failed",
                _ => {
                    self.terminate("upstream Responses ended without a terminal status");
                    return;
                }
            };
            event["type"] = Value::String(kind.into());
            Bytes::from(sse::frame_event(kind, &event.to_string()))
        } else {
            bytes
        };
        if native && event.get("type").is_none() {
            if let (Some(object), Some(name)) = (event.as_object_mut(), raw.event) {
                object.insert("type".into(), Value::String(name));
            }
        } else if matches!(self.translator, Translator::Responses(_)) {
            translate::responses::fill_responses_event_type(&mut event, raw.event.as_deref());
        }
        usage.observe(self.protocol, &event);
        if self.protocol == WireProtocol::Chat {
            if let Some(error) = chat_error(&event) {
                self.embedded_failure = embedded_http_failure(error);
                self.terminate("upstream Chat returned a stream error");
                return;
            }
            if chat_finish_failed(&event) {
                self.terminate("upstream Chat ended with an error");
                return;
            }
        }
        if self.protocol == WireProtocol::Anthropic && event["type"] == "error" {
            self.embedded_failure = embedded_http_failure(&event["error"]);
        }
        if self.protocol == WireProtocol::Responses {
            self.embedded_failure = match event["type"].as_str() {
                Some("response.failed") => embedded_http_failure(&event["response"]["error"])
                    .or_else(|| embedded_http_failure(&event["error"])),
                Some("error") => {
                    embedded_http_failure(&event["error"]).or_else(|| embedded_http_failure(&event))
                }
                _ => None,
            };
        }
        if native {
            if native_responses {
                if let Some(id) = event["response"]["id"].as_str() {
                    self.response_id = Some(id.into());
                }
                if let Some(sequence) = event["sequence_number"].as_u64() {
                    self.response_sequence = sequence.saturating_add(1);
                }
                if matches!(
                    event["type"].as_str(),
                    Some("error" | "response.failed" | "response.cancelled")
                ) {
                    let code = event["response"]["error"]["code"]
                        .as_str()
                        .or_else(|| event["code"].as_str());
                    self.response_context_exceeded = matches!(
                        code,
                        Some("context_length_exceeded" | "context_window_exceeded")
                    );
                    self.terminate("upstream Responses failed");
                    return;
                }
            }
            self.pending.push_back(Frame {
                bytes,
                event,
                wire: self.protocol,
            });
        } else if let Translator::NativeChat {
            include_usage,
            expected_choices,
            finished,
            eof_eligible,
        } = &mut self.translator
        {
            let Some(choices) = event["choices"].as_array() else {
                self.terminate("Invalid upstream Chat chunk");
                return;
            };
            for choice in choices {
                if nonblank(&choice["finish_reason"]) {
                    let index = choice["index"].as_u64();
                    // Keep legacy defaults for explicit [DONE], but infer EOF
                    // completion only from unambiguous finish identities.
                    *eof_eligible &= index.is_some_and(|index| index < *expected_choices);
                    finished.insert(index.unwrap_or_default());
                }
            }
            let bytes = if !*include_usage && !event["usage"].is_null() {
                if choices.is_empty() {
                    return;
                }
                event
                    .as_object_mut()
                    .expect("Chat chunk object")
                    .remove("usage");
                Bytes::from(format!("data: {event}\n\n"))
            } else {
                bytes
            };
            self.pending.push_back(Frame {
                bytes,
                event,
                wire: WireProtocol::Chat,
            });
        } else {
            self.pending.extend(
                self.translator
                    .push(event)
                    .into_iter()
                    .map(Frame::translated),
            );
        }
    }
}

pub(crate) async fn whole_message(
    mut held: VecDeque<Frame>,
    reply: &mut Reply,
    usage: &mut ReportedUsage,
) -> Result<Value, &'static str> {
    let mut message = WholeMessage::default();
    loop {
        let frame = match held.pop_front() {
            Some(frame) => frame,
            None => match reply.next(usage).await {
                Some(frame) => frame,
                None => break,
            },
        };
        if reply.failed {
            return Err("stream-error");
        }
        message.push(frame)?;
    }
    message.finish()
}

#[derive(Default)]
struct WholeMessage {
    message: Option<Value>,
    blocks: BTreeMap<usize, Value>,
    arguments: BTreeMap<usize, String>,
    size: usize,
}

impl WholeMessage {
    fn push(&mut self, frame: Frame) -> Result<(), &'static str> {
        self.size += frame.bytes.len();
        if self.size > WHOLE_MESSAGE_LIMIT {
            return Err("message-too-large");
        }
        let event = frame.event;
        match event["type"].as_str() {
            Some("message_start") => {
                let mut message = event["message"].clone();
                if !message.is_object() {
                    return Err("invalid-message");
                }
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    self.blocks
                        .extend(std::mem::take(content).into_iter().enumerate());
                }
                self.message = Some(message);
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                if !block.is_object() {
                    return Err("invalid-content-block");
                }
                self.blocks.insert(block_index(&event)?, block.clone());
            }
            Some("content_block_delta") => {
                self.block_delta(block_index(&event)?, &event["delta"])?;
            }
            Some("message_delta") => {
                let message = self.message.as_mut().ok_or("invalid-message")?;
                if let Some(delta) = event["delta"].as_object() {
                    for (key, value) in delta {
                        message[key] = value.clone();
                    }
                }
                if let Some(usage) = event["usage"].as_object() {
                    if !message["usage"].is_object() {
                        message["usage"] = serde_json::json!({});
                    }
                    for (key, value) in usage {
                        message["usage"][key] = value.clone();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn block_delta(&mut self, index: usize, delta: &Value) -> Result<(), &'static str> {
        let block = self.blocks.get_mut(&index).ok_or("invalid-content-block")?;
        let field = match delta["type"].as_str() {
            Some("text_delta") => "text",
            Some("thinking_delta") => "thinking",
            Some("signature_delta") => "signature",
            Some("input_json_delta") => {
                if let Some(part) = delta["partial_json"].as_str() {
                    self.arguments.entry(index).or_default().push_str(part);
                }
                return Ok(());
            }
            Some("citations_delta") => {
                if !block["citations"].is_array() {
                    block["citations"] = Value::Array(Vec::new());
                }
                if let Some(citations) = block["citations"].as_array_mut() {
                    citations.push(delta["citation"].clone());
                }
                return Ok(());
            }
            _ => return Err("unsupported-content-delta"),
        };
        if let Some(part) = delta[field].as_str() {
            if !block[field].is_string() {
                block[field] = Value::String(String::new());
            }
            if let Value::String(text) = &mut block[field] {
                text.push_str(part);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Value, &'static str> {
        let mut message = self.message.ok_or("invalid-message")?;
        for (index, args) in self.arguments {
            let block = self.blocks.get_mut(&index).ok_or("invalid-content-block")?;
            if !args.is_empty() {
                block["input"] = serde_json::from_str(&args).map_err(|_| "invalid-tool-input")?;
            }
        }
        message["type"] = Value::String("message".into());
        message["content"] = Value::Array(self.blocks.into_values().collect());
        if message.get("stop_sequence").is_none() {
            message["stop_sequence"] = Value::Null;
        }
        Ok(message)
    }
}

fn block_index(event: &Value) -> Result<usize, &'static str> {
    event["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or("invalid-content-block")
}

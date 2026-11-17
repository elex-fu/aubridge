//! Chat 解析側(OpenAI Chat Completions SSE → 语义增量,§6.2/§6.3)。
//!
//! Chat 流式分块逐个喂入 [`ChatToAnthropic`],由共享的
//! [`crate::emit::AnthropicEmitter`] 产出 Anthropic SSE 事件流。
//! 工具调用增量保序(F-1)、usage 透传(F-2)、finish_reason 显式映射
//! (F-4)、中途失败补完整 error 终止(F-5)。
//!
//! design: docs/product-design.md §6.1/§6.2/§6.3

use std::collections::{BTreeMap, HashSet};

use proto::protocol::*;
use proto::sse;

use crate::emit::AnthropicEmitter;

/// Chat finish_reason → Anthropic stop_reason。
/// 非常规原因显式映射到最接近的语义,**永不默认成 end_turn**(F-4;
/// magpie L-14:SAFETY 被当 stop,agent 收空内容还以为成功)。
pub fn map_finish_reason(finish: &str) -> &'static str {
    match finish {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" | "refusal" => "refusal",
        // 未知原因:宁可报 refusal(可被 agent 识别为异常),
        // 不可伪装成正常结束。
        _ => "refusal",
    }
}

/// 解析上游 Chat SSE 文本为分块序列。`data: [DONE]` 是流终止标记,
/// 不是坏 JSON(F-10);坏 JSON 记入报告,绝不 panic。
pub fn parse_chat_stream(input: &str) -> (Vec<ChatChunk>, sse::ParseReport) {
    let (events, mut report) = sse::parse(input);
    let mut chunks = Vec::new();
    for e in &events {
        if e.data.trim() == "[DONE]" {
            continue;
        }
        match serde_json::from_str::<ChatChunk>(&e.data) {
            Ok(c) => chunks.push(c),
            Err(_) => report.bad_json.push(e.data.clone()),
        }
    }
    (chunks, report)
}

/// 响应側翻译状态机(Chat 解析側)。
///
/// 契约(调用方保证):按到达顺序 `push` 全部分块,恰好一次 `finish`。
/// 上游若发出无法顺序表示的交错增量(如两个 tool_call 的参数交错),
/// 按 F-5 语义转 error 终止,不静默丢失。
#[derive(Debug)]
pub struct ChatToAnthropic {
    emit: AnthropicEmitter,
    /// chat tool_calls 索引 → 块状态(交错检测,F-1)。
    tool_slots: BTreeMap<usize, ToolSlot>,
    pending_tool: Option<PendingTool>,
    has_refusal: bool,
}

const PENDING_TOOL_BYTES: usize = 64 * 1024;
const PENDING_TOOL_FRAGMENTS: usize = 256;

#[derive(Debug)]
struct PendingTool {
    slot: usize,
    id: String,
    arguments: Vec<String>,
    bytes: usize,
    fragments: usize,
}

#[derive(Debug, Clone, Copy)]
enum ToolSlot {
    Open { index: usize },
    Closed,
}

impl ChatToAnthropic {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            emit: AnthropicEmitter::new(model),
            tool_slots: BTreeMap::new(),
            pending_tool: None,
            has_refusal: false,
        }
    }

    /// 喂入一个 Chat 分块,产出本分块贡献的 Anthropic 事件(顺序拼接)。
    pub fn push(&mut self, chunk: &ChatChunk) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        if self.emit.errored() {
            return out;
        }
        if let Some(u) = &chunk.usage {
            let cache_count = |key| {
                u.prompt_tokens_details[key]
                    .as_u64()
                    .filter(|count| i64::try_from(*count).is_ok())
            };
            self.emit.note_gross_usage(
                u.prompt_tokens,
                u.completion_tokens,
                cache_count("cached_tokens"),
                cache_count("cache_write_tokens"),
            );
        }
        self.emit.adopt_id(&chunk.id);
        self.emit.ensure_started(&mut out);

        for choice in &chunk.choices {
            if let Some(text) = choice.delta.content.as_deref() {
                self.push_text(text, &mut out);
                if self.emit.errored() {
                    return out;
                }
            }
            if let Some(text) = choice
                .delta
                .refusal
                .as_ref()
                .and_then(serde_json::Value::as_str)
            {
                let nonblank = !text.trim().is_empty();
                self.has_refusal |= nonblank;
                // A blank fragment alone must not interrupt tool arguments.
                if nonblank || self.open_tool_slot().is_none() {
                    self.push_text(text, &mut out);
                    if self.emit.errored() {
                        return out;
                    }
                }
            }
            for tc in &choice.delta.tool_calls {
                self.push_tool_call(tc, &mut out);
                if self.emit.errored() {
                    return out;
                }
            }
            if let Some(finish) = choice.finish_reason.as_deref() {
                if self.pending_tool.take().is_some() {
                    self.emit
                        .mark_error("tool call ended without a name", &mut out);
                    return out;
                }
                let stop = if finish == "stop" && self.has_refusal && self.tool_slots.is_empty() {
                    "refusal"
                } else {
                    map_finish_reason(finish)
                };
                self.emit.note_stop_reason(stop.to_string());
            }
        }
        out
    }

    /// 流末(语义全在 emitter:关块 + message_delta/usage + message_stop,
    /// 截断补 error 终止,F-2/F-5)。
    pub fn finish(&mut self) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        if self.pending_tool.take().is_some() && !self.emit.errored() {
            self.emit
                .mark_error("tool call ended without a name", &mut out);
        }
        self.emit.finish(&mut out);
        out
    }

    /// 喂入解析失败的原始分块(bad JSON):不 panic,按 F-5 语义
    /// 转成 error 终止事件。
    pub fn push_bad_chunk(&mut self, raw: &str) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        self.emit
            .mark_error(&format!("bad chunk from upstream: {raw}"), &mut out);
        out
    }

    // ------------------------------------------------------------------
    // 内部:槽位簿记(交错增量检测)
    // ------------------------------------------------------------------

    fn push_text(&mut self, text: &str, out: &mut Vec<AnthropicEvent>) {
        if !text.is_empty() && self.pending_tool.take().is_some() {
            self.emit.mark_error("text before tool call name", out);
            return;
        }
        let open_tool = self.open_tool_slot();
        self.emit.push_text(text, out);
        self.sync_slots_after_switch(open_tool);
    }

    fn push_tool_call(&mut self, tc: &ChatToolCallDelta, out: &mut Vec<AnthropicEvent>) {
        let slot = tc.index;
        let name = tc.function.as_ref().and_then(|f| f.name.as_deref());
        let args = tc.function.as_ref().and_then(|f| f.arguments.as_deref());
        if let Some(pending) = self.pending_tool.take() {
            self.push_pending_tool(pending, tc, name, args, out);
            return;
        }
        match self.tool_slots.get(&slot).copied() {
            None => {
                let Some(id) = tc.id.as_deref() else {
                    self.emit
                        .mark_error(&format!("tool call delta without id at index {slot}"), out);
                    return;
                };
                if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
                    let index = self.start_tool(slot, id, name, out);
                    self.emit
                        .push_tool_args(index, args.unwrap_or_default(), out);
                } else if id.trim().is_empty() {
                    self.emit
                        .mark_error(&format!("tool call delta without id at index {slot}"), out);
                } else {
                    let pending = PendingTool {
                        slot,
                        id: id.to_owned(),
                        arguments: Vec::new(),
                        bytes: 0,
                        fragments: 0,
                    };
                    self.push_pending_tool(pending, tc, name, args, out);
                }
            }
            Some(ToolSlot::Open { index }) => {
                if !self.emit.is_open_tool(index) {
                    // 交错增量无法用顺序块表示,显式失败优于静默乱序(F-1/F-5)。
                    self.emit
                        .mark_error("interleaved tool call deltas are not representable", out);
                    return;
                }
                self.emit
                    .push_tool_args(index, args.unwrap_or_default(), out);
            }
            Some(ToolSlot::Closed) => {
                self.emit
                    .mark_error("tool call delta after block closed (interleaved)", out);
            }
        }
    }

    fn push_pending_tool(
        &mut self,
        mut pending: PendingTool,
        tc: &ChatToolCallDelta,
        name: Option<&str>,
        args: Option<&str>,
        out: &mut Vec<AnthropicEvent>,
    ) {
        if pending.slot != tc.index || tc.id.as_ref().is_some_and(|id| id != &pending.id) {
            self.emit
                .mark_error("conflicting tool call before name", out);
            return;
        }
        let args = args.unwrap_or_default();
        if pending.fragments == PENDING_TOOL_FRAGMENTS
            || args.len() > PENDING_TOOL_BYTES - pending.bytes
        {
            self.emit
                .mark_error("pending tool call exceeds buffer limit", out);
            return;
        }
        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
            let index = self.start_tool(pending.slot, &pending.id, name, out);
            for fragment in pending.arguments {
                self.emit.push_tool_args(index, &fragment, out);
            }
            self.emit.push_tool_args(index, args, out);
        } else {
            pending.fragments += 1;
            pending.bytes += args.len();
            if !args.is_empty() {
                pending.arguments.push(args.to_owned());
            }
            self.pending_tool = Some(pending);
        }
    }

    fn start_tool(
        &mut self,
        slot: usize,
        id: &str,
        name: &str,
        out: &mut Vec<AnthropicEvent>,
    ) -> usize {
        let open_tool = self.open_tool_slot();
        let index = self.emit.begin_tool(id, name, out);
        self.tool_slots.insert(slot, ToolSlot::Open { index });
        self.sync_slots_after_switch(open_tool);
        index
    }

    /// 当前打开的工具槽位(块被 emitter 关掉时用于同步 Closed)。
    fn open_tool_slot(&self) -> Option<usize> {
        self.tool_slots
            .iter()
            .find_map(|(&slot, state)| matches!(state, ToolSlot::Open { .. }).then_some(slot))
    }

    /// emitter 切换块后:原打开槽位若不再打开则记 Closed。
    fn sync_slots_after_switch(&mut self, open_tool: Option<usize>) {
        let Some(slot) = open_tool else { return };
        if let Some(ToolSlot::Open { index }) = self.tool_slots.get(&slot) {
            if !self.emit.is_open_tool(*index) {
                self.tool_slots.insert(slot, ToolSlot::Closed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 请求側翻译
// ---------------------------------------------------------------------------

/// 请求側翻译:Anthropic Messages 请求体 → Chat 请求体(纯函数,
/// 不修改入参)。
///
/// - system(文本或块)→ 首条 system 消息;
/// - thinking 块**整块丢弃**(F-3:内容永不修改;Chat 上游不认
///   thinking,签名一并消失,后续多轮靠 F-7 整流兜底);
/// - assistant tool_use → tool_calls;user tool_result → tool 消息;
/// - **孤儿 tool_result(历史里无配对 tool_use)→ 丢弃**(F-8);
/// - tools/tool_choice/max_tokens 映射(F-11);
/// - 注入 `stream: true` + `include_usage: true`(F-2 前提)。
pub fn messages_request_to_chat(req: &MessagesRequest) -> ChatRequest {
    let mut messages: Vec<ChatMessage> = Vec::new();

    if let Some(system) = &req.system {
        let text = system.as_text();
        if !text.is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(text),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    let mut seen_tool_ids: HashSet<&str> = HashSet::new();
    for msg in &req.messages {
        match msg.role.as_str() {
            "assistant" => {
                translate_assistant(&msg.content, &mut seen_tool_ids, &mut messages);
            }
            "user" => {
                translate_user(&msg.content, &seen_tool_ids, &mut messages);
            }
            // 未知角色按纯文本透传(serde 从宽原则)。
            other => {
                let text = content_as_text(&msg.content);
                messages.push(ChatMessage {
                    role: other.to_string(),
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
    }

    ChatRequest {
        model: req.model.clone(),
        messages,
        max_tokens: if req.max_tokens > 0 {
            Some(req.max_tokens)
        } else {
            None
        },
        stream: Some(true),
        stream_options: Some(ChatStreamOptions {
            include_usage: true,
        }),
        tools: req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| ChatTool {
                    kind: "function".to_string(),
                    function: ChatFunctionSpec {
                        name: t.name.clone(),
                        description: if t.description.is_empty() {
                            None
                        } else {
                            Some(t.description.clone())
                        },
                        parameters: t.input_schema.clone(),
                    },
                })
                .collect()
        }),
        tool_choice: req.tool_choice.as_ref().map(map_tool_choice),
        parallel_tool_calls: req.parallel_tool_calls_override(),
    }
}

fn translate_assistant<'a>(
    content: &'a InboundContent,
    seen_tool_ids: &mut HashSet<&'a str>,
    out: &mut Vec<ChatMessage>,
) {
    let mut text = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    match content {
        InboundContent::Text(s) => text.push_str(s),
        InboundContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    InboundBlock::Text { text: t } => text.push_str(t),
                    // F-3:thinking 整块丢弃,内容与签名都不外泄。
                    InboundBlock::Thinking { .. } | InboundBlock::NativeOnly => {}
                    InboundBlock::ToolUse { id, name, input } => {
                        seen_tool_ids.insert(id);
                        tool_calls.push(ChatToolCall {
                            id: id.clone(),
                            kind: "function".to_string(),
                            function: ChatFunction {
                                name: name.clone(),
                                arguments: input.to_string(),
                            },
                        });
                    }
                    InboundBlock::ToolResult { .. } => {
                        // assistant 消息里的 tool_result 是畸形输入,
                        // 丢弃(serde 从宽,不 panic)。
                    }
                }
            }
        }
    }
    if !tool_calls.is_empty() || !text.is_empty() {
        out.push(ChatMessage {
            role: "assistant".to_string(),
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        });
    }
}

fn translate_user<'a>(
    content: &'a InboundContent,
    seen_tool_ids: &HashSet<&'a str>,
    out: &mut Vec<ChatMessage>,
) {
    let mut text = String::new();
    let mut tool_results: Vec<ChatMessage> = Vec::new();
    match content {
        InboundContent::Text(s) => text.push_str(s),
        InboundContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    InboundBlock::Text { text: t } => text.push_str(t),
                    InboundBlock::Thinking { .. } | InboundBlock::NativeOnly => {}
                    InboundBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        // F-8:孤儿 tool_result(无配对 tool_use)丢弃。
                        if !seen_tool_ids.contains(tool_use_id.as_str()) {
                            continue;
                        }
                        tool_results.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(content.as_text()),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                    InboundBlock::ToolUse { .. } => {
                        // user 消息里的 tool_use 是畸形输入,丢弃。
                    }
                }
            }
        }
    }
    out.extend(tool_results);
    if !text.is_empty() {
        out.push(ChatMessage {
            role: "user".to_string(),
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

fn content_as_text(content: &InboundContent) -> String {
    match content {
        InboundContent::Text(s) => s.clone(),
        InboundContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                InboundBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn map_tool_choice(tc: &ToolChoice) -> ChatToolChoice {
    match tc {
        ToolChoice::Auto { .. } => ChatToolChoice::Mode("auto".to_string()),
        ToolChoice::Any { .. } => ChatToolChoice::Mode("required".to_string()),
        ToolChoice::None => ChatToolChoice::Mode("none".to_string()),
        ToolChoice::Tool { name, .. } => ChatToolChoice::Tool {
            kind: "function".to_string(),
            function: ChatToolNameRef { name: name.clone() },
        },
    }
}

/// tool_choice 映射公开入口(F-11 用例直接断言)。
pub fn tool_choice_to_chat(tc: &ToolChoice) -> ChatToolChoice {
    map_tool_choice(tc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_tool_index_allocates_only_one_slot() {
        let chunks = parse_chat_stream(include_str!("../tests/fixtures/chat_tool_call.sse")).0;
        let mut translator = ChatToAnthropic::new("fixture");
        for mut chunk in chunks {
            for call in chunk
                .choices
                .iter_mut()
                .flat_map(|choice| &mut choice.delta.tool_calls)
            {
                call.index = usize::MAX;
            }
            translator.push(&chunk);
        }
        assert_eq!(translator.tool_slots.len(), 1);
        assert!(matches!(
            translator.finish().last(),
            Some(AnthropicEvent::MessageStop)
        ));
    }
}

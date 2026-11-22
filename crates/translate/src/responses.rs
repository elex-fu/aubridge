//! Responses 解析/生成側(codex 订阅上游方言,§6.3)。
//!
//! 生成側:`messages_request_to_responses` 把入面 Anthropic Messages
//! 请求体翻译成 Responses 请求体(强制 stream/store、system→instructions、
//! thinking→reasoning 回放项)。
//! 解析側:[`ResponsesToAnthropic`] 把 Responses SSE 事件流驱动成
//! Anthropic 事件流(复用共享 [`crate::emit::AnthropicEmitter`])。
//!
//! design: docs/product-design.md §6.3

use std::collections::{BTreeMap, HashSet};

use proto::protocol::*;
use proto::responses::*;
use proto::sse;
use serde_json::Value;

use crate::emit::AnthropicEmitter;

// ---------------------------------------------------------------------------
// 请求側翻译(Anthropic Messages → Responses)
// ---------------------------------------------------------------------------

/// 请求側翻译:Anthropic Messages 请求体 → Responses 请求体(纯函数,
/// 不修改入参)。
///
/// 强制变换(§6.3 上游硬约束,照抄 sub2api,非可选项):
/// - `stream: true`、`store: false`;
/// - system 搬进 `instructions`(端点拒 `role:"system"`),**不注入**
///   任何默认 prompt(实测留空亦 200;注入官方客户端文本边界不干净);
/// - thinking 块存在时加 `include: ["reasoning.encrypted_content"]`;
/// - assistant thinking(带签名)→ reasoning 回放项,签名原样透传(F-3);
/// - assistant tool_use → function_call;user tool_result →
///   function_call_output(孤儿丢弃,F-8);
/// - tools/tool_choice/max_tokens 映射。
pub fn messages_request_to_responses(req: &MessagesRequest) -> ResponsesRequest {
    let mut input: Vec<ResponsesInputItem> = Vec::new();
    let mut has_thinking = false;

    // system 只搬 instructions(§6.3),不进 input 序列。
    let instructions = req
        .system
        .as_ref()
        .map(|s| s.as_text())
        .filter(|s| !s.is_empty());

    let mut seen_tool_ids: HashSet<&str> = HashSet::new();
    for msg in &req.messages {
        let role = if msg.role == "assistant" {
            "assistant"
        } else {
            // 未知角色按 user 透传(sub2api 同款,serde 从宽)。
            "user"
        };
        let items = if role == "assistant" {
            translate_assistant(&msg.content, &mut seen_tool_ids, &mut has_thinking)
        } else {
            translate_user(&msg.content, &seen_tool_ids)
        };
        // system 语义已由 instructions 承载,这里不再区分 system 角色。
        input.extend(items);
    }

    ResponsesRequest {
        model: req.model.clone(),
        input,
        // 强制 true:chatgpt.com 端点只走 SSE(§6.3)。
        stream: true,
        // 强制 false:"Store must be set to false",否则上游报错(§6.3)。
        store: false,
        instructions,
        max_output_tokens: if req.max_tokens > 0 {
            // Responses 拒绝过小的 max_output_tokens(sub2api 下限 128),
            // 钳到下限,行为写死不做配置面。
            Some(req.max_tokens.max(128))
        } else {
            None
        },
        tools: req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| ResponsesTool {
                    kind: "function".to_string(),
                    name: t.name.clone(),
                    description: if t.description.is_empty() {
                        None
                    } else {
                        Some(t.description.clone())
                    },
                    parameters: t.input_schema.clone(),
                })
                .collect()
        }),
        tool_choice: req.tool_choice.as_ref().map(map_tool_choice),
        parallel_tool_calls: req.parallel_tool_calls_override(),
        reasoning: reasoning_effort(req),
        include: if has_thinking {
            Some(vec!["reasoning.encrypted_content".to_string()])
        } else {
            None
        },
    }
}

fn reasoning_effort(req: &MessagesRequest) -> Option<ResponsesReasoning> {
    let mode = req.thinking.as_ref()?.get("type")?.as_str()?;
    if !matches!(mode, "enabled" | "adaptive") {
        return None;
    }
    let effort = req.output_config.as_ref()?.get("effort")?.as_str()?;
    matches!(effort, "low" | "medium" | "high").then(|| ResponsesReasoning {
        effort: effort.to_string(),
    })
}

/// anthropic tool_choice → Responses tool_choice。
/// auto→auto、any→required、none→none、tool→具名 function(§6.3)。
pub fn tool_choice_to_responses(tc: &ToolChoice) -> ResponsesToolChoice {
    map_tool_choice(tc)
}

fn map_tool_choice(tc: &ToolChoice) -> ResponsesToolChoice {
    match tc {
        ToolChoice::Auto { .. } => ResponsesToolChoice::Mode("auto".to_string()),
        ToolChoice::Any { .. } => ResponsesToolChoice::Mode("required".to_string()),
        ToolChoice::None => ResponsesToolChoice::Mode("none".to_string()),
        ToolChoice::Tool { name, .. } => ResponsesToolChoice::Tool {
            kind: "function".to_string(),
            name: name.clone(),
        },
    }
}

/// assistant 消息 → Responses items。
/// 保序口径(sub2api):reasoning 回放项 → 文本消息 → function_call,
/// 与 Anthropic 单轮内的自然块序一致。
fn translate_assistant<'a>(
    content: &'a InboundContent,
    seen_tool_ids: &mut HashSet<&'a str>,
    has_thinking: &mut bool,
) -> Vec<ResponsesInputItem> {
    let mut items: Vec<ResponsesInputItem> = Vec::new();
    let mut text = String::new();
    let mut calls: Vec<ResponsesInputItem> = Vec::new();

    match content {
        InboundContent::Text(s) => text.push_str(s),
        InboundContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    InboundBlock::Text { text: t } => text.push_str(t),
                    InboundBlock::Thinking { signature, .. } => {
                        // F-3:带签名整块透传;无签名无法回放,整块丢弃。
                        // include 只在存在可回放 thinking 时注入——它的用途
                        // 就是拿回 encrypted_content 供多轮回放(§6.3)。
                        if !signature.is_empty() {
                            *has_thinking = true;
                            items.push(ResponsesInputItem::Reasoning {
                                encrypted_content: signature.clone(),
                            });
                        }
                    }
                    InboundBlock::ToolUse { id, name, input } => {
                        seen_tool_ids.insert(id);
                        calls.push(ResponsesInputItem::FunctionCall {
                            call_id: id.clone(),
                            name: name.clone(),
                            // 空入参按 "{}" 回放(上游拒裸 null 参数串)。
                            arguments: if input.is_null() {
                                "{}".to_string()
                            } else {
                                input.to_string()
                            },
                        });
                    }
                    // assistant 消息里的 tool_result 是畸形输入,丢弃。
                    InboundBlock::ToolResult { .. }
                    | InboundBlock::NativeOnly
                    | InboundBlock::Image { .. }
                    | InboundBlock::Document { .. } => {}
                }
            }
        }
    }

    if !text.is_empty() {
        items.push(ResponsesInputItem::Message {
            role: "assistant".to_string(),
            content: vec![ResponsesContentPart::OutputText { text }],
        });
    }
    items.extend(calls);
    items
}

/// user 消息 → Responses items。
/// 保序口径(sub2api):function_call_output 先于文本消息
/// (tool 结果紧跟上轮 function_call,缓存形状更好)。
fn translate_user(
    content: &InboundContent,
    seen_tool_ids: &HashSet<&str>,
) -> Vec<ResponsesInputItem> {
    let mut items: Vec<ResponsesInputItem> = Vec::new();
    let mut text = String::new();

    match content {
        InboundContent::Text(s) => text.push_str(s),
        InboundContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    InboundBlock::Text { text: t } => text.push_str(t),
                    InboundBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        // F-8:孤儿 tool_result(无配对 tool_use)丢弃。
                        if !seen_tool_ids.contains(tool_use_id.as_str()) {
                            continue;
                        }
                        items.push(ResponsesInputItem::FunctionCallOutput {
                            call_id: tool_use_id.clone(),
                            output: content.as_text(),
                        });
                    }
                    // user 消息里的 thinking/tool_use 是畸形输入:thinking
                    // 不回放(F-3 只回放 assistant 带签名块),tool_use 丢弃。
                    InboundBlock::Thinking { .. }
                    | InboundBlock::ToolUse { .. }
                    | InboundBlock::NativeOnly
                    | InboundBlock::Image { .. }
                    | InboundBlock::Document { .. } => {}
                }
            }
        }
    }

    if !text.is_empty() {
        items.push(ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![ResponsesContentPart::InputText { text }],
        });
    }
    items
}

// ---------------------------------------------------------------------------
// 事件側翻译(Responses SSE → Anthropic)
// ---------------------------------------------------------------------------

/// Use a supported Responses SSE label when the JSON object omits its type.
pub fn fill_responses_event_type(event: &mut Value, label: Option<&str>) -> bool {
    let Some(
        label @ ("response.created"
        | "response.output_item.added"
        | "response.output_item.done"
        | "response.output_text.delta"
        | "response.output_text.done"
        | "response.refusal.delta"
        | "response.content_part.added"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.completed"
        | "response.incomplete"
        | "response.failed"
        | "response.done"
        | "error"),
    ) = label
    else {
        return false;
    };
    let Some(object) = event.as_object_mut() else {
        return false;
    };
    if object.contains_key("type") {
        return false;
    }
    object.insert("type".into(), Value::String(label.into()));
    true
}

/// 解析上游 Responses SSE 文本为事件序列。Responses 协议没有 Chat 的
/// `[DONE]` 标记(终态由 response.completed/incomplete/failed 承载,
/// response.done 只是流层回执);个别兼容网关会追加 `[DONE]`,宽松跳过。
/// 坏 JSON 记入报告,绝不 panic(F-10)。
pub fn parse_responses_stream(input: &str) -> (Vec<ResponsesEvent>, sse::ParseReport) {
    let (events, mut report) = sse::parse(input);
    let mut out = Vec::new();
    for e in &events {
        if e.data.trim() == "[DONE]" {
            continue;
        }
        let parsed = serde_json::from_str::<ResponsesEvent>(&e.data)
            .ok()
            .or_else(|| {
                let mut value = serde_json::from_str(&e.data).ok()?;
                if !fill_responses_event_type(&mut value, e.event.as_deref()) {
                    return None;
                }
                serde_json::from_value(value).ok()
            });
        match parsed {
            Some(ev) => out.push(ev),
            None => report.bad_json.push(e.data.clone()),
        }
    }
    (out, report)
}

/// Responses incomplete 原因 → Anthropic stop_reason。
/// 非常规原因显式映射到最接近语义,**永不默认成 end_turn**(F-4)。
pub fn map_incomplete_reason(reason: &str) -> &'static str {
    match reason {
        "max_output_tokens" => "max_tokens",
        // content_filter 及未知原因:宁可报 refusal(可被 agent 识别为
        // 异常),不可伪装成正常结束。
        _ => "refusal",
    }
}

/// 响应側翻译状态机(Responses 解析側)。
///
/// 契约(调用方保证):按到达顺序 `push` 全部事件,恰好一次 `finish`。
/// 语义映射(sub2api `responses_to_anthropic.go` 口径):
/// - output_item.added:function_call/custom_tool_call → tool_use 块,
///   reasoning → thinking 块(encrypted_content 留作关块前签名),
///   message → 文本块懒开,长尾 item 类型跳过;
/// - output_text.delta → text_delta;reasoning(_summary)_text.delta →
///   thinking_delta;function_call_arguments.delta → input_json_delta;
/// - 交错参数增量无法用顺序块表示 → 显式 error 终止(F-1/F-5),
///   不静默丢弃;
/// - 终态 completed/incomplete/failed/顶层 error → 关块 + message_delta
///   + message_stop,usage 换算口径见 `note_body_usage`(§6.3)。
#[derive(Debug)]
pub struct ResponsesToAnthropic {
    emit: AnthropicEmitter,
    /// output_index → item 槽位(交错检测,F-1)。
    slots: BTreeMap<u64, ItemSlot>,
    active_message: Option<ActiveMessage>,
    /// 出现过 function_call(终态 stop_reason 取 tool_use)。
    has_tool_call: bool,
    has_refusal: bool,
    /// 拒答增量以及无对应 added 的工具/推理完成事件也阻止终态文本补齐。
    blocks_terminal_text_recovery: bool,
    /// 当前 reasoning 项的 encrypted_content(= anthropic thinking
    /// signature,关块前以 signature_delta 落地,F-3 原样透传)。
    pending_signature: String,
    /// 终态事件已到:此后事件静默,finish 无操作。
    terminated: bool,
}

#[derive(Debug)]
struct ActiveMessage {
    output_index: u64,
    item_id: String,
    content_index: u64,
    emitted: bool,
}

#[derive(Debug, Clone, Copy)]
enum ItemSlot {
    ToolOpen {
        index: usize,
        /// 已发送参数增量或一份完整参数。
        args_emitted: bool,
    },
    ThinkingOpen {
        index: usize,
    },
    /// message 项活跃:文本块由 emitter 懒开,项 done 时关闭。
    MessageOpen,
    Closed,
}

impl ResponsesToAnthropic {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            emit: AnthropicEmitter::new(model),
            slots: BTreeMap::new(),
            active_message: None,
            has_tool_call: false,
            has_refusal: false,
            blocks_terminal_text_recovery: false,
            pending_signature: String::new(),
            terminated: false,
        }
    }

    /// 喂入一个 Responses 事件,产出本事件贡献的 Anthropic 事件。
    pub fn push(&mut self, ev: &ResponsesEvent) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        if self.emit.errored() || self.terminated {
            return out;
        }
        match ev {
            ResponsesEvent::Created { response } => {
                self.note_body_usage(response);
                self.emit.adopt_id(&response.id);
                self.emit.ensure_started(&mut out);
            }
            ResponsesEvent::OutputItemAdded { output_index, item } => {
                self.emit.ensure_started(&mut out);
                self.item_added(*output_index, item, &mut out);
            }
            ResponsesEvent::ContentPartAdded {
                item_id,
                output_index,
                content_index,
            } => {
                self.active_part(item_id, *output_index, *content_index);
            }
            ResponsesEvent::OutputTextDelta {
                item_id,
                output_index,
                content_index,
                delta,
            }
            | ResponsesEvent::RefusalDelta {
                item_id,
                output_index,
                content_index,
                delta,
            } => {
                if matches!(ev, ResponsesEvent::RefusalDelta { .. }) {
                    self.blocks_terminal_text_recovery = true;
                    // A refusal stop can itself commit the stream, so blank deltas
                    // must not select it.
                    self.has_refusal |= !delta.trim().is_empty();
                }
                if let Some(part) = self.active_part(item_id, *output_index, *content_index) {
                    part.emitted |= !delta.is_empty();
                }
                self.emit.ensure_started(&mut out);
                self.emit.push_text(delta, &mut out);
                self.sync_slots();
            }
            ResponsesEvent::OutputTextDone {
                item_id,
                output_index,
                content_index,
                text,
            } => {
                let can_emit = !self.emit.has_open_non_text();
                if let Some(part) = self.active_part(item_id, *output_index, *content_index) {
                    if can_emit && !part.emitted && !text.is_empty() {
                        part.emitted = true;
                        self.emit.push_text(text, &mut out);
                    }
                }
            }
            ResponsesEvent::ReasoningSummaryTextDelta { delta, .. }
            | ResponsesEvent::ReasoningTextDelta { delta, .. } => {
                if !delta.is_empty() {
                    self.active_message = None;
                }
                self.emit.ensure_started(&mut out);
                self.emit.push_thinking(delta, &mut out);
                self.sync_slots();
            }
            ResponsesEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => {
                self.emit.ensure_started(&mut out);
                self.args_delta(*output_index, delta, &mut out);
            }
            ResponsesEvent::FunctionCallArgumentsDone {
                output_index,
                arguments,
                ..
            } => {
                self.blocks_terminal_text_recovery = true;
                self.emit.ensure_started(&mut out);
                self.args_done(*output_index, arguments, &mut out);
            }
            ResponsesEvent::OutputItemDone { output_index, item } => {
                self.blocks_terminal_text_recovery |= matches!(
                    item.kind.as_str(),
                    "function_call" | "custom_tool_call" | "reasoning"
                );
                self.item_done(*output_index, item, &mut out);
            }
            ResponsesEvent::Completed { response } => {
                self.note_body_usage(response);
                self.recover_terminal_text(response, &mut out);
                let stop = if self.has_tool_call {
                    "tool_use"
                } else if self.has_refusal {
                    "refusal"
                } else {
                    "end_turn"
                };
                self.finish_terminal(stop.to_string(), &mut out);
            }
            ResponsesEvent::Incomplete { response } => {
                self.note_body_usage(response);
                let reason = response
                    .incomplete_details
                    .as_ref()
                    .map(|d| map_incomplete_reason(&d.reason).to_string())
                    .unwrap_or_else(|| "refusal".to_string());
                self.finish_terminal(reason, &mut out);
            }
            ResponsesEvent::Failed { response } => {
                let msg = response
                    .error
                    .as_ref()
                    .map(|e| match &e.code {
                        Some(code) if !code.is_empty() => {
                            format!("upstream error {}: {}", code, e.message)
                        }
                        _ => format!("upstream error: {}", e.message),
                    })
                    .unwrap_or_else(|| "upstream response failed".to_string());
                self.terminated = true;
                self.emit.mark_error(&msg, &mut out);
                self.emit.finish(&mut out);
            }
            ResponsesEvent::Error { code, message } => {
                let msg = match code {
                    Some(code) if !code.is_empty() => {
                        format!("upstream error {}: {}", code, message)
                    }
                    _ => format!("upstream error: {}", message),
                };
                self.terminated = true;
                self.emit.mark_error(&msg, &mut out);
                self.emit.finish(&mut out);
            }
            // 流层回执:终态已由 completed/incomplete/failed 处理;
            // 若流终止而终态未到,finish() 的 F-5 兜底负责。
            ResponsesEvent::Done | ResponsesEvent::Unknown => {}
        }
        out
    }

    /// 流末:终态已到则无操作(事件已发完),否则 F-5 截断兜底。
    pub fn finish(&mut self) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        if !self.terminated {
            self.terminated = true;
            self.emit.finish(&mut out);
        }
        out
    }

    /// 喂入解析失败的原始事件(bad JSON):不 panic,按 F-5 语义转
    /// error 终止事件。
    pub fn push_bad_event(&mut self, raw: &str) -> Vec<AnthropicEvent> {
        let mut out = Vec::new();
        if !self.terminated {
            self.terminated = true;
            self.emit
                .mark_error(&format!("bad event from upstream: {raw}"), &mut out);
            self.emit.finish(&mut out);
        }
        out
    }

    // ------------------------------------------------------------------
    // 内部:item 生命周期与槽位簿记
    // ------------------------------------------------------------------

    fn active_part(
        &mut self,
        item_id: &str,
        output_index: u64,
        content_index: u64,
    ) -> Option<&mut ActiveMessage> {
        let message = self.active_message.as_mut()?;
        if message.output_index != output_index
            || (!item_id.is_empty() && !message.item_id.is_empty() && item_id != message.item_id)
            || content_index < message.content_index
        {
            return None;
        }
        // 顺序 part 只需记录当前位置,迟到的旧 part 不再有补齐资格。
        if content_index > message.content_index {
            message.content_index = content_index;
            message.emitted = false;
        }
        Some(message)
    }

    fn item_added(&mut self, oi: u64, item: &ResponsesItem, out: &mut Vec<AnthropicEvent>) {
        self.active_message = None;
        match item.kind.as_str() {
            "function_call" | "custom_tool_call" => {
                self.has_tool_call = true;
                // call_id 与 anthropic tool_use id 逐字一致(sub2api
                // toResponsesCallID 恒等映射);缺 call_id 退 item.id。
                let id = item
                    .call_id
                    .clone()
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| item.id.clone());
                let name = item.name.clone().unwrap_or_default();
                let index = self.emit.begin_tool(&id, &name, out);
                self.slots.insert(
                    oi,
                    ItemSlot::ToolOpen {
                        index,
                        args_emitted: false,
                    },
                );
                self.sync_slots();
            }
            "reasoning" => {
                if let Some(sig) = item.encrypted_content.as_deref() {
                    self.pending_signature = sig.to_string();
                }
                let index = self.emit.begin_thinking(out);
                self.slots.insert(oi, ItemSlot::ThinkingOpen { index });
                self.sync_slots();
            }
            "message" => {
                self.active_message = Some(ActiveMessage {
                    output_index: oi,
                    item_id: item.id.clone(),
                    content_index: 0,
                    emitted: false,
                });
                self.slots.insert(oi, ItemSlot::MessageOpen);
            }
            // 长尾 item 类型(web_search_call、local_shell_call…)不消费,
            // 不为其开块(最小集;真实语料需要再补)。
            _ => {}
        }
    }

    fn item_done(&mut self, oi: u64, item: &ResponsesItem, out: &mut Vec<AnthropicEvent>) {
        if self
            .active_message
            .as_ref()
            .is_some_and(|message| message.output_index == oi)
        {
            self.active_message = None;
        }
        if let Some(arguments) = item.arguments.as_deref() {
            self.args_done(oi, arguments, out);
        }
        let Some(slot) = self.slots.get_mut(&oi) else {
            return;
        };
        // reasoning 的签名 done 时才随项送达(比 added 更常见)。
        if item.kind == "reasoning" {
            if let Some(sig) = item.encrypted_content.as_deref().filter(|s| !s.is_empty()) {
                self.pending_signature = sig.to_string();
            }
        }
        match *slot {
            ItemSlot::ToolOpen { index, .. } => {
                if self.emit.is_open_tool(index) {
                    self.emit.close_open(out);
                }
                *slot = ItemSlot::Closed;
            }
            ItemSlot::ThinkingOpen { index } => {
                if self.emit.is_open_thinking(index) {
                    // 签名先于块关闭落地(Anthropic 客户端要求,
                    // 否则拒收 thinking 回放)。
                    let sig = self.pending_signature.clone();
                    self.emit.push_signature(index, &sig, out);
                    self.emit.close_open(out);
                }
                *slot = ItemSlot::Closed;
                self.pending_signature.clear();
            }
            ItemSlot::MessageOpen => {
                self.emit.close_open(out);
                *slot = ItemSlot::Closed;
            }
            ItemSlot::Closed => {}
        }
    }

    fn args_delta(&mut self, oi: u64, delta: &str, out: &mut Vec<AnthropicEvent>) {
        let Some(slot) = self.slots.get_mut(&oi) else {
            self.emit
                .mark_error("tool argument delta for an unknown output item", out);
            return;
        };
        match slot {
            ItemSlot::ToolOpen {
                index,
                args_emitted,
            } => {
                if !self.emit.is_open_tool(*index) {
                    // 交错增量无法用顺序块表示,显式失败优于静默乱序
                    // (F-1/F-5)。
                    self.emit.mark_error(
                        "interleaved tool argument deltas are not representable",
                        out,
                    );
                    return;
                }
                self.emit.push_tool_args(*index, delta, out);
                *args_emitted |= !delta.is_empty();
            }
            _ => {
                self.emit
                    .mark_error("tool argument delta for a block that is not open", out);
            }
        }
    }

    fn args_done(&mut self, oi: u64, arguments: &str, out: &mut Vec<AnthropicEvent>) {
        // 部分上游只在终态给出全量参数，同一个调用也可能有多份完整参数事件。
        let Some(ItemSlot::ToolOpen {
            index,
            args_emitted,
        }) = self.slots.get_mut(&oi)
        else {
            return;
        };
        if !*args_emitted && self.emit.is_open_tool(*index) && !arguments.is_empty() {
            self.emit.push_tool_args(*index, arguments, out);
            *args_emitted = true;
        }
    }

    fn recover_terminal_text(&mut self, response: &ResponsesBody, out: &mut Vec<AnthropicEvent>) {
        if response.status.as_deref() != Some("completed")
            || response.error.is_some()
            || response.incomplete_details.is_some()
            || self.emit.has_content_blocks()
            || self.blocks_terminal_text_recovery
        {
            return;
        }
        let Some(text) = terminal_text(&response.output) else {
            return;
        };
        if text.iter().any(|part| !part.is_empty()) {
            self.emit.adopt_id(&response.id);
            self.emit.ensure_started(out);
            for part in text {
                self.emit.push_text(part, out);
            }
        }
    }

    /// 终态:stop_reason + finish(completed 与 incomplete 共用)。
    fn finish_terminal(&mut self, stop_reason: String, out: &mut Vec<AnthropicEvent>) {
        self.emit.note_stop_reason(stop_reason);
        self.terminated = true;
        self.emit.finish(out);
    }

    /// usage 换算(§6.3):
    /// `input = responses.input − cached − cache_creation`(clamp ≥0),
    /// cached → cache_read_input_tokens,cache_creation 直传。
    fn note_body_usage(&mut self, body: &ResponsesBody) {
        if let Some(u) = &body.usage {
            let cached = u
                .input_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0);
            // cache creation 上游有两种别名:usage 顶层
            // cache_creation_input_tokens 与 details.cache_write_tokens
            // (真实 chatgpt.com 语料用后者)。二者只出现其一,取 max
            // 归一(sub2api 同款容错)。
            let creation = u
                .input_tokens_details
                .as_ref()
                .map(|d| d.cache_write_tokens)
                .unwrap_or(0)
                .max(u.cache_creation_input_tokens);
            let net = u
                .input_tokens
                .saturating_sub(cached)
                .saturating_sub(creation);
            self.emit.note_usage(net, u.output_tokens);
            self.emit.note_cache_usage(cached, creation);
        }
    }

    /// emitter 切换块后:不再打开的槽位记 Closed(交错序在此归档)。
    fn sync_slots(&mut self) {
        for slot in self.slots.values_mut() {
            match slot {
                ItemSlot::ToolOpen { index, .. } if !self.emit.is_open_tool(*index) => {
                    *slot = ItemSlot::Closed;
                }
                ItemSlot::ThinkingOpen { index } if !self.emit.is_open_thinking(*index) => {
                    *slot = ItemSlot::Closed;
                }
                _ => {}
            }
        }
    }
}

// Validate every item before emitting anything from a potentially mixed output.
fn terminal_text(output: &Value) -> Option<Vec<&str>> {
    let mut text = Vec::new();
    for item in output.as_array()? {
        if item.get("type")?.as_str()? != "message"
            || item.get("role")?.as_str()? != "assistant"
            || item
                .get("status")
                .is_some_and(|status| status.as_str() != Some("completed"))
        {
            return None;
        }
        for part in item.get("content")?.as_array()? {
            if part.get("type")?.as_str()? != "output_text" {
                return None;
            }
            text.push(part.get("text")?.as_str()?);
        }
    }
    Some(text)
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn done_only_text_recovers_removed_codex_delta() {
        // Fault injection on the captured SSE: drop only the text delta frame.
        let captured = include_str!("../tests/fixtures/codex_text.sse");
        let fixture = captured
            .split("\n\n")
            .filter(|frame| !frame.starts_with("event: response.output_text.delta\n"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (events, report) = parse_responses_stream(&fixture);
        assert!(!report.incomplete);
        assert!(report.bad_json.is_empty());
        let mut translator = ResponsesToAnthropic::new("fixture");
        let mut output: Vec<_> = events.iter().flat_map(|ev| translator.push(ev)).collect();
        output.extend(translator.finish());
        let text: Vec<_> = output
            .iter()
            .filter_map(|ev| match ev {
                AnthropicEvent::ContentBlockDelta {
                    index: 0,
                    delta: AnthropicDelta::TextDelta { text },
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, ["ok"]);
        assert!(matches!(
            output.as_slice(),
            [
                AnthropicEvent::MessageStart { .. },
                AnthropicEvent::ContentBlockStart { index: 0, .. },
                AnthropicEvent::ContentBlockDelta { index: 0, .. },
                AnthropicEvent::ContentBlockStop { index: 0 },
                AnthropicEvent::MessageDelta { delta, usage: Some(usage) },
                AnthropicEvent::MessageStop,
            ] if delta.stop_reason.as_deref() == Some("end_turn")
                && usage.input_tokens == 16 && usage.output_tokens == 5
        ));
    }

    #[test]
    fn output_text_done_respects_part_and_item_lifecycle() {
        let mut translator = ResponsesToAnthropic::new("fixture");
        for (wire, expected) in [
            (
                r#"{"type":"response.output_text.done","text":"unknown"}"#,
                None,
            ),
            (
                r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg_0"}}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.done","item_id":"wrong","text":"wrong"}"#,
                None,
            ),
            (r#"{"type":"response.output_text.delta","delta":""}"#, None),
            (r#"{"type":"response.output_text.done","text":""}"#, None),
            (
                r#"{"type":"response.output_text.done","text":"first"}"#,
                Some("first"),
            ),
            (
                r#"{"type":"response.output_text.done","text":"duplicate"}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.delta","content_index":1,"delta":"part"}"#,
                Some("part"),
            ),
            (
                r#"{"type":"response.output_text.done","content_index":1,"text":"partial"}"#,
                None,
            ),
            (
                r#"{"type":"response.content_part.added","content_index":3}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.done","content_index":2,"text":"late untouched part"}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.done","content_index":3,"text":"next"}"#,
                Some("next"),
            ),
            (
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.done","content_index":4,"text":"stale item"}"#,
                None,
            ),
            (
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}"#,
                None,
            ),
            (
                r#"{"type":"response.output_text.done","output_index":1,"text":"closed item"}"#,
                None,
            ),
            (
                r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"message"}}"#,
                None,
            ),
            (r#"{"type":"response.completed"}"#, None),
            (
                r#"{"type":"response.output_text.done","output_index":2,"text":"after terminal"}"#,
                None,
            ),
        ] {
            let event = serde_json::from_str(wire).unwrap();
            let out = translator.push(&event);
            let text: Vec<_> = out
                .iter()
                .filter_map(|event| match event {
                    AnthropicEvent::ContentBlockDelta {
                        delta: AnthropicDelta::TextDelta { text },
                        ..
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, expected.into_iter().collect::<Vec<_>>(), "{wire}");
            if matches!(event, ResponsesEvent::OutputTextDone { .. }) && expected.is_none() {
                assert!(out.is_empty(), "{wire}");
            }
        }
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn output_text_done_cannot_interrupt_tool_or_thinking() {
        for (kind, following_delta) in [
            ("function_call", "response.function_call_arguments.delta"),
            ("reasoning", "response.reasoning_text.delta"),
        ] {
            let mut translator = ResponsesToAnthropic::new("fixture");
            let mut push = |wire: &str| translator.push(&serde_json::from_str(wire).unwrap());
            push(r#"{"type":"response.output_item.added","item":{"type":"message"}}"#);
            push(&format!(
                r#"{{"type":"response.output_item.added","output_index":1,"item":{{"type":"{kind}"}}}}"#
            ));
            assert!(
                push(r#"{"type":"response.output_text.done","text":"stale message"}"#).is_empty()
            );
            push(
                r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"message"}}"#,
            );
            assert!(push(
                r#"{"type":"response.output_text.done","output_index":2,"text":"interleaved"}"#
            )
            .is_empty());
            assert!(matches!(
                push(&format!(
                    r#"{{"type":"{following_delta}","output_index":1,"delta":"{{}}"}}"#
                ))
                .as_slice(),
                [AnthropicEvent::ContentBlockDelta { index: 0, .. }]
            ));
        }
    }

    #[test]
    fn sparse_output_index_allocates_only_one_slot() {
        let fixture = include_str!("../tests/fixtures/codex_tool.sse").replace(
            "\"output_index\":0",
            &format!("\"output_index\":{}", u64::MAX),
        );
        let (events, report) = parse_responses_stream(&fixture);
        assert!(report.bad_json.is_empty());
        let mut translator = ResponsesToAnthropic::new("fixture");
        let output: Vec<_> = events
            .iter()
            .flat_map(|event| translator.push(event))
            .collect();
        assert_eq!(translator.slots.len(), 1);
        assert!(matches!(output.last(), Some(AnthropicEvent::MessageStop)));
        assert!(output.iter().any(|event| matches!(
            event,
            AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::InputJsonDelta { .. }
            }
        )));
        assert!(!output
            .iter()
            .any(|event| matches!(event, AnthropicEvent::Error { .. })));
    }

    /// 合成语料 v0(待真实 codex 语料替换):最简文本流 + usage 换算。
    #[test]
    fn minimal_text_stream() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"He\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"llo\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":20},\"cache_creation_input_tokens\":5}}}\n\n",
        );
        let (events, report) = parse_responses_stream(sse);
        assert!(!report.incomplete);
        assert!(report.bad_json.is_empty());
        assert_eq!(events.len(), 6);

        let mut sm = ResponsesToAnthropic::new("gpt-5.2-codex");
        let mut out = Vec::new();
        for ev in &events {
            out.extend(sm.push(ev));
        }
        out.extend(sm.finish());

        // message_start(id 采纳)→ text 块 → 2 delta → 块停 → message_delta
        // → message_stop;usage:100 − 20 − 5 = 75,cached/creation 直传。
        assert_eq!(out.len(), 7);
        assert!(matches!(&out[0], AnthropicEvent::MessageStart { message }
            if message.id == "msg_resp_1"));
        assert!(matches!(
            &out[1],
            AnthropicEvent::ContentBlockStart { index: 0, .. }
        ));
        assert!(
            matches!(&out[2], AnthropicEvent::ContentBlockDelta { delta: AnthropicDelta::TextDelta { text }, .. } if text == "He")
        );
        assert!(matches!(
            &out[4],
            AnthropicEvent::ContentBlockStop { index: 0 }
        ));
        match &out[5] {
            AnthropicEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                let usage = usage.as_ref().unwrap();
                assert_eq!(usage.input_tokens, 75);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.cache_read_input_tokens, Some(20));
                assert_eq!(usage.cache_creation_input_tokens, Some(5));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(out[6], AnthropicEvent::MessageStop));
    }

    /// 合成语料 v0:reasoning(签名在 done 送达)+ 工具调用 → thinking 块
    /// 带 signature_delta + tool_use 块 + stop_reason=tool_use。
    #[test]
    fn reasoning_and_tool_stream() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"think\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"SIG1\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"get_weather\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"city\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"Beijing\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"arguments\":\"{\\\"city\\\":\\\"Beijing\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":8}}}\n\n",
        );
        let (events, _) = parse_responses_stream(sse);
        let mut sm = ResponsesToAnthropic::new("m");
        let mut out = Vec::new();
        for ev in &events {
            out.extend(sm.push(ev));
        }
        out.extend(sm.finish());

        // thinking 块:开 → ThinkingDelta → SignatureDelta(SIG1)→ 停;
        // tool 块:开(call_1)→ 2×InputJsonDelta(严格保序)→ 停;
        // 终态 stop_reason=tool_use。
        let kinds: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
        assert_eq!(
            kinds,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(matches!(
            &out[1],
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlockStart::Thinking { .. },
                ..
            }
        ));
        assert!(
            matches!(&out[2], AnthropicEvent::ContentBlockDelta { delta: AnthropicDelta::ThinkingDelta { thinking }, .. } if thinking == "think")
        );
        assert!(
            matches!(&out[3], AnthropicEvent::ContentBlockDelta { delta: AnthropicDelta::SignatureDelta { signature }, .. } if signature == "SIG1")
        );
        assert!(
            matches!(&out[5], AnthropicEvent::ContentBlockStart { content_block: ContentBlockStart::ToolUse { id, name, .. }, .. } if id == "call_1" && name == "get_weather")
        );
        match (&out[6], &out[7]) {
            (
                AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::InputJsonDelta { partial_json: a },
                    ..
                },
                AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::InputJsonDelta { partial_json: b },
                    ..
                },
            ) => {
                assert_eq!(a, "{\"city\":");
                assert_eq!(b, "\"Beijing\"}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &out[9] {
            AnthropicEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 参数可以只随终态项送达，也可能经多个完整参数事件重复送达。
    #[test]
    fn tool_args_only_in_done() {
        let added = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c\",\"name\":\"f\"}}\n\n";
        let empty_delta = "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\"}\n\n";
        let args_done = "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"x\\\":1}\"}\n\n";
        let terminal = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"arguments\":\"{\\\"x\\\":1}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        for prior in [String::new(), empty_delta.into(), args_done.repeat(2)] {
            let sse = format!("{added}{prior}{terminal}");
            let (events, report) = parse_responses_stream(&sse);
            assert!(report.bad_json.is_empty());
            let mut sm = ResponsesToAnthropic::new("m");
            let mut out: Vec<_> = events.iter().flat_map(|event| sm.push(event)).collect();
            out.extend(sm.finish());
            assert!(
                matches!(
                    out.as_slice(),
                    [AnthropicEvent::MessageStart { .. },
                     AnthropicEvent::ContentBlockStart { index: 0, .. },
                     AnthropicEvent::ContentBlockDelta {
                         index: 0,
                         delta: AnthropicDelta::InputJsonDelta { partial_json },
                     },
                     AnthropicEvent::ContentBlockStop { index: 0 },
                     AnthropicEvent::MessageDelta { .. },
                     AnthropicEvent::MessageStop] if partial_json == "{\"x\":1}"
                ),
                "unexpected events after {prior:?}: {out:?}"
            );
        }
    }

    /// incomplete + max_output_tokens → max_tokens(F-4 显式映射);
    /// 未知 incomplete 原因 → refusal,不默认 end_turn。
    #[test]
    fn incomplete_reasons() {
        assert_eq!(map_incomplete_reason("max_output_tokens"), "max_tokens");
        assert_eq!(map_incomplete_reason("content_filter"), "refusal");
        assert_eq!(map_incomplete_reason("whatever_new"), "refusal");

        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        let (events, _) = parse_responses_stream(sse);
        let mut sm = ResponsesToAnthropic::new("m");
        let out = sm.push(&events[0]);
        assert!(
            matches!(&out[..], [AnthropicEvent::MessageStart { .. }, AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop] if delta.stop_reason.as_deref() == Some("max_tokens"))
        );
        // 终态后静默 + finish 无操作。
        assert!(sm.finish().is_empty());
    }

    /// failed / 顶层 error → 完整 error 终止(F-5,不裸断流);
    /// 未知事件类型从宽跳过。
    #[test]
    fn failed_and_unknown_events() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.web_search_call.in_progress\",\"output_index\":2}\n\n",
            "data: {\"type\":\"error\",\"code\":\"rate_limited\",\"message\":\"too many\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let (events, report) = parse_responses_stream(sse);
        assert!(report.bad_json.is_empty());
        let mut sm = ResponsesToAnthropic::new("m");
        let mut out = Vec::new();
        for ev in &events {
            out.extend(sm.push(ev));
        }
        out.extend(sm.finish());
        // error 终态后 completed 静默;序列 = start + error + stop(F-5)。
        assert!(
            matches!(&out[1], AnthropicEvent::Error { error } if error.message.contains("rate_limited"))
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(out[2], AnthropicEvent::MessageStop));
    }

    /// 流无终态(截断)→ finish 兜底补 error 终止(F-5)。
    #[test]
    fn truncated_stream_f5() {
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
        let (events, _) = parse_responses_stream(sse);
        let mut sm = ResponsesToAnthropic::new("m");
        let mut out = Vec::new();
        for ev in &events {
            out.extend(sm.push(ev));
        }
        out.extend(sm.finish());
        let names: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "error",
                "message_stop",
            ]
        );
    }

    /// 交错参数增量(块已被文本切换关闭)→ 显式 error(F-1/F-5)。
    #[test]
    fn interleaved_args_error() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c\",\"name\":\"f\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"interjected\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\"}\n\n",
        );
        let (events, _) = parse_responses_stream(sse);
        let mut sm = ResponsesToAnthropic::new("m");
        let mut out = Vec::new();
        for ev in &events {
            out.extend(sm.push(ev));
        }
        assert!(out
            .iter()
            .any(|e| matches!(e, AnthropicEvent::Error { .. })));
    }

    /// 坏 JSON 事件:解析报告记录,不 panic;经 push_bad_event 转 F-5。
    #[test]
    fn bad_json_stream() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
            "data: not-json\n\n",
        );
        let (_events, report) = parse_responses_stream(sse);
        assert_eq!(report.bad_json, vec!["not-json".to_string()]);
        let mut sm = ResponsesToAnthropic::new("m");
        let out = sm.push_bad_event("not-json");
        let names: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
        assert_eq!(names, vec!["message_start", "error", "message_stop"]);
    }
}

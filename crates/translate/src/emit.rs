//! Anthropic 生成側(IR → Anthropic SSE 事件,§6.2/§6.3)。
//!
//! 协议解析側(chat.rs / responses.rs)把上游方言解码成语义增量,
//! 由本发射器统一驱动 Anthropic 事件序列:块生命周期、message_start/
//! message_delta/message_stop 框架、usage 落地(F-2)、错误终止(F-5)。
//! 事件序列契约由 F 回归集锁定,本模块是各方言共享的唯一生成路径。
//!
//! design: docs/product-design.md §6.1/§6.2/§6.3

use proto::protocol::*;

/// 上游中途失败的规范终止事件(F-5;magpie L-19:裸断流会让 agent
/// 状态错乱)。任何 mid-stream 错误路径都必须以它收尾,不许直接关流。
pub fn error_terminator(message: &str) -> AnthropicEvent {
    AnthropicEvent::Error {
        error: AnthropicError {
            kind: "upstream_error".to_string(),
            message: message.to_string(),
        },
    }
}

#[derive(Debug)]
pub(super) struct OpenBlock {
    pub index: usize,
    pub kind: OpenKind,
}

#[derive(Debug)]
pub(super) enum OpenKind {
    Text,
    ToolUse,
    Thinking,
}

/// Anthropic 事件发射器(方言无关)。
///
/// 契约(解析側保证):按到达顺序喂增量,恰好一次 `finish`。
/// Anthropic 内容块是顺序协议(同一时刻至多一个块打开),解析側若
/// 遇到无法顺序表示的交错增量,应走 `mark_error`(F-5),不静默丢失。
#[derive(Debug)]
pub(super) struct AnthropicEmitter {
    model: String,
    message_id: String,
    started: bool,
    next_index: usize,
    open: Option<OpenBlock>,
    stop_reason: Option<String>,
    /// 已以 error 终止;此后增量静默、finish 只补 message_stop。
    errored: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    /// Chat reports gross input; Responses supplies already-net input.
    input_includes_cache: bool,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl AnthropicEmitter {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            message_id: "msg_aubridge".to_string(),
            started: false,
            next_index: 0,
            open: None,
            stop_reason: None,
            errored: None,
            input_tokens: 0,
            output_tokens: 0,
            input_includes_cache: false,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
    }

    pub fn errored(&self) -> bool {
        self.errored.is_some()
    }

    pub fn has_content_blocks(&self) -> bool {
        self.next_index != 0
    }

    pub fn has_open_non_text(&self) -> bool {
        matches!(
            &self.open,
            Some(block) if matches!(block.kind, OpenKind::ToolUse | OpenKind::Thinking)
        )
    }

    /// 流首 message_start(未发则发)。
    pub fn ensure_started(&mut self, out: &mut Vec<AnthropicEvent>) {
        if !self.started {
            out.push(self.message_start_event());
            self.started = true;
        }
    }

    /// 采纳上游响应 id(首个非空生效,与 Chat 行为一致)。
    pub fn adopt_id(&mut self, id: &str) {
        if !id.is_empty() && self.message_id == "msg_aubridge" {
            self.message_id = format!("msg_{id}");
        }
    }

    /// usage 全程 max 透传(F-2:magpie L-07,丢了记账就是 0)。
    pub fn note_usage(&mut self, input: u64, output: u64) {
        self.input_tokens = self.input_tokens.max(input);
        self.output_tokens = self.output_tokens.max(output);
    }

    pub fn note_gross_usage(
        &mut self,
        input: u64,
        output: u64,
        read: Option<u64>,
        creation: Option<u64>,
    ) {
        self.input_includes_cache = true;
        self.note_usage(input, output);
        self.cache_read_input_tokens = self.cache_read_input_tokens.max(read);
        self.cache_creation_input_tokens = self.cache_creation_input_tokens.max(creation);
    }

    pub fn note_stop_reason(&mut self, reason: String) {
        self.stop_reason = Some(reason);
    }

    /// Responses 缓存 usage(§6.3:input = input − cached −
    /// cache_creation 已由解析側换算,这里只落地缓存侧字段)。
    /// max 语义与 note_usage 一致:多份终态体取峰值。
    pub fn note_cache_usage(&mut self, read: u64, creation: u64) {
        self.cache_read_input_tokens = Some(self.cache_read_input_tokens.unwrap_or(0).max(read));
        self.cache_creation_input_tokens =
            Some(self.cache_creation_input_tokens.unwrap_or(0).max(creation));
    }

    /// 文本增量:确保文本块打开后发 TextDelta。
    pub fn push_text(&mut self, text: &str, out: &mut Vec<AnthropicEvent>) {
        if text.is_empty() {
            return;
        }
        self.ensure_text_open(out);
        if let Some(b) = &self.open {
            out.push(AnthropicEvent::ContentBlockDelta {
                index: b.index,
                delta: AnthropicDelta::TextDelta {
                    text: text.to_string(),
                },
            });
        }
    }

    /// 打开工具块(关掉当前块)。交错检测由解析側的槽位表负责。
    pub fn begin_tool(&mut self, id: &str, name: &str, out: &mut Vec<AnthropicEvent>) -> usize {
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock {
            index,
            kind: OpenKind::ToolUse,
        });
        out.push(AnthropicEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::Value::Null,
            },
        });
        index
    }

    /// 工具入参增量(F-1:严格按到达顺序)。
    pub fn push_tool_args(&mut self, index: usize, args: &str, out: &mut Vec<AnthropicEvent>) {
        if args.is_empty() {
            return;
        }
        out.push(AnthropicEvent::ContentBlockDelta {
            index,
            delta: AnthropicDelta::InputJsonDelta {
                partial_json: args.to_string(),
            },
        });
    }

    /// 当前打开块是否是指定 index 的工具块(交错检测用)。
    pub fn is_open_tool(&self, index: usize) -> bool {
        matches!(&self.open, Some(b) if b.index == index && matches!(b.kind, OpenKind::ToolUse))
    }

    /// 打开 thinking 块(关掉当前块)。空 thinking 起步,内容由
    /// ThinkingDelta 增量补(F-3:内容只透传,永不改写)。
    pub fn begin_thinking(&mut self, out: &mut Vec<AnthropicEvent>) -> usize {
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock {
            index,
            kind: OpenKind::Thinking,
        });
        out.push(AnthropicEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        });
        index
    }

    /// 思考文本增量:确保 thinking 块打开后发 ThinkingDelta。
    pub fn push_thinking(&mut self, text: &str, out: &mut Vec<AnthropicEvent>) {
        if text.is_empty() {
            return;
        }
        self.ensure_thinking_open(out);
        if let Some(b) = &self.open {
            out.push(AnthropicEvent::ContentBlockDelta {
                index: b.index,
                delta: AnthropicDelta::ThinkingDelta {
                    thinking: text.to_string(),
                },
            });
        }
    }

    /// 签名增量:关 thinking 块前落地(sub2api closeCurrentBlock 口径,
    /// Anthropic 客户端要求 signature 在块结束前送达,否则拒收回放)。
    pub fn push_signature(&mut self, index: usize, sig: &str, out: &mut Vec<AnthropicEvent>) {
        if sig.is_empty() {
            return;
        }
        out.push(AnthropicEvent::ContentBlockDelta {
            index,
            delta: AnthropicDelta::SignatureDelta {
                signature: sig.to_string(),
            },
        });
    }

    /// 当前打开块是否是指定 index 的 thinking 块。
    pub fn is_open_thinking(&self, index: usize) -> bool {
        matches!(&self.open, Some(b) if b.index == index && matches!(b.kind, OpenKind::Thinking))
    }

    /// 关闭当前打开块(若有)。
    pub fn close_open(&mut self, out: &mut Vec<AnthropicEvent>) {
        if let Some(block) = self.open.take() {
            out.push(AnthropicEvent::ContentBlockStop { index: block.index });
        }
    }

    /// 流末:关块 + message_delta(stop_reason 与 usage 在此落地,
    /// F-2)+ message_stop。流截断(无 stop_reason)= 中途失败,
    /// 补 error 终止(F-5)。
    pub fn finish(&mut self, out: &mut Vec<AnthropicEvent>) {
        if !self.started {
            out.push(self.message_start_event());
            self.started = true;
        }
        if self.errored.is_none() {
            self.close_open(out);
            if self.stop_reason.is_none() {
                out.push(error_terminator(
                    "upstream stream ended without finish_reason",
                ));
            } else {
                out.push(AnthropicEvent::MessageDelta {
                    delta: MessageDeltaBody {
                        stop_reason: self.stop_reason.clone(),
                        stop_sequence: None,
                    },
                    usage: Some(self.usage_snapshot()),
                });
            }
        }
        out.push(AnthropicEvent::MessageStop);
    }

    /// 标记流以 error 终止:确保 message_start 已发,块由 error 语义
    /// 终结,后续增量静默。
    pub fn mark_error(&mut self, msg: &str, out: &mut Vec<AnthropicEvent>) {
        if !self.started {
            out.push(self.message_start_event());
            self.started = true;
        }
        out.push(error_terminator(msg));
        self.open = None;
        self.errored = Some(msg.to_string());
    }

    fn ensure_thinking_open(&mut self, out: &mut Vec<AnthropicEvent>) {
        if let Some(b) = &self.open {
            if matches!(b.kind, OpenKind::Thinking) {
                return;
            }
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock {
            index,
            kind: OpenKind::Thinking,
        });
        out.push(AnthropicEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        });
    }

    fn ensure_text_open(&mut self, out: &mut Vec<AnthropicEvent>) {
        if let Some(b) = &self.open {
            if matches!(b.kind, OpenKind::Text) {
                return;
            }
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock {
            index,
            kind: OpenKind::Text,
        });
        out.push(AnthropicEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        });
    }

    fn message_start_event(&self) -> AnthropicEvent {
        let mut usage = self.usage_snapshot();
        usage.output_tokens = 1;
        if !self.input_includes_cache {
            usage.cache_read_input_tokens = None;
            usage.cache_creation_input_tokens = None;
        }
        AnthropicEvent::MessageStart {
            message: MessageStartBody {
                id: self.message_id.clone(),
                model: self.model.clone(),
                role: "assistant".to_string(),
                usage,
            },
        }
    }

    fn usage_snapshot(&self) -> Usage {
        let input_tokens = if self.input_includes_cache {
            self.input_tokens
                .saturating_sub(self.cache_read_input_tokens.unwrap_or_default())
                .saturating_sub(self.cache_creation_input_tokens.unwrap_or_default())
        } else {
            self.input_tokens
        };
        Usage {
            input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
        }
    }
}

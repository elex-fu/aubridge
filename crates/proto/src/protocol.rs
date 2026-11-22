//! 线协议类型:Anthropic Messages 与 OpenAI Chat Completions。
//!
//! serde 定义从宽(未知字段忽略、缺省字段 default),因为真实客户端的
//! 长尾字段永远比文档多——magpie LESSONS #823 的语料纪律决定了我们
//! 不能假设上游只发规范内字段。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Anthropic Messages(向 agent 暴露的协议)
// ---------------------------------------------------------------------------

/// Anthropic SSE 事件(数据行反序列化后的形状,`type` 字段做标签)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicEvent {
    /// 流首:envelope(id/model/role)+ 输入侧 usage。
    MessageStart {
        message: MessageStartBody,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    /// 流尾:`delta.stop_reason` 与累计 `usage` 在这里(F-2)。
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    /// 中途失败的规范终止事件(F-5)。
    Error {
        error: AnthropicError,
    },
}

impl AnthropicEvent {
    /// SSE `event:` 行的名称(与 serde tag 同名)。
    pub fn event_name(&self) -> &'static str {
        match self {
            AnthropicEvent::MessageStart { .. } => "message_start",
            AnthropicEvent::ContentBlockStart { .. } => "content_block_start",
            AnthropicEvent::ContentBlockDelta { .. } => "content_block_delta",
            AnthropicEvent::ContentBlockStop { .. } => "content_block_stop",
            AnthropicEvent::MessageDelta { .. } => "message_delta",
            AnthropicEvent::MessageStop => "message_stop",
            AnthropicEvent::Ping => "ping",
            AnthropicEvent::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStartBody {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default)]
    pub usage: Usage,
}

fn default_role() -> String {
    "assistant".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStart {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Responses 换算副产物(§6.3:cached → cache_read)。Chat 路恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Responses 换算副产物(§6.3:cache_creation)。Chat 路恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Anthropic Messages 请求(翻译的输入侧)
// ---------------------------------------------------------------------------

/// 工具定义(anthropic 侧 schema)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// anthropic `tool_choice`:`auto` / `any` / `none` / `tool`(具名)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    None,
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

/// 请求 `metadata`(§8.4:Claude Code 在此携带稳定 `user_id`,
/// 会话亲和 key 的来源之一)。serde 从宽:未知子字段忽略。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Anthropic Messages 请求体(翻译的输入侧)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    #[serde(default)]
    pub max_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RequestMetadata>,
    // Input-only hints; native Anthropic forwards the original body.
    #[serde(default, skip_serializing)]
    pub thinking: Option<serde_json::Value>,
    #[serde(default, skip_serializing)]
    pub output_config: Option<serde_json::Value>,
}

impl MessagesRequest {
    pub fn parallel_tool_calls_override(&self) -> Option<bool> {
        self.tools.as_ref().filter(|tools| !tools.is_empty())?;
        let disabled = match self.tool_choice.as_ref()? {
            ToolChoice::Auto {
                disable_parallel_tool_use,
            }
            | ToolChoice::Any {
                disable_parallel_tool_use,
            }
            | ToolChoice::Tool {
                disable_parallel_tool_use,
                ..
            } => *disable_parallel_tool_use,
            ToolChoice::None => None,
        };
        (disabled == Some(true)).then_some(false)
    }
}

/// `system` 既可以是纯文本也可以是块数组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<InboundTextBlock>),
}

impl SystemContent {
    /// 展开成纯文本(system 只承载文本语义)。
    pub fn as_text(&self) -> String {
        match self {
            SystemContent::Text(s) => s.clone(),
            SystemContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboundTextBlock {
    #[serde(rename = "type")]
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
}

/// 入站消息:`content` 既可以是字符串也可以是块数组(真实客户端两种都发)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboundMessage {
    pub role: String,
    pub content: InboundContent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InboundContent {
    Text(String),
    Blocks(Vec<InboundBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundBlock {
    Image {
        source: MediaSource,
    },
    Document {
        source: MediaSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Text {
        text: String,
    },
    /// agent 回传的思考块(带签名)。翻译到 Chat 上游时整块保留或
    /// 整块丢弃,内容永不修改(F-3 / magpie L-11)。
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// 原生路径保留请求原文。翻译路径不可选用含未知块的请求。
    #[serde(other)]
    NativeOnly,
}

/// Portable attachment source; opaque IDs remain tied to their original wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Text {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
    File {
        file_id: String,
    },
    #[serde(other)]
    Unsupported,
}

/// tool_result 的 content:字符串或块数组。缺省视为空文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<InboundBlock>),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        ToolResultContent::Text(String::new())
    }
}

impl ToolResultContent {
    /// Preserve the established text-only wire shape; media keeps ordered blocks.
    pub fn from_blocks(blocks: Vec<InboundBlock>) -> Self {
        if blocks
            .iter()
            .all(|block| matches!(block, InboundBlock::Text { .. }))
        {
            Self::Text(
                blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        InboundBlock::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            Self::Blocks(blocks)
        }
    }

    /// 展开成纯文本(Chat `tool` 消息只吃文本)。
    pub fn as_text(&self) -> String {
        match self {
            ToolResultContent::Text(s) => s.clone(),
            ToolResultContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    InboundBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions(上游侧协议)
// ---------------------------------------------------------------------------

/// Chat 流式分块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "object")]
    pub object: String,
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub delta: ChatDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFunctionDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default, skip_serializing)]
    pub prompt_tokens_details: serde_json::Value,
}

// ---------------------------------------------------------------------------
// OpenAI Chat 请求(翻译的输出侧)
// ---------------------------------------------------------------------------

/// Chat 请求体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// F-2 前提:注入 `include_usage`,否则流末拿不到 usage,
    /// 断言空转(engineering-constraints A-2)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<ChatStreamOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatStreamOptions {
    pub include_usage: bool,
}

/// Chat 非流式/请求侧消息(翻译输出侧)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFunction {
    pub name: String,
    pub arguments: String,
}

/// Chat 侧工具定义(function 型)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatFunctionSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFunctionSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// Chat `tool_choice`:模式字符串或具名 function 对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatToolChoice {
    Mode(String),
    Tool {
        #[serde(rename = "type")]
        kind: String,
        function: ChatToolNameRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolNameRef {
    pub name: String,
}

//! OpenAI Responses 线类型(codex 订阅上游方言,§6.3)。
//!
//! 只建模 aubridge 实际消费的字段;serde 从宽(未知字段忽略、缺省
//! default)——真实上游事件长尾字段永远比文档多(magpie #823)。
//! 请求侧类型是 translate 生成側的输出;事件侧类型是 Responses SSE
//! 解析側的输入。行为契约由 F-12 锁定,不在本 crate 断言。
//!
//! design: docs/product-design.md §6.3

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Responses 请求(翻译的输出側,发往 chatgpt.com/backend-api/codex/responses)
// ---------------------------------------------------------------------------

/// Responses 请求体。
///
/// `stream`/`store` 由 translate 生成側强制置 `true`/`false`(§6.3
/// 上游硬约束,非可选项),类型上仍如实携带字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<ResponsesInputItem>,
    /// system 消息的目标位置(端点拒 `role:"system"`,§6.3)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub stream: bool,
    pub store: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponsesToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
    /// thinking 块存在时注入 `["reasoning.encrypted_content"]`(§6.3)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    pub effort: String,
}

/// 请求 input 序列项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
    },
    /// assistant thinking 块的回放项:encrypted_content = anthropic
    /// signature,原样透传(F-3:整块保留或整块丢弃,内容永不修改)。
    /// 上游以此做多轮 reasoning 前缀复用,丢失只影响缓存命中不影响正确性。
    Reasoning {
        #[serde(default)]
        encrypted_content: String,
    },
    /// assistant 工具调用历史(对应 anthropic tool_use 块)。
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// user 工具结果(对应 anthropic tool_result 块)。
    FunctionCallOutput { call_id: String, output: String },
}

/// 消息内容分段(user 侧 input_text,assistant 侧 output_text)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesContentPart {
    InputText { text: String },
    OutputText { text: String },
}

/// Responses function 工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// Responses `tool_choice`:模式字符串或具名 function 对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesToolChoice {
    Mode(String),
    Tool {
        #[serde(rename = "type")]
        kind: String,
        name: String,
    },
}

// ---------------------------------------------------------------------------
// Responses SSE 事件(翻译的输入側)
// ---------------------------------------------------------------------------

/// Responses SSE 事件。`#[serde(other)]` 兜底:上游新增事件类型
/// (如各 `*.done` 前置确认、web_search 系列)一律落入 `Unknown`,
/// 解析側跳过,不算坏 JSON(serde 从宽)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesEvent {
    #[serde(rename = "response.created")]
    Created {
        #[serde(default)]
        response: ResponsesBody,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        item: ResponsesItem,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        item: ResponsesItem,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        content_index: u64,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        content_index: u64,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        content_index: u64,
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        content_index: u64,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        delta: String,
    },
    /// 原始推理文本增量(真实 Codex 客户端消费的通道,与 summary 同映射)。
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        delta: String,
    },
    /// 工具入参整体到达(部分流不增量发参,只在 done 给全量)。
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u64,
        #[serde(default)]
        arguments: String,
    },
    #[serde(rename = "response.completed")]
    Completed {
        #[serde(default)]
        response: ResponsesBody,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        #[serde(default)]
        response: ResponsesBody,
    },
    #[serde(rename = "response.failed")]
    Failed {
        #[serde(default)]
        response: ResponsesBody,
    },
    /// 流终止标记(事件层,与终态事件语义分离)。
    #[serde(rename = "response.done")]
    Done,
    /// 上游中途错误事件(F-5:必须转完整 error 终止,不裸断流)。
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: String,
    },
    /// 未知/不消费的事件类型(从宽跳过)。
    #[serde(other)]
    Unknown,
}

/// 事件里的 output item。松散结构(`kind` 字符串分派):item 类型
/// 长尾(web_search_call、local_shell_call…),非 message/function_call/
/// reasoning 的一律由解析側跳过,不为其建类型。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesItem {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub role: Option<String>,
    /// function_call 项的调用句柄(对应 anthropic tool_use id)。
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// reasoning 项的加密推理内容(= anthropic thinking signature,
    /// F-3 原样透传;added 与 done 都可能携带,done 更常见)。
    #[serde(default)]
    pub encrypted_content: Option<String>,
}

/// 终态/创建事件携带的响应体(松散投影:只取 aubridge 消费的字段)。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesBody {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
    #[serde(default)]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default)]
    pub error: Option<ResponsesError>,
    #[serde(default)]
    pub output: serde_json::Value,
}

/// Responses usage(sub2api 口径换算见 translate 解析側:
/// `input = responses.input − cached`,§6.3)。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub input_tokens_details: Option<ResponsesInputTokensDetails>,
    #[serde(default)]
    pub output_tokens_details: Option<ResponsesOutputTokensDetails>,
    /// 少数兼容上游直接在 usage 上报 cache_creation(§6.3 换算口径)。
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
    /// cache creation 的真实上游别名(实测 chatgpt.com 方言放在
    /// details 里而非 usage 顶层,§6.3 换算口径)。
    #[serde(default)]
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesOutputTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// incomplete 的原因(`max_output_tokens` 等映射 max_tokens 语义,F-4)。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesIncompleteDetails {
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ResponsesError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从宽纪律:未知事件类型 + 长尾字段不炸、缺字段走 default。
    #[test]
    fn responses_event_lenient_parse() {
        let ev: ResponsesEvent = serde_json::from_str(
            r#"{"type":"response.web_search_call.in_progress","output_index":3,"im_unknown":true}"#,
        )
        .unwrap();
        assert_eq!(ev, ResponsesEvent::Unknown);

        let ev: ResponsesEvent =
            serde_json::from_str(r#"{"type":"response.output_text.delta","delta":"hi"}"#).unwrap();
        match ev {
            ResponsesEvent::OutputTextDelta { delta, .. } => assert_eq!(delta, "hi"),
            other => panic!("unexpected: {other:?}"),
        }

        // 终态体缺 usage/error → default,不报错。
        let ev: ResponsesEvent =
            serde_json::from_str(r#"{"type":"response.completed","response":{"id":"r1"}}"#)
                .unwrap();
        match ev {
            ResponsesEvent::Completed { response } => {
                assert_eq!(response.id, "r1");
                assert_eq!(response.usage, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 请求序列化:强制字段如实落地,空 Option 不出现。
    #[test]
    fn responses_request_serializes_forced_fields() {
        let req = ResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![ResponsesContentPart::InputText {
                    text: "ping".to_string(),
                }],
            }],
            instructions: Some("be brief".to_string()),
            stream: true,
            store: false,
            max_output_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            reasoning: None,
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["store"], false);
        assert_eq!(json["instructions"], "be brief");
        assert_eq!(json["include"][0], "reasoning.encrypted_content");
        assert!(json.get("tools").is_none());
        assert_eq!(json["input"][0]["type"], "message");
        assert_eq!(json["input"][0]["content"][0]["type"], "input_text");
    }
}

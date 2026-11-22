//! translate —— 协议翻译层(§6.2 IR 组织:每协议"解析側/生成側")。
//!
//! - emit:Anthropic 生成側(共享事件发射器,各方言唯一出口);
//! - chat:Chat 解析側(OpenAI Chat Completions → Anthropic);
//! - Responses 解析/生成側(codex 订阅上游方言)随 §6.3 落地;
//! - common:跨方言请求側预处理;
//! - 硬规则与验收用例见 `docs/product-design.md` §6.1(F-1~F-12)。
//!
//! 入面支持 Messages 和 Chat，上游方言按源选择。

pub mod chat;
pub mod chat_input;
pub mod common;
pub mod count;
mod emit;
pub mod media;
pub mod responses;
pub mod responses_input;

pub use chat::{
    map_finish_reason, messages_request_to_chat, parse_chat_stream, tool_choice_to_chat,
    ChatToAnthropic,
};
pub use common::rectify_thinking_for_retry;
pub use emit::error_terminator;
pub use responses::{
    map_incomplete_reason, messages_request_to_responses, parse_responses_stream,
    tool_choice_to_responses, ResponsesToAnthropic,
};

pub mod gemini_input;
pub mod gemini_output;

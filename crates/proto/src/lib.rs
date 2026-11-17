//! proto —— 线协议类型与 SSE 编解码。
//!
//! 依赖铁律(docs/crate-layout.md):本 crate 永久只依赖 serde /
//! serde_json,不引任何运行时——协议层必须能在纯 CI 环境秒级检查。
//!
//! serde 从宽(未知字段忽略、缺省 default):真实客户端的长尾字段
//! 永远比文档多,magpie #823 的语料纪律决定了不能假设规范内字段集。

pub mod protocol;
pub mod responses;
pub mod sse;

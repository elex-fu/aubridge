//! count_tokens 双路径(F-6;magpie L-23:透传到不支持的上游会 500)。
//!
//! 上游原生支持(Anthropic)→ 透传;否则本地估算并标记 estimated。
//! 网关复用本地估算，把明显放不下输入的候选后置。

use proto::protocol::MessagesRequest;

pub enum CountOutcome {
    /// 上游原生支持 count_tokens,转发原始请求。
    Upstream,
    /// 本地估算(字节/4,magpie 兜底语义),标记 estimated。
    Estimated { tokens: usize },
}

/// 双路径分发。
pub fn count_tokens(upstream_supports: bool, req: &MessagesRequest) -> CountOutcome {
    if upstream_supports {
        CountOutcome::Upstream
    } else {
        CountOutcome::Estimated {
            tokens: estimate_tokens(req),
        }
    }
}

/// 本地估算:请求体序列化字节数 / 4。
pub fn estimate_tokens(req: &MessagesRequest) -> usize {
    estimate_serialized(req)
}

pub fn estimate_serialized(request: &impl serde::Serialize) -> usize {
    serde_json::to_vec(request)
        .map(|bytes| bytes.len() / 4)
        .unwrap_or_default()
}

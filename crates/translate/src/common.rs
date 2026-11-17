//! 跨方言共享的翻译工具(请求側预处理,F-7)。

use proto::protocol::*;

/// thinking 整流(F-7):上游报 thinking 签名/内容无效时,产出
/// 剥离全部 thinking 块的重试请求;原请求不动。已无 thinking 块时
/// 返回 None(重试无意义)。对 Chat 与 Responses 上游同样适用
/// (两边都不回传 thinking 块时)。
pub fn rectify_thinking_for_retry(req: &MessagesRequest) -> Option<MessagesRequest> {
    let mut had_thinking = false;
    let mut messages = req.messages.clone();
    for msg in &mut messages {
        if let InboundContent::Blocks(blocks) = &mut msg.content {
            let before = blocks.len();
            blocks.retain(|b| !matches!(b, InboundBlock::Thinking { .. }));
            if blocks.len() != before {
                had_thinking = true;
            }
        }
    }
    if !had_thinking {
        return None;
    }
    Some(MessagesRequest {
        messages,
        ..req.clone()
    })
}

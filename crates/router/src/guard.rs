//! 防卡死护栏:tool call 循环检测(设计文档 §8.5)。
//!
//! magpie 实战事故的直接产物——曾有 agent 半小时 308 次读同一文件
//! 烧掉 537 credits。规则:会话尾部同一 tool call(name + input)
//! 连续 8 轮(轮与轮之间只隔 tool_result)→ gateway 本地作答,
//! 不再发上游(免费额度产品的止损线)。纯函数,无状态。

use proto::protocol::{InboundBlock, InboundContent, InboundMessage, MessagesRequest};

/// 触发阈值(连续轮数;§8.5 定死,不做配置)。
const LOOP_ROUNDS: usize = 8;

/// —— 文本重复循环检测(§8.5 第二护栏)——
///
/// 流式输出侧:文本/reasoning 陷入逐字重复循环(模型退化)时提前
/// 切断,不烧到 max_tokens。纯函数,无状态。
///
/// 口径(定死,不做配置):
/// - 尾部按周期 p 逐字重复,且 p ≥ 8(排除 markdown 分隔线/空白
///   缩进/两词以内短循环的误伤面);
/// - 重复总量 ≥ max(5·p, 48) 字符(短周期要求更多遍);
/// - 只看最近 1,024 字符(检测窗口,内存有界)。
const MIN_PATTERN: usize = 8;
const MAX_PATTERN: usize = 128;
const MIN_REPEATS: usize = 5;
const MIN_REPEATED_CHARS: usize = 48;
const TAIL_CAP: usize = 1024;

/// 文本重复循环检测器:流式喂增量文本,命中返回 true。
#[derive(Default)]
pub struct TextLoopDetector {
    tail: String,
}

impl TextLoopDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段增量文本(delta 拼接后整体判周期,跨 chunk 边界的
    /// 重复照样命中)。命中即应切断流。
    pub fn push(&mut self, text: &str) -> bool {
        self.tail.push_str(text);
        if self.tail.chars().count() > TAIL_CAP {
            // 保尾部:char 边界裁剪(UTF-8 安全)。
            let keep: String = self
                .tail
                .chars()
                .skip(self.tail.chars().count() - TAIL_CAP)
                .collect();
            self.tail = keep;
        }
        repetitive_suffix(&self.tail)
    }
}

/// 尾部是否存在退化的逐字周期重复。升序找最小周期:纯分隔线/
/// 空白/两词短循环的最小周期 < 8,直接判非循环(其倍数周期虽也
/// 满足逐字相等,但语义上就是短周期重复)。
fn repetitive_suffix(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    if n < MIN_REPEATED_CHARS {
        return false;
    }
    let max_p = (n / MIN_REPEATS).min(MAX_PATTERN);
    for q in 1..=max_p {
        if has_period(&chars, q) {
            return q >= MIN_PATTERN;
        }
    }
    false
}

/// 末尾 need = max(5·p, 48) 个字符是否以周期 p 重复。
fn has_period(chars: &[char], p: usize) -> bool {
    let need = (MIN_REPEATS * p).max(MIN_REPEATED_CHARS);
    if chars.len() < need {
        return false;
    }
    let tail = &chars[chars.len() - need..];
    (p..need).all(|i| tail[i] == tail[i - p])
}

/// 会话尾部是否已陷入同一 tool call 循环。
pub fn tool_loop_detected(req: &MessagesRequest) -> bool {
    // 尾部必须是 tool_result:循环正在等下一轮——尾部是用户文本
    // (人已介入)或 assistant 文本(自然收尾)都不算。
    let Some(last) = req.messages.last() else {
        return false;
    };
    if !is_tool_result_message(last) {
        return false;
    }

    let mut run: (String, String) = (String::new(), String::new());
    let mut started = false;
    let mut count: usize = 0;
    'outer: for msg in req.messages.iter().rev() {
        let uses = assistant_tool_uses(msg);
        if uses.is_empty() {
            // user 的 tool_result 是轮间隔;其它消息(纯文本/assistant
            // 文本)打断循环。
            if is_tool_result_message(msg) {
                continue;
            }
            break;
        }
        for (name, input) in uses.iter().rev() {
            let sig = (name.clone(), input.clone());
            if !started {
                run = sig;
                started = true;
                count = 1;
            } else if run == sig {
                count += 1;
            } else {
                // 换了工具:循环结束。
                break 'outer;
            }
        }
    }
    count >= LOOP_ROUNDS
}

/// user 消息且含 tool_result 块。
fn is_tool_result_message(msg: &InboundMessage) -> bool {
    if msg.role != "user" {
        return false;
    }
    matches!(&msg.content, InboundContent::Blocks(blocks) if blocks
        .iter()
        .any(|b| matches!(b, InboundBlock::ToolResult { .. })))
}

/// assistant 消息里的 tool_use 签名(name, input 序列化)。
fn assistant_tool_uses(msg: &InboundMessage) -> Vec<(String, String)> {
    if msg.role != "assistant" {
        return Vec::new();
    }
    let InboundContent::Blocks(blocks) = &msg.content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| match b {
            InboundBlock::ToolUse { name, input, .. } => {
                // input 规范化:Value 相同即同调用;失败序列化按原文
                // 比较(None 桶合并,极端畸形不误伤)。
                let canonical = serde_json::to_string(input).unwrap_or_default();
                Some((name.clone(), canonical))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::protocol::{InboundBlock, InboundContent, InboundMessage};

    /// 造一轮循环:assistant(同一 tool call)→ user(tool_result)。
    fn round(name: &str, arg: &str) -> Vec<InboundMessage> {
        vec![
            InboundMessage {
                role: "assistant".into(),
                content: InboundContent::Blocks(vec![InboundBlock::ToolUse {
                    id: "tu-1".into(),
                    name: name.into(),
                    input: serde_json::json!({ "path": arg }),
                }]),
            },
            InboundMessage {
                role: "user".into(),
                content: InboundContent::Blocks(vec![InboundBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: Default::default(),
                    is_error: false,
                }]),
            },
        ]
    }

    fn req(messages: Vec<InboundMessage>) -> MessagesRequest {
        MessagesRequest {
            model: "m".into(),
            messages,
            system: None,
            max_tokens: 100,
            tools: None,
            tool_choice: None,
            stream: None,
            metadata: None,
            thinking: None,
            output_config: None,
        }
    }

    #[test]
    fn eight_identical_rounds_trigger_guard() {
        let mut msgs = Vec::new();
        for _ in 0..8 {
            msgs.extend(round("Read", "/tmp/a.txt"));
        }
        assert!(tool_loop_detected(&req(msgs)));
    }

    #[test]
    fn seven_rounds_do_not_trigger() {
        let mut msgs = Vec::new();
        for _ in 0..7 {
            msgs.extend(round("Read", "/tmp/a.txt"));
        }
        assert!(!tool_loop_detected(&req(msgs)));
    }

    #[test]
    fn different_tool_breaks_run() {
        let mut msgs = Vec::new();
        for _ in 0..6 {
            msgs.extend(round("Read", "/tmp/a.txt"));
        }
        // 中途换过工具:尾部 run 只有 2 轮。
        msgs.extend(round("Grep", "pattern"));
        msgs.extend(round("Read", "/tmp/a.txt"));
        msgs.extend(round("Read", "/tmp/a.txt"));
        assert!(!tool_loop_detected(&req(msgs)));
    }

    #[test]
    fn different_input_breaks_run() {
        let mut msgs = Vec::new();
        // 每轮读不同文件:不是循环,是正常多文件读取。
        for i in 0..8 {
            msgs.extend(round("Read", &format!("/tmp/{i}.txt")));
        }
        assert!(!tool_loop_detected(&req(msgs)));
    }

    #[test]
    fn user_text_tail_is_not_loop() {
        let mut msgs = Vec::new();
        for _ in 0..8 {
            msgs.extend(round("Read", "/tmp/a.txt"));
        }
        // 人已介入(尾部是用户文本):放行。
        msgs.push(InboundMessage {
            role: "user".into(),
            content: InboundContent::Text("换个思路".into()),
        });
        assert!(!tool_loop_detected(&req(msgs)));
    }

    #[test]
    fn empty_conversation_is_not_loop() {
        assert!(!tool_loop_detected(&req(Vec::new())));
    }

    #[test]
    fn repeated_phrase_triggers() {
        let mut d = TextLoopDetector::new();
        // 10 字符短语,5 遍(50 字符)达到阈值:先不足,后命中。
        let phrase = "重复的退化输出内容,";
        assert!(!d.push(&phrase.repeat(3)));
        assert!(d.push(&phrase.repeat(3)));
    }

    #[test]
    fn repetition_across_chunk_boundaries_triggers() {
        // delta 拆碎喂:周期检测作用于拼接后的尾部,任何单个 delta
        // 都看不出循环,拼起来命中。
        let mut d = TextLoopDetector::new();
        let phrase = "same phrase again ";
        let mut hit = false;
        for _ in 0..40 {
            if d.push(phrase) {
                hit = true;
                break;
            }
        }
        assert!(hit, "拼接后的循环应命中");
    }

    #[test]
    fn normal_prose_does_not_trigger() {
        let mut d = TextLoopDetector::new();
        let text = "先读文件,再改代码,然后跑测试。测试失败时先看报错,再定位,最后修复。";
        assert!(!d.push(text));
        assert!(!d.push("接着提交,清理临时代码,更新文档。"));
    }

    #[test]
    fn markdown_separator_and_whitespace_do_not_trigger() {
        let mut d = TextLoopDetector::new();
        // p < 8:分隔线/空白缩进永不命中(误伤面排除)。
        assert!(!d.push(&"-".repeat(200)));
        assert!(!d.push(&" ".repeat(300)));
        assert!(!d.push(&"\n\n\n\n".repeat(50)));
    }

    #[test]
    fn short_word_loop_below_pattern_floor_does_not_trigger() {
        // "the the the …"(p=4 < 8):不在 v1 检测范围(短循环误伤
        // 面大,句级循环才是烧 token 主战场;§8.5 文档口径)。
        let mut d = TextLoopDetector::new();
        assert!(!d.push(&"the ".repeat(100)));
    }

    #[test]
    fn detector_window_is_bounded() {
        let mut d = TextLoopDetector::new();
        // 正常长文 → 窗口只留尾部,不会因长前缀误判/内存膨胀。
        let filler = "正常文本内容。".repeat(300);
        assert!(!d.push(&filler));
        // 窗口裁剪后仍能检测尾部新出现的循环。
        assert!(d.push(&"退化重复的句子内容出现了".repeat(10)));
    }
}

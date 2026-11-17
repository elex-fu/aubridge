//! SSE 编解码层。
//!
//! 对应 F-9(注入的保活注释不得产生伪事件、注释后不写空行)与
//! F-10(空 body / 截断 body 不 panic)。解析器从宽:无法反序列化为
//! JSON 的事件记入 errors 而不是 panic——真实上游的截断流是常态
//! (magpie #823)。

/// 解析报告:流是否完整、坏事件清单。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParseReport {
    /// 流末尾没有以空行终止(截断)。
    pub incomplete: bool,
    /// data 行存在但不是合法 JSON 的下标。
    pub bad_json: Vec<String>,
}

/// 解析结果:原始事件(event 名 + data 原文)。
#[derive(Debug, Clone, PartialEq)]
pub struct RawSseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 帧化:`event:` + `data:` + 结尾空行。data 会按行拆开多 `data:` 行
/// (SSE 规范语义,合并由解析侧负责)。
pub fn frame_event(event: &str, data: &str) -> String {
    let mut out = String::new();
    if !event.is_empty() {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
    }
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// 保活注释:**后面不写空行**——openai-go v2 / Crush 把注释后的空行
/// 当事件边界处理会崩(magpie `sse.go` 实战坑,F-9)。
pub fn frame_keepalive() -> String {
    ": keepalive\n".to_string()
}

/// 把 Anthropic 事件序列帧化为 SSE 文本(`event:` 名取自事件的
/// serde tag)。gateway 输出路径与 smoke 共用。
pub fn frame_anthropic(events: &[crate::protocol::AnthropicEvent]) -> String {
    let mut out = String::new();
    for e in events {
        let data = serde_json::to_string(e).unwrap_or_default();
        out.push_str(&frame_event(e.event_name(), &data));
    }
    out
}

/// 解析一段(可能不完整的)SSE 文本。逐行扫描,注释行忽略,
/// 空行终结当前事件;多行 data 以 `\n` 合并(SSE 规范)。
pub fn parse(input: &str) -> (Vec<RawSseEvent>, ParseReport) {
    let mut events = Vec::new();
    let mut report = ParseReport::default();
    let (mut cur_event, mut cur_data) = (None, Vec::<String>::new());
    let mut saw_any_line = false;

    for line in input.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        saw_any_line = true;
        if line.is_empty() {
            // 无 data 的控制帧也要清除标签；显式空 data 仍是数据事件。
            let event = cur_event.take();
            if !cur_data.is_empty() {
                events.push(RawSseEvent {
                    event,
                    data: cur_data.join("\n"),
                });
                cur_data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            // 注释:活性信号,不是事件,不重置当前缓冲(F-9)。
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => cur_event = Some(value.to_string()),
            "data" => cur_data.push(value.to_string()),
            _ => {} // id / retry 等字段忽略
        }
    }

    // 流末尾:有未终结的缓冲 → 截断(F-10)。
    if saw_any_line && (cur_event.is_some() || !cur_data.is_empty()) {
        report.incomplete = true;
        events.push(RawSseEvent {
            event: cur_event.take(),
            data: cur_data.join("\n"),
        });
    }

    (events, report)
}

/// 解析并反序列化 data 为 `T`;坏 JSON 记入报告而不是 panic。
pub fn parse_json<T: serde::de::DeserializeOwned>(
    events: &[RawSseEvent],
    report: &mut ParseReport,
) -> Vec<Option<T>> {
    events
        .iter()
        .map(|e| match serde_json::from_str::<T>(&e.data) {
            Ok(v) => Some(v),
            Err(_) => {
                report.bad_json.push(e.data.clone());
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let framed = frame_event("content_block_delta", r#"{"a":1}"#);
        let (events, report) = parse(&framed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(events[0].data, r#"{"a":1}"#);
        assert!(!report.incomplete);
    }

    #[test]
    fn multi_line_data_merges() {
        let (events, _) = parse("data: a\ndata: b\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn complete_sse_frames_require_a_data_field_and_reset_the_event_label() {
        let stream = concat!(
            "\n: keepalive\n\nid: 1\nretry: 2000\n\n",
            "event: ping\n\nevent: error\n\ndata: {}\n\n",
            "event: labeled\ndata: {\ndata: }\n\n",
            "event: empty\ndata:\n\ndata: \n\ndata\n\n",
        );
        let expected = [
            (None, "{}"),
            (Some("labeled"), "{\n}"),
            (Some("empty"), ""),
            (None, ""),
            (None, ""),
        ];
        for input in [stream.to_owned(), stream.replace('\n', "\r\n")] {
            let (events, mut report) = parse(&input);
            let actual: Vec<_> = events
                .iter()
                .map(|event| (event.event.as_deref(), event.data.as_str()))
                .collect();
            assert_eq!(actual, expected);
            assert!(!report.incomplete);
            let parsed = parse_json::<serde_json::Value>(&events, &mut report);
            assert_eq!(parsed.iter().flatten().count(), 2);
            assert_eq!(report.bad_json, ["", "", ""]);
        }
        for input in ["event: ping", "event: ping\r\ndata: {}"] {
            let (_, report) = parse(input);
            assert!(report.incomplete, "{input:?}");
        }
    }
}

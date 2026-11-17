//! F-1~F-11 翻译保真回归集(docs/product-design.md §6.1)。
//!
//! 语料纪律:fixtures 为 M0 手工构造(文件头 synthetic 标注),
//! M1 建录制器后逐个用真实录制语料替换(magpie #823:原始字节,
//! 不自造)。断言方式:golden 事件序列(内联精确相等)防漂移 +
//! F 用例结构断言(保序/存在性)可读,两者并用。

use proto::protocol::*;
use proto::sse;
use translate::count::{count_tokens, CountOutcome};
use translate::*;

const CHAT_TOOL_CALL: &str = include_str!("fixtures/chat_tool_call.sse");
const CHAT_REFUSAL: &str = include_str!("fixtures/chat_refusal.sse");
const CHAT_TRUNCATED: &str = include_str!("fixtures/chat_truncated.sse");
const CODEX_TEXT: &str = include_str!("fixtures/codex_text.sse");
const CODEX_TOOL: &str = include_str!("fixtures/codex_tool.sse");
const CODEX_REASONING: &str = include_str!("fixtures/codex_reasoning.sse");

/// 跑完整流:解析 → 逐块喂入 → finish。
fn run_stream(sse_text: &str) -> Vec<AnthropicEvent> {
    let (chunks, report) = parse_chat_stream(sse_text);
    assert!(
        report.bad_json.is_empty(),
        "bad json: {:?}",
        report.bad_json
    );
    let mut tr = ChatToAnthropic::new("glm-4.5-flash");
    let mut events = Vec::new();
    for c in &chunks {
        events.extend(tr.push(c));
    }
    events.extend(tr.finish());
    events
}

fn req(json: &str) -> MessagesRequest {
    serde_json::from_str(json).expect("fixture request json")
}

// ---------------------------------------------------------------------------
// F-1 tool_use 增量保序(magpie L-04:乱序致 agent 死循环)
// ---------------------------------------------------------------------------

#[test]
fn f01_tool_call_delta_order() {
    let events = run_stream(CHAT_TOOL_CALL);

    // 块顺序协议:文本块 0 先开,工具块 1 后开。
    let starts: Vec<(usize, &ContentBlockStart)> = events
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => Some((*index, content_block)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2, "two content blocks expected");
    assert_eq!(starts[0].0, 0);
    assert!(matches!(starts[0].1, ContentBlockStart::Text { .. }));
    assert_eq!(starts[1].0, 1);
    assert!(matches!(starts[1].1, ContentBlockStart::ToolUse { .. }));

    // F-1 核心:input_json_delta 严格按上游顺序。
    let json_deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockDelta {
                index: 1,
                delta: AnthropicDelta::InputJsonDelta { partial_json },
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(json_deltas, vec!["{\"ci", "ty\":\"Beijing\"", "}"]);
    // 增量合并后是完整合法 JSON。
    let joined: String = json_deltas.concat();
    let v: serde_json::Value = serde_json::from_str(&joined).expect("joined json");
    assert_eq!(v["city"], "Beijing");

    // 文本块先闭合,工具块后闭合。
    let stops: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockStop { index } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(stops, vec![0, 1]);
}

#[test]
fn chat_tool_name_arrives_after_id_and_early_arguments() {
    // Derive a synthetic late-name stream from the existing Chat tool replay.
    // Keep the original call ID, argument bytes, stop reason and usage.
    let late_name = CHAT_TOOL_CALL
        .replacen(
            r#""type":"function","function":{"name":"get_weather","arguments":""}"#,
            r#""type":"function""#,
            1,
        )
        .replacen(
            r#""function":{"arguments":"ty\":\"Beijing\""}"#,
            r#""id":"call_w_1","function":{"name":"get_weather","arguments":"ty\":\"Beijing\""}"#,
            1,
        );
    assert_eq!(late_name.matches(r#""name":"get_weather""#).count(), 1);
    assert_eq!(run_stream(&late_name), run_stream(CHAT_TOOL_CALL));

    let (chunks, report) = parse_chat_stream(&late_name);
    assert!(report.bad_json.is_empty());
    let mut translator = ChatToAnthropic::new("glm-4.5-flash");
    for chunk in &chunks[..5] {
        assert!(translator.push(chunk).iter().all(|event| !matches!(
            event,
            AnthropicEvent::ContentBlockStart {
                content_block: ContentBlockStart::ToolUse { .. },
                ..
            } | AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::InputJsonDelta { .. },
                ..
            }
        )));
    }
}

#[test]
fn chat_tool_name_pending_rejects_incomplete_or_out_of_order_calls() {
    use serde_json::{json, Value};

    let tool = |delta: Value| json!({"choices":[{"delta":{"tool_calls":[delta]}}]});
    let pending = tool(json!({"index":0,"id":"call_1","function":{"name":"","arguments":"{"}}));
    let tail = |delta: Value| vec![pending.clone(), delta];
    let text = json!({"choices":[{"delta":{"content":"already sent"}}]});
    for chunks in [
        vec![tool(
            json!({"index":0,"function":{"name":"fixture","arguments":"{}"}}),
        )],
        tail(tool(
            json!({"index":0,"id":"other_call","function":{"name":"fixture"}}),
        )),
        tail(tool(
            json!({"index":1,"id":"call_2","function":{"name":"fixture"}}),
        )),
        tail(json!({"choices":[{"delta":{"content":"must not overtake tool"}}]})),
        tail(json!({"choices":[{"finish_reason":"tool_calls"}]})),
        vec![text, pending.clone()],
    ] {
        let stream: String = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect();
        let out = run_stream(&stream);
        assert!(matches!(
            out.as_slice(),
            [
                ..,
                AnthropicEvent::Error { .. },
                AnthropicEvent::MessageStop
            ]
        ));
        assert_eq!(
            out.iter()
                .filter(|event| matches!(event, AnthropicEvent::Error { .. }))
                .count(),
            1
        );
        assert!(out.iter().all(|event| !matches!(
            event,
            AnthropicEvent::MessageDelta { .. }
                | AnthropicEvent::ContentBlockStart {
                    content_block: ContentBlockStart::ToolUse { .. },
                    ..
                }
                | AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::InputJsonDelta { .. },
                    ..
                }
        )));
    }
}

#[test]
fn chat_tool_name_pending_limits_count_utf8_bytes_and_empty_fragments() {
    use serde_json::json;

    let repeat = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1"}]}}]}"#;
    let named = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"fixture"}}]},"finish_reason":"tool_calls"}]}"#;
    for (letters, repeats, accepted) in [
        (32767, 254, true), // 64 KiB including quotes; 256 fragments including ID and name.
        (32768, 0, false),
        (0, 255, false),
    ] {
        let args = format!("\"{}\"", "é".repeat(letters));
        let first = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"arguments":args}}
        ]}}]});
        let repeated = format!("{repeat}\n\n").repeat(repeats);
        let stream = format!("data: {first}\n\n{repeated}{named}\n\n");
        let out = run_stream(&stream);
        if accepted {
            assert!(matches!(
                out.as_slice(),
                [
                    AnthropicEvent::MessageStart { .. },
                    AnthropicEvent::ContentBlockStart {
                        content_block: ContentBlockStart::ToolUse { id, name, .. },
                        ..
                    },
                    AnthropicEvent::ContentBlockDelta {
                        delta: AnthropicDelta::InputJsonDelta { partial_json },
                        ..
                    },
                    AnthropicEvent::ContentBlockStop { .. },
                    AnthropicEvent::MessageDelta { delta, .. },
                    AnthropicEvent::MessageStop,
                ] if id == "call_1" && name == "fixture" && partial_json == &args
                    && delta.stop_reason.as_deref() == Some("tool_use")
            ));
        } else {
            assert!(matches!(
                out.as_slice(),
                [
                    AnthropicEvent::MessageStart { .. },
                    AnthropicEvent::Error { .. },
                    AnthropicEvent::MessageStop,
                ]
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// F-2 usage 全程透传(magpie L-07:丢了记账就是 0)
// ---------------------------------------------------------------------------

#[test]
fn f02_usage_passthrough() {
    let events = run_stream(CHAT_TOOL_CALL);
    let usage = events.iter().find_map(|e| match e {
        AnthropicEvent::MessageDelta { usage, .. } => usage.clone(),
        _ => None,
    });
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 25,
            output_tokens: 30,
            ..Default::default()
        })
    );
    // message_stop 必须收尾。
    assert!(matches!(events.last(), Some(AnthropicEvent::MessageStop)));
}

#[test]
fn chat_usage_preserves_cache_split_and_recomputes_net_input() {
    use serde_json::json;

    let content = r#"data: {"id":"chatcmpl_usage","choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#;
    let gross = json!({"prompt_tokens":100,"completion_tokens":7});
    let split = json!({
        "prompt_tokens":100,"completion_tokens":7,
        "prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":10}
    });
    for (snapshots, expected) in [
        (vec![split.clone()], (30, 7, Some(60), Some(10))),
        (
            vec![
                gross.clone(),
                split.clone(),
                split.clone(),
                json!({"prompt_tokens":90,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":2}}),
            ],
            (30, 7, Some(60), Some(10)),
        ),
        (
            vec![
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":60}}),
            ],
            (40, 7, Some(60), None),
        ),
        (
            vec![
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cache_write_tokens":0}}),
            ],
            (100, 7, None, Some(0)),
        ),
        (
            vec![
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":95,"cache_write_tokens":10}}),
            ],
            (0, 7, Some(95), Some(10)),
        ),
        (
            vec![
                gross,
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":[]}),
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":-1,"cache_write_tokens":1.5}}),
                json!({"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":"60","cache_write_tokens":null}}),
            ],
            (100, 7, None, None),
        ),
    ] {
        let mut stream = format!("{content}\n\n");
        for usage in &snapshots {
            let chunk = json!({"choices":[],"usage":usage});
            stream.push_str(&format!("data: {chunk}\n\n"));
        }
        stream.push_str("data: [DONE]\n\n");
        let out = run_stream(&stream);
        let [.., AnthropicEvent::MessageDelta {
            delta,
            usage: Some(usage),
        }, AnthropicEvent::MessageStop] = out.as_slice()
        else {
            panic!("unexpected terminal events: {out:?}");
        };
        assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            ),
            expected,
            "{snapshots:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// F-3 thinking 块完整性(magpie L-11:签名丢失 → 上游 400)
// ---------------------------------------------------------------------------

const REQ_WITH_THINKING: &str = r#"{
  "model": "glm-4.5-flash", "max_tokens": 100,
  "messages": [
    {"role": "user", "content": [{"type": "text", "text": "hi"}]},
    {"role": "assistant", "content": [
      {"type": "thinking", "thinking": "secret-thought", "signature": "sig123"},
      {"type": "text", "text": "answer"}
    ]}
  ]
}"#;

#[test]
fn f03_thinking_whole_drop() {
    let r = req(REQ_WITH_THINKING);
    let chat = messages_request_to_chat(&r);
    let s = serde_json::to_string(&chat).expect("serialize chat req");
    // 整块丢弃:thinking 内容与签名都不外泄。
    assert!(!s.contains("secret-thought"));
    assert!(!s.contains("sig123"));
    assert!(!s.contains("thinking"));
    // 其余内容保真。
    assert!(s.contains("answer"));
    // 纯函数:原请求不被修改。
    assert!(matches!(
        req(REQ_WITH_THINKING).messages[1].content,
        InboundContent::Blocks(_)
    ));
}

// ---------------------------------------------------------------------------
// F-4 finish_reason 显式映射(magpie L-14:SAFETY→stop 是事故)
// ---------------------------------------------------------------------------

#[test]
fn f04_finish_reason_explicit() {
    // 纯函数映射表。
    assert_eq!(map_finish_reason("stop"), "end_turn");
    assert_eq!(map_finish_reason("length"), "max_tokens");
    assert_eq!(map_finish_reason("tool_calls"), "tool_use");
    assert_eq!(map_finish_reason("content_filter"), "refusal");
    assert_eq!(map_finish_reason("totally-unknown"), "refusal");

    // 流级:content_filter 不得被翻译成 end_turn。
    let events = run_stream(CHAT_REFUSAL);
    let stop = events.iter().find_map(|e| match e {
        AnthropicEvent::MessageDelta { delta, .. } => delta.stop_reason.clone(),
        _ => None,
    });
    assert_eq!(stop.as_deref(), Some("refusal"));
}

#[test]
fn chat_refusal_deltas_preserve_text_usage_and_stop_reason() {
    // Synthetic Chat replay: refusal is the only text-bearing field.
    let prefix = concat!(
        "data: {\"id\":\"chatcmpl_refusal\",\"choices\":[{\"delta\":{\"refusal\":\"I can’t\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"refusal\":\" \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"refusal\":\"do that.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"refusal\":\"\\n\"}}]}\n\n",
    );
    for reason in ["stop", "content_filter"] {
        let finish = serde_json::json!({"choices":[{"finish_reason":reason}]});
        let usage = r#"data: {"choices":[],"usage":{"prompt_tokens":17,"completion_tokens":4}}"#;
        let out = run_stream(&format!(
            "{prefix}data: {finish}\n\n{usage}\n\ndata: [DONE]\n\n"
        ));
        assert_eq!(
            out.iter()
                .map(AnthropicEvent::event_name)
                .collect::<Vec<_>>(),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{reason}"
        );
        assert!(matches!(
            &out[0],
            AnthropicEvent::MessageStart { message } if message.id == "msg_chatcmpl_refusal"
        ));
        for (event, expected) in out[2..6].iter().zip(["I can’t", " ", "do that.", "\n"]) {
            assert!(matches!(
                event,
                AnthropicEvent::ContentBlockDelta {
                    index: 0,
                    delta: AnthropicDelta::TextDelta { text },
                } if text == expected
            ));
        }
        assert!(matches!(
            &out[7],
            AnthropicEvent::MessageDelta { delta, usage: Some(usage) }
                if delta.stop_reason.as_deref() == Some("refusal")
                    && usage.input_tokens == 17
                    && usage.output_tokens == 4
        ));
    }
}

#[test]
fn chat_refusal_keeps_content_tool_order_and_ignores_invalid_values() {
    use serde_json::json;

    let prefix = concat!(
        r#"data: {"choices":[{"delta":{"content":"text","refusal":" refusal ","tool_calls":[{"index":0,"id":"call_1","function":{"name":"fixture","arguments":"{"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"refusal":" \n","tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]}"#,
        "\n\n",
    );
    for (finish, expected) in [
        ("tool_calls", "tool_use"),
        ("length", "max_tokens"),
        ("stop", "end_turn"),
    ] {
        let terminal = json!({"choices":[{"finish_reason":finish}]});
        let out = run_stream(&format!("{prefix}data: {terminal}\n\n"));
        let deltas: Vec<_> = out
            .iter()
            .filter_map(|event| match event {
                AnthropicEvent::ContentBlockDelta { index, delta } => Some((*index, delta)),
                _ => None,
            })
            .collect();
        assert!(matches!(
            deltas.as_slice(),
            [
                (0, AnthropicDelta::TextDelta { text }),
                (0, AnthropicDelta::TextDelta { text: refusal }),
                (1, AnthropicDelta::InputJsonDelta { partial_json: open }),
                (1, AnthropicDelta::InputJsonDelta { partial_json: close }),
            ] if text == "text" && refusal == " refusal " && open == "{" && close == "}"
        ));
        assert!(matches!(
            out.as_slice(),
            [.., AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop]
                if delta.stop_reason.as_deref() == Some(expected)
        ));
    }

    let ordinary = r#"data: {"choices":[{"delta":{"content":"ordinary"},"finish_reason":"stop"}]}"#;
    let expected = run_stream(&format!("{ordinary}\n\n"));
    for refusal in [
        json!(null),
        json!(""),
        json!(42),
        json!(["x"]),
        json!({"x":1}),
    ] {
        let chunk = json!({"choices":[{"delta":{"content":"ordinary","refusal":refusal},"finish_reason":"stop"}]});
        assert_eq!(
            run_stream(&format!("data: {chunk}\n\n")),
            expected,
            "{refusal}"
        );
    }

    let whitespace = r#"data: {"choices":[{"delta":{"refusal":" \t\n"},"finish_reason":"stop"}]}"#;
    let out = run_stream(&format!("{whitespace}\n\n"));
    assert!(matches!(
        out.as_slice(),
        [
            ..,
            AnthropicEvent::ContentBlockDelta { delta: AnthropicDelta::TextDelta { text }, .. },
            AnthropicEvent::ContentBlockStop { .. },
            AnthropicEvent::MessageDelta { delta, .. },
            AnthropicEvent::MessageStop,
        ] if text == " \t\n" && delta.stop_reason.as_deref() == Some("end_turn")
    ));

    let prefix = r#"data: {"choices":[{"delta":{"refusal":"cannot comply"}}]}"#;
    let prefix = format!("{prefix}\n\n");
    let eof = run_stream(&prefix);
    let mut translator = ChatToAnthropic::new("glm-4.5-flash");
    let (chunks, _) = parse_chat_stream(&prefix);
    let mut broken = translator.push(&chunks[0]);
    broken.extend(translator.push_bad_chunk("fixture corrupted chunk"));
    broken.extend(translator.finish());
    for events in [eof, broken] {
        assert!(events.iter().any(|event| matches!(
            event,
            AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::TextDelta { text },
                ..
            } if text == "cannot comply"
        )));
        assert!(matches!(
            events.as_slice(),
            [
                ..,
                AnthropicEvent::Error { .. },
                AnthropicEvent::MessageStop
            ]
        ));
    }
}

// ---------------------------------------------------------------------------
// F-5 mid-stream 失败必须补完整 error 终止(magpie L-19:裸断流)
// ---------------------------------------------------------------------------

#[test]
fn f05_midstream_error_terminates() {
    // a) 上游中途坏块。
    let (chunks, _) = parse_chat_stream(CHAT_TOOL_CALL);
    let mut tr = ChatToAnthropic::new("m");
    let mut events: Vec<AnthropicEvent> = chunks[..2].iter().flat_map(|c| tr.push(c)).collect();
    events.extend(tr.push_bad_chunk("upstream 502 mid-stream"));
    events.extend(tr.finish());

    assert!(events.iter().any(|e| matches!(
        &e,
        AnthropicEvent::Error { error } if error.message.contains("502")
    )));
    // 错误后不得再出现正常 message_delta。
    assert!(!events
        .iter()
        .any(|e| matches!(e, AnthropicEvent::MessageDelta { .. })));
    // 流仍以 message_stop 规范闭合(不裸断)。
    assert!(matches!(events.last(), Some(AnthropicEvent::MessageStop)));

    // b) 截断流(无 finish_reason)= 中途失败,显式报错。
    let ev2 = run_stream(CHAT_TRUNCATED);
    assert!(ev2
        .iter()
        .any(|e| matches!(e, AnthropicEvent::Error { .. })));
    assert!(matches!(ev2.last(), Some(AnthropicEvent::MessageStop)));
}

// ---------------------------------------------------------------------------
// F-6 count_tokens 双路径(magpie L-23:不支持的上游会 500)
// ---------------------------------------------------------------------------

#[test]
fn f06_count_tokens_dual_path() {
    let r = req(REQ_WITH_THINKING);
    assert!(matches!(count_tokens(true, &r), CountOutcome::Upstream));
    assert!(matches!(
        count_tokens(false, &r),
        CountOutcome::Estimated { tokens } if tokens > 0
    ));
}

// ---------------------------------------------------------------------------
// F-7 thinking 签名整流(sub2api/model-router rectifier)
// ---------------------------------------------------------------------------

#[test]
fn f07_thinking_rectify() {
    let r = req(REQ_WITH_THINKING);
    let fixed = rectify_thinking_for_retry(&r).expect("should strip thinking");
    // 剥离后无 thinking 块,其余保留。
    for m in &fixed.messages {
        if let InboundContent::Blocks(blocks) = &m.content {
            assert!(!blocks
                .iter()
                .any(|b| matches!(b, InboundBlock::Thinking { .. })));
        }
    }
    assert_eq!(fixed.messages.len(), r.messages.len());
    // 原请求不动。
    assert!(matches!(r.messages[1].content, InboundContent::Blocks(_)));

    // 无 thinking 块 → None(重试无意义)。
    let plain = req(REQ_WITH_TOOLS);
    assert!(rectify_thinking_for_retry(&plain).is_none());
}

// ---------------------------------------------------------------------------
// F-8 孤儿 tool_result 清理(model-router)
// ---------------------------------------------------------------------------

const REQ_ORPHAN: &str = r#"{
  "model": "glm-4.5-flash", "max_tokens": 100,
  "messages": [
    {"role": "user", "content": "weather?"},
    {"role": "assistant", "content": [
      {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "Beijing"}}
    ]},
    {"role": "user", "content": [
      {"type": "tool_result", "tool_use_id": "t1", "content": "20C"},
      {"type": "tool_result", "tool_use_id": "t9", "content": "ghost"},
      {"type": "text", "text": "thanks"}
    ]}
  ]
}"#;

#[test]
fn f08_orphan_tool_result_dropped() {
    let chat = messages_request_to_chat(&req(REQ_ORPHAN));
    let tool_msgs: Vec<&ChatMessage> = chat.messages.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tool_msgs.len(), 1, "orphan tool_result dropped");
    assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("t1"));
    assert_eq!(tool_msgs[0].content.as_deref(), Some("20C"));
    // 孤儿 id 不出现在任何消息里。
    let all = serde_json::to_string(&chat.messages).expect("serialize");
    assert!(!all.contains("t9"));
    assert!(!all.contains("ghost"));
    // assistant 带 tool_calls,user 文本保留。
    assert!(chat
        .messages
        .iter()
        .any(|m| m.role == "assistant" && m.tool_calls.is_some()));
    assert!(chat
        .messages
        .iter()
        .any(|m| m.role == "user" && m.content.as_deref() == Some("thanks")));
}

// ---------------------------------------------------------------------------
// F-9 keepalive 注释不产生伪事件、注释后不写空行(magpie sse.go)
// ---------------------------------------------------------------------------

#[test]
fn f09_keepalive_is_not_event() {
    let framed = sse::frame_event("message_start", "{}")
        + &sse::frame_keepalive()
        + &sse::frame_event("message_stop", "{}");
    let (events, report) = sse::parse(&framed);
    assert_eq!(events.len(), 2);
    assert!(!report.incomplete);

    // keepalive 帧本身:注释行,不以空行结尾。
    let ka = sse::frame_keepalive();
    assert_eq!(ka, ": keepalive\n");
    assert!(!ka.ends_with("\n\n"));

    // 注释插在 data 行之间:不重置事件缓冲,不拆事件。
    let mixed = "event: x\ndata: a\n: keepalive\ndata: b\n\n";
    let (ev, rep) = sse::parse(mixed);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data, "a\nb");
    assert!(!rep.incomplete);
}

// ---------------------------------------------------------------------------
// F-10 空 body / 截断 body 不 panic(magpie #823)
// ---------------------------------------------------------------------------

#[test]
fn f10_empty_and_truncated_no_panic() {
    // 空输入。
    let (ev, rep) = sse::parse("");
    assert!(ev.is_empty());
    assert!(!rep.incomplete);

    // 截断输入:标记 incomplete,不 panic。
    let (ev, rep) = sse::parse(CHAT_TRUNCATED);
    assert!(rep.incomplete);
    assert!(!ev.is_empty());

    // 坏 JSON:记入报告,不 panic。
    let (chunks, rep) = parse_chat_stream("data: not-json\n\n");
    assert!(chunks.is_empty());
    assert!(!rep.bad_json.is_empty());

    // [DONE] 是流终止标记,不是坏 JSON。
    let (chunks, rep) = parse_chat_stream("data: [DONE]\n\n");
    assert!(chunks.is_empty());
    assert!(rep.bad_json.is_empty());

    // 坏块经翻译器:产 error 终止事件(先补 message_start),不 panic。
    let mut tr = ChatToAnthropic::new("m");
    let ev = tr.push_bad_chunk("garbage");
    assert!(ev.iter().any(|e| matches!(e, AnthropicEvent::Error { .. })));
    let tail = tr.finish();
    assert!(matches!(tail.last(), Some(AnthropicEvent::MessageStop)));
}

// ---------------------------------------------------------------------------
// F-11 tools / tool_choice / 参数映射(engineering-constraints A-3)
// ---------------------------------------------------------------------------

const REQ_WITH_TOOLS: &str = r#"{
  "model": "glm-4.5-flash", "max_tokens": 128,
  "system": "You are helpful.",
  "messages": [
    {"role": "user", "content": [{"type": "text", "text": "weather in Beijing?"}]}
  ],
  "tools": [
    {"name": "get_weather", "description": "Get weather",
     "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}
  ],
  "tool_choice": {"type": "tool", "name": "get_weather"}
}"#;

#[test]
fn explicit_serial_tool_choice_survives_translation() {
    let mut input: serde_json::Value = serde_json::from_str(REQ_WITH_TOOLS).unwrap();
    for mode in ["auto", "any", "tool"] {
        input["tool_choice"] = serde_json::json!({
            "type":mode, "name":"get_weather", "disable_parallel_tool_use":true
        });
        let request = req(&input.to_string());
        let chat = serde_json::to_value(messages_request_to_chat(&request)).unwrap();
        let responses = serde_json::to_value(messages_request_to_responses(&request)).unwrap();
        assert_eq!(chat["parallel_tool_calls"], false);
        assert_eq!(responses["parallel_tool_calls"], false);
        if mode == "tool" {
            assert_eq!(chat["tool_choice"]["function"]["name"], "get_weather");
            assert_eq!(responses["tool_choice"]["name"], "get_weather");
        }
    }
    input["tool_choice"]["disable_parallel_tool_use"] = serde_json::json!(false);
    for result in [
        serde_json::to_value(messages_request_to_chat(&req(&input.to_string()))).unwrap(),
        serde_json::to_value(messages_request_to_responses(&req(&input.to_string()))).unwrap(),
    ] {
        assert!(result.get("parallel_tool_calls").is_none());
    }
}

#[test]
fn chat_serial_tool_choice_allows_responses_fallback() {
    use serde_json::{json, Value};
    use translate::chat_input::ChatInput;

    let mut raw = serde_json::to_value(messages_request_to_chat(&req(REQ_WITH_TOOLS))).unwrap();
    raw["parallel_tool_calls"] = json!(false);
    for (choice, expected, kind) in [
        (Value::Null, json!("auto"), "auto"),
        (json!("auto"), json!("auto"), "auto"),
        (json!("required"), json!("required"), "any"),
        (
            json!({"type":"function","function":{"name":"get_weather"}}),
            json!({"type":"function","name":"get_weather"}),
            "tool",
        ),
    ] {
        raw["tool_choice"] = choice;
        let chat = ChatInput::parse(&raw.to_string()).unwrap();
        assert!(chat.supports_responses(), "{kind}");
        assert_eq!(chat.for_chat("model")["parallel_tool_calls"], false);
        let responses = serde_json::to_value(messages_request_to_responses(&chat.request)).unwrap();
        assert_eq!(responses["parallel_tool_calls"], false);
        assert_eq!(responses["tool_choice"], expected);
        let anthropic = chat.for_anthropic(&chat.request).unwrap();
        assert_eq!(anthropic["tool_choice"]["type"], kind);
        assert_eq!(anthropic["tool_choice"]["disable_parallel_tool_use"], true);
    }
    for (choice, parallel, tools) in [
        (json!("none"), json!(false), raw["tools"].clone()),
        (Value::Null, json!(false), Value::Null),
        (json!("auto"), json!(true), raw["tools"].clone()),
        (json!("auto"), Value::Null, raw["tools"].clone()),
    ] {
        raw["tool_choice"] = choice;
        raw["parallel_tool_calls"] = parallel;
        raw["tools"] = tools;
        let chat = ChatInput::parse(&raw.to_string()).unwrap();
        assert!(chat.supports_responses());
        assert_eq!(chat.request.parallel_tool_calls_override(), None);
    }
    raw["parallel_tool_calls"] = json!(false);
    raw["temperature"] = json!(0.2);
    assert!(!ChatInput::parse(&raw.to_string())
        .unwrap()
        .supports_responses());
}

#[test]
fn f11_tools_roundtrip() {
    let chat = messages_request_to_chat(&req(REQ_WITH_TOOLS));

    // system → 首条 system 消息。
    assert_eq!(chat.messages[0].role, "system");
    assert_eq!(
        chat.messages[0].content.as_deref(),
        Some("You are helpful.")
    );

    // tools:schema 原样映射为 parameters。
    let tools = chat.tools.as_ref().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].kind, "function");
    assert_eq!(tools[0].function.name, "get_weather");
    assert_eq!(
        tools[0].function.description.as_deref(),
        Some("Get weather")
    );
    assert_eq!(
        tools[0].function.parameters,
        serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}})
    );

    // tool_choice:具名 → function 对象;序列化形状精确。
    let tc = serde_json::to_value(chat.tool_choice.clone().unwrap()).expect("tc");
    assert_eq!(
        tc,
        serde_json::json!({"type": "function", "function": {"name": "get_weather"}})
    );

    // F-2 前提:include_usage 注入。
    assert_eq!(
        chat.stream_options,
        Some(ChatStreamOptions {
            include_usage: true
        })
    );
    assert_eq!(chat.stream, Some(true));
    assert_eq!(chat.max_tokens, Some(128));

    // 模式映射:auto / any / none。
    let mk = |json: &str| req(json).tool_choice.unwrap();
    let auto = serde_json::to_value(tool_choice_to_chat(&mk(
        r#"{"tool_choice": {"type": "auto"}, "messages": [], "model": "m"}"#,
    )))
    .unwrap();
    assert_eq!(auto, serde_json::json!("auto"));
    let any = serde_json::to_value(tool_choice_to_chat(&mk(
        r#"{"tool_choice": {"type": "any"}, "messages": [], "model": "m"}"#,
    )))
    .unwrap();
    assert_eq!(any, serde_json::json!("required"));
    let none = serde_json::to_value(tool_choice_to_chat(&mk(
        r#"{"tool_choice": {"type": "none"}, "messages": [], "model": "m"}"#,
    )))
    .unwrap();
    assert_eq!(none, serde_json::json!("none"));
}

// ---------------------------------------------------------------------------
// golden:事件序列精确相等(防漂移)+ SSE 帧→解析往返
// ---------------------------------------------------------------------------

#[test]
fn golden_tool_call_event_sequence() {
    let events = run_stream(CHAT_TOOL_CALL);
    assert_eq!(
        events,
        vec![
            AnthropicEvent::MessageStart {
                message: MessageStartBody {
                    id: "msg_chatcmpl-smoke-1".into(),
                    model: "glm-4.5-flash".into(),
                    role: "assistant".into(),
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 1,
                        ..Default::default()
                    },
                }
            },
            AnthropicEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new()
                }
            },
            AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::TextDelta {
                    text: "Let me check ".into()
                }
            },
            AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::TextDelta {
                    text: "the weather.".into()
                }
            },
            AnthropicEvent::ContentBlockStop { index: 0 },
            AnthropicEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_w_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::Value::Null,
                }
            },
            AnthropicEvent::ContentBlockDelta {
                index: 1,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: "{\"ci".into()
                }
            },
            AnthropicEvent::ContentBlockDelta {
                index: 1,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: "ty\":\"Beijing\"".into()
                }
            },
            AnthropicEvent::ContentBlockDelta {
                index: 1,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: "}".into()
                }
            },
            AnthropicEvent::ContentBlockStop { index: 1 },
            AnthropicEvent::MessageDelta {
                delta: MessageDeltaBody {
                    stop_reason: Some("tool_use".into()),
                    stop_sequence: None,
                },
                usage: Some(Usage {
                    input_tokens: 25,
                    output_tokens: 30,
                    ..Default::default()
                }),
            },
            AnthropicEvent::MessageStop,
        ]
    );
}

#[test]
fn framing_round_trip() {
    let events = run_stream(CHAT_TOOL_CALL);
    let framed = sse::frame_anthropic(&events);
    let (raw, report) = sse::parse(&framed);
    assert!(!report.incomplete);
    assert_eq!(raw.len(), events.len());
    // 事件名与数据一一对应。
    for (raw_ev, ev) in raw.iter().zip(events.iter()) {
        assert_eq!(raw_ev.event.as_deref(), Some(ev.event_name()));
        assert_eq!(
            serde_json::from_str::<AnthropicEvent>(&raw_ev.data).expect("re-parse event"),
            *ev
        );
    }
}

// ---------------------------------------------------------------------------
// F-12 Responses 上游方言(codex 订阅,§6.3):请求側强制变换。
// 事件側用例随真实 codex 语料录制补齐(A5)。
// ---------------------------------------------------------------------------

/// 请求側:强制 stream/store、system→instructions、thinking→reasoning
/// 回放、tool_use/tool_result 保序映射、max_tokens 钳下限、孤儿丢弃(F-8)。
#[test]
fn f12_responses_request_forced_transforms() {
    let req = req(r#"{
        "model": "gpt-5.2-codex",
        "max_tokens": 32,
        "system": "be brief",
        "tools": [
            {"name": "get_weather",
             "description": "查天气",
             "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}
        ],
        "tool_choice": {"type": "any"},
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "ponder", "signature": "SIG1"},
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "北京"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"},
                {"type": "tool_result", "tool_use_id": "toolu_orphan", "content": "ghost"},
                {"type": "text", "text": "thanks"}
            ]}
        ]
    }"#);

    let out = messages_request_to_responses(&req);

    // 强制变换(§6.3 上游硬约束)。
    assert!(out.stream);
    assert!(!out.store);
    assert_eq!(out.instructions.as_deref(), Some("be brief"));
    assert_eq!(
        out.include.as_deref(),
        Some(&["reasoning.encrypted_content".to_string()][..])
    );
    // max_tokens 钳到 Responses 下限(sub2api 口径 128)。
    assert_eq!(out.max_output_tokens, Some(128));
    // tools / tool_choice 映射。
    let tools = out.tools.as_ref().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].kind, "function");
    assert_eq!(tools[0].name, "get_weather");
    assert_eq!(
        out.tool_choice,
        Some(proto::responses::ResponsesToolChoice::Mode(
            "required".to_string()
        ))
    );

    // input 序列保序:user 首轮 → reasoning 回放 → assistant 文本 →
    // function_call → function_call_output(孤儿丢弃)→ user 文本。
    use proto::responses::ResponsesInputItem as Item;
    match &out.input[0] {
        Item::Message { role, content } => {
            assert_eq!(role, "user");
            assert!(matches!(
                content[0],
                proto::responses::ResponsesContentPart::InputText { ref text }
                    if text == "hi"
            ));
        }
        other => panic!("unexpected item 0: {other:?}"),
    }
    assert!(matches!(
        out.input[1],
        Item::Reasoning { ref encrypted_content } if encrypted_content == "SIG1"
    ));
    match &out.input[2] {
        Item::Message { role, content } => {
            assert_eq!(role, "assistant");
            assert!(matches!(
                content[0],
                proto::responses::ResponsesContentPart::OutputText { ref text }
                    if text == "let me check"
            ));
        }
        other => panic!("unexpected item 1: {other:?}"),
    }
    assert!(matches!(
        &out.input[3],
        Item::FunctionCall { call_id, name, arguments }
            if call_id == "toolu_1" && name == "get_weather" && arguments.contains("北京")
    ));
    assert!(matches!(
        &out.input[4],
        Item::FunctionCallOutput { call_id, output }
            if call_id == "toolu_1" && output == "sunny"
    ));
    match &out.input[5] {
        Item::Message { role, content } => {
            assert_eq!(role, "user");
            assert!(matches!(
                content[0],
                proto::responses::ResponsesContentPart::InputText { ref text }
                    if text == "thanks"
            ));
        }
        other => panic!("unexpected item 4: {other:?}"),
    }
    assert_eq!(out.input.len(), 6, "孤儿 tool_result 必须被丢弃(F-8)");
}

#[test]
fn responses_request_preserves_only_explicit_enabled_claude_effort() {
    use serde_json::{json, Value};

    let minimal = json!({
        "model": "fixture-model",
        "max_tokens": 256,
        "messages": [{"role":"user","content":"hello"}]
    });
    let responses = |input: &Value| {
        serde_json::to_value(messages_request_to_responses(&req(&input.to_string()))).unwrap()
    };
    let chat = |input: &Value| {
        serde_json::to_value(messages_request_to_chat(&req(&input.to_string()))).unwrap()
    };
    let baseline = responses(&minimal);
    let chat_baseline = chat(&minimal);
    assert!(baseline.get("reasoning").is_none());
    assert!(baseline.get("include").is_none());

    for mode in ["enabled", "adaptive"] {
        for effort in ["low", "medium", "high"] {
            let mut input = minimal.clone();
            input["thinking"] = json!({"type":mode,"budget_tokens":1024,"future":true});
            input["output_config"] = json!({"effort":effort,"summary":"detailed","future":true});
            let mut output = responses(&input);
            assert_eq!(
                output.as_object_mut().unwrap().remove("reasoning"),
                Some(json!({"effort":effort})),
                "{mode}/{effort}"
            );
            assert_eq!(output, baseline);
            assert_eq!(chat(&input), chat_baseline);
        }
    }

    // disabled/high is also the shape of Claude's auxiliary request.
    for (thinking, output_config) in [
        (None, Some(json!({"effort":"high"}))),
        (
            Some(json!({"type":"disabled"})),
            Some(json!({"effort":"high"})),
        ),
        (
            Some(json!({"type":"between_tools"})),
            Some(json!({"effort":"high"})),
        ),
        (
            Some(json!({"type":"future"})),
            Some(json!({"effort":"high"})),
        ),
        (Some(json!({"type":"enabled","budget_tokens":1024})), None),
        (Some(json!({"type":"adaptive"})), Some(json!({}))),
        (
            Some(json!({"type":"adaptive"})),
            Some(json!({"effort":"max"})),
        ),
        (
            Some(json!({"type":"adaptive"})),
            Some(json!({"effort":"xhigh"})),
        ),
        (
            Some(json!({"type":"adaptive"})),
            Some(json!({"effort":null})),
        ),
        (Some(json!({"type":"adaptive"})), Some(json!({"effort":2}))),
        (
            Some(json!({"type":"adaptive"})),
            Some(json!({"effort":["high"]})),
        ),
        (Some(json!("enabled")), Some(json!({"effort":"high"}))),
        (Some(json!(["adaptive"])), Some(json!({"effort":"high"}))),
        (Some(Value::Null), Some(json!({"effort":"high"}))),
        (Some(json!({"type":"enabled"})), Some(json!("high"))),
        (Some(json!({"type":"enabled"})), Some(json!(["high"]))),
        (Some(json!({"type":"enabled"})), Some(Value::Null)),
    ] {
        let mut input = minimal.clone();
        if let Some(thinking) = thinking {
            input["thinking"] = thinking;
        }
        if let Some(output_config) = output_config {
            input["output_config"] = output_config;
        }
        assert_eq!(responses(&input), baseline, "{input}");
    }
}

/// 无 thinking 块时不加 include;无签名 thinking 整块丢弃(F-3)。
#[test]
fn f12_responses_request_no_thinking_no_include() {
    let req = req(r#"{
        "model": "gpt-5.2-codex",
        "max_tokens": 0,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "no sig", "signature": ""},
                {"type": "text", "text": "answer"}
            ]}
        ]
    }"#);
    let out = messages_request_to_responses(&req);
    assert_eq!(out.include, None);
    assert_eq!(out.max_output_tokens, None);
    assert_eq!(out.input.len(), 1);
    assert!(matches!(
        &out.input[0],
        proto::responses::ResponsesInputItem::Message { role, .. } if role == "assistant"
    ));
}

// ---------------------------------------------------------------------------
// F-12 事件側:Responses SSE → Anthropic 事件(合成语料 v0,待真实
// codex 语料替换;语料纪律:先用合成锁定行为契约,真实语料录制后对齐)
// ---------------------------------------------------------------------------

/// 语料:reasoning(签名随 done 送达)+ tool 参数增量 + usage 换算。
/// 契约:F-1 参数严格保序、F-3 签名原样透传且先于块停、F-2 usage
/// net 换算(input − cached − cache_creation)与缓存字段直传。
#[test]
fn f12_responses_events_reasoning_tool_usage() {
    // 合成 SSE 上游流 v0。
    let sse = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_9"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_9"}}"#,
        "\n\n",
        r#"data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"deliberating"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"SIG_9"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_9","name":"read_file"}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"pa"}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"th\":\"a.txt\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","arguments":"{\"path\":\"a.txt\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1000,"output_tokens":42,"input_tokens_details":{"cached_tokens":600},"cache_creation_input_tokens":100}}}"#,
        "\n\n",
    );
    let (events, report) = parse_responses_stream(sse);
    assert!(!report.incomplete);
    assert!(report.bad_json.is_empty());

    let mut sm = ResponsesToAnthropic::new("gpt-5.2-codex");
    let mut out = Vec::new();
    for ev in &events {
        out.extend(sm.push(ev));
    }
    out.extend(sm.finish());

    // thinking 块:ThinkingDelta → SignatureDelta(SIG_9 原样)→ 块停;
    // tool 块:两个 InputJsonDelta 拼回完整 JSON(保序,F-1);
    // 终态:stop_reason=tool_use;usage 1000−600−100=300。
    let names: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
    assert_eq!(
        names,
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
        &out[3],
        AnthropicEvent::ContentBlockDelta {
            delta: AnthropicDelta::SignatureDelta { signature },
            ..
        } if signature == "SIG_9"
    ));
    let args: String = out
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(args, r#"{"path":"a.txt"}"#);
    match &out[9] {
        AnthropicEvent::MessageDelta { delta, usage } => {
            assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
            let usage = usage.as_ref().unwrap();
            assert_eq!(usage.input_tokens, 300);
            assert_eq!(usage.output_tokens, 42);
            assert_eq!(usage.cache_read_input_tokens, Some(600));
            assert_eq!(usage.cache_creation_input_tokens, Some(100));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(matches!(
        &out[0],
        AnthropicEvent::MessageStart { message } if message.id == "msg_resp_9"
    ));
}

/// 语料:中途 error 事件 + 终态后静默。
/// 契约:F-5 完整 error 终止(start + error + stop),不裸断流;
/// 终态后再到的事件一律静默;F-10 坏 JSON 不 panic。
#[test]
fn f12_responses_events_midstream_error() {
    let sse = concat!(
        r#"data: {"type":"response.created","response":{"id":"r"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"partial"}"#,
        "\n\n",
        r#"data: {"type":"error","code":"server_error","message":"upstream exploded"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        "\n\n",
    );
    let (events, _) = parse_responses_stream(sse);
    let mut sm = ResponsesToAnthropic::new("m");
    let mut out = Vec::new();
    for ev in &events {
        out.extend(sm.push(ev));
    }
    out.extend(sm.finish());
    // error 后 completed 静默:流以 error 终止,usage 不再落地。
    // mark_error 不补块停(error 语义终结整条消息):start + 块开 +
    // delta + error + stop,共 5 事件(F-5 完整终止,不裸断流)。
    assert!(matches!(
        &out[3],
        AnthropicEvent::Error { error }
            if error.kind == "upstream_error"
                && error.message.contains("server_error")
                && error.message.contains("upstream exploded")
    ));
    assert!(matches!(out.last(), Some(AnthropicEvent::MessageStop)));
    assert_eq!(out.len(), 5);

    // 截断流(无终态):finish 兜底补 error 终止(F-5)。
    let (events, _) = parse_responses_stream(concat!(
        r#"data: {"type":"response.output_text.delta","delta":"x"}"#,
        "\n\n"
    ));
    let mut sm = ResponsesToAnthropic::new("m");
    let mut out = Vec::new();
    for ev in &events {
        out.extend(sm.push(ev));
    }
    out.extend(sm.finish());
    assert!(out
        .iter()
        .any(|e| matches!(e, AnthropicEvent::Error { .. })));
    assert!(matches!(out.last(), Some(AnthropicEvent::MessageStop)));
}

// ---------------------------------------------------------------------------
// F-12 事件側真实语料(2026-11-04 录制,chatgpt.com/backend-api/
// codex/responses,gpt-6.1-sol;录制探针经本地代理,凭据只在进程内存)
// ---------------------------------------------------------------------------

fn run_responses(sse: &str, model: &str) -> Vec<AnthropicEvent> {
    let (events, report) = parse_responses_stream(sse);
    assert!(!report.incomplete, "完整流不应报告截断");
    assert!(report.bad_json.is_empty(), "真实语料不应有坏 JSON");
    let mut sm = ResponsesToAnthropic::new(model);
    let mut out = Vec::new();
    for ev in &events {
        out.extend(sm.push(ev));
    }
    out.extend(sm.finish());
    out
}

fn without_known_responses_json_types(recording: &str) -> String {
    use proto::responses::ResponsesEvent;

    let (events, report) = sse::parse(recording);
    assert!(!report.incomplete);
    events
        .into_iter()
        .map(|event| {
            let kind: ResponsesEvent = serde_json::from_str(&event.data).unwrap();
            let label = event.event.as_deref().unwrap();
            if kind == ResponsesEvent::Unknown {
                // An unknown label cannot stand in for a missing JSON type.
                return sse::frame_event(label, &event.data);
            }
            let mut data: serde_json::Value = serde_json::from_str(&event.data).unwrap();
            assert_eq!(
                data.as_object_mut().unwrap().remove("type"),
                Some(serde_json::json!(label))
            );
            sse::frame_event(label, &data.to_string())
        })
        .collect()
}

#[test]
fn responses_sse_event_labels_fill_missing_types() {
    // Derived from the existing recording; only known events lose their
    // top-level JSON type. The SSE labels and nested payloads stay intact.
    let labeled = without_known_responses_json_types(CODEX_TEXT);
    let (expected, report) = parse_responses_stream(CODEX_TEXT);
    assert!(report.bad_json.is_empty());
    let (actual, report) = parse_responses_stream(&labeled);
    assert!(
        report.bad_json.is_empty(),
        "{} events failed to parse with labels alone",
        report.bad_json.len()
    );
    assert_eq!(actual, expected);
    assert_eq!(
        run_responses(&labeled, "gpt-6.1-sol"),
        run_responses(CODEX_TEXT, "gpt-6.1-sol")
    );
}

#[test]
fn responses_sse_event_labels_preserve_tool_order_and_terminal_semantics() {
    let labeled = without_known_responses_json_types(CODEX_TOOL);
    // Full event equality checks each of the six ordered argument deltas,
    // the final tool block, and usage without duplicating the original fixture.
    assert_eq!(
        run_responses(&labeled, "gpt-6.1-sol"),
        run_responses(CODEX_TOOL, "gpt-6.1-sol")
    );

    let prefix = concat!(
        "event: response.created\ndata: {\"response\":{\"id\":\"r\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"delta\":\"partial\"}\n\n",
    );
    let done = format!(
        "{prefix}event: response.done\ndata: {{\"response\":{{\"status\":\"completed\"}}}}\n\n"
    );
    let explicit_done = done.replace(
        "data: {\"response\":{\"status\":",
        "data: {\"type\":\"response.done\",\"response\":{\"status\":",
    );
    assert_eq!(
        run_responses(&done, "fixture"),
        run_responses(&explicit_done, "fixture")
    );

    let labeled_error =
        format!("{prefix}event: error\ndata: {{\"message\":\"fixture error\"}}\n\n");
    let events = run_responses(&labeled_error, "fixture");
    assert!(matches!(
        events.as_slice(),
        [
            AnthropicEvent::MessageStart { .. },
            AnthropicEvent::ContentBlockStart { .. },
            AnthropicEvent::ContentBlockDelta { .. },
            AnthropicEvent::Error { error },
            AnthropicEvent::MessageStop,
        ] if error.message.contains("fixture error")
    ));
}

#[test]
fn responses_sse_event_labels_do_not_override_or_guess_types() {
    use proto::responses::ResponsesEvent;

    let explicit_delta = r#"{"type":"response.output_text.delta","delta":"explicit"}"#;
    let stream = sse::frame_event("response.completed", explicit_delta);
    let (events, report) = parse_responses_stream(&stream);
    assert!(report.bad_json.is_empty());
    assert!(matches!(
        events.as_slice(),
        [ResponsesEvent::OutputTextDelta { delta, .. }] if delta == "explicit"
    ));

    for data in [r#"{"type":""}"#, r#"{"type":"response.fixture_unknown"}"#] {
        let stream = sse::frame_event("response.created", data);
        let (events, report) = parse_responses_stream(&stream);
        assert!(report.bad_json.is_empty());
        assert_eq!(events, [ResponsesEvent::Unknown]);
    }
    for (label, data) in [
        ("response.created", r#"{"type":null}"#),
        ("response.created", r#"{"type":42}"#),
        ("response.created", "null"),
        ("response.created", "bad-json"),
        ("response.fixture_unknown", "{}"),
        ("message", "{}"),
        ("", "{}"),
    ] {
        let (events, report) = parse_responses_stream(&sse::frame_event(label, data));
        assert!(events.is_empty(), "{label}: {data}");
        assert_eq!(report.bad_json, [data], "{label}: {data}");
    }
}

#[test]
fn responses_refusal_deltas_preserve_text_usage_and_terminal() {
    // Synthetic compatibility replay of Responses refusal deltas. Terminal
    // output also carries the full refusal, which must not duplicate the text.
    let created = r#"data: {"type":"response.created","response":{"id":"resp_refusal"}}"#;
    let first = r#"data: {"type":"response.refusal.delta","delta":"I can’t ","future_field":true}"#;
    let second = r#"data: {"type":"response.refusal.delta","delta":"do that."}"#;
    let terminal = r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":17,"output_tokens":4},"output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I can’t do that."}]}]}}"#;
    let out = run_responses(
        &format!("{created}\n\n{first}\n\n{second}\n\n{terminal}\n\n{terminal}\n\n"),
        "fixture",
    );
    assert_eq!(
        out.iter()
            .map(AnthropicEvent::event_name)
            .collect::<Vec<_>>(),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert!(matches!(
        &out[0],
        AnthropicEvent::MessageStart { message } if message.id == "msg_resp_refusal"
    ));
    for (event, expected) in out[2..4].iter().zip(["I can’t ", "do that."]) {
        assert!(matches!(
            event,
            AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::TextDelta { text },
            } if text == expected
        ));
    }
    assert!(matches!(
        &out[5],
        AnthropicEvent::MessageDelta { delta, usage: Some(usage) }
            if delta.stop_reason.as_deref() == Some("refusal")
                && usage.input_tokens == 17
                && usage.output_tokens == 4
    ));
}

#[test]
fn responses_refusal_respects_labels_and_other_terminal_rules() {
    let frame = |label, data| sse::frame_event(label, data);
    let created = frame("response.created", r#"{"response":{"id":"resp_refusal"}}"#);
    let refusal = frame("response.refusal.delta", r#"{"delta":"cannot comply"}"#);
    let completed = frame(
        "response.completed",
        r#"{"response":{"status":"completed","usage":{"input_tokens":2,"output_tokens":1}}}"#,
    );
    let text = |events: &[AnthropicEvent]| {
        events
            .iter()
            .filter_map(|event| match event {
                AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::TextDelta { text },
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    };
    let labeled = run_responses(&format!("{created}{refusal}{completed}{refusal}"), "m");
    assert_eq!(text(&labeled), "cannot comply");
    assert!(matches!(
        labeled.as_slice(),
        [.., AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop]
            if delta.stop_reason.as_deref() == Some("refusal")
    ));

    let ignored = concat!(
        "event: response.refusal.delta\ndata: {\"delta\":\"\"}\n\n",
        "event: response.refusal.done\ndata: {\"type\":\"response.refusal.done\",\"refusal\":\"ignore\"}\n\n",
    );
    let empty = run_responses(&format!("{created}{ignored}{completed}"), "m");
    assert!(text(&empty).is_empty());
    assert!(matches!(
        empty.as_slice(),
        [.., AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop]
            if delta.stop_reason.as_deref() == Some("end_turn")
    ));
    for data in [r#"{"type":null,"delta":"ignore"}"#, r#"{"delta":42}"#] {
        let (events, report) = parse_responses_stream(&frame("response.refusal.delta", data));
        assert!(events.is_empty());
        assert_eq!(report.bad_json, [data]);
    }

    let incomplete = frame(
        "response.incomplete",
        r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
    );
    let out = run_responses(&format!("{created}{refusal}{incomplete}"), "m");
    assert_eq!(text(&out), "cannot comply");
    assert!(matches!(
        out.as_slice(),
        [.., AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop]
            if delta.stop_reason.as_deref() == Some("max_tokens")
    ));
    let truncated = run_responses(&format!("{created}{refusal}"), "m");
    assert_eq!(text(&truncated), "cannot comply");
    assert!(matches!(
        truncated.as_slice(),
        [
            ..,
            AnthropicEvent::Error { .. },
            AnthropicEvent::MessageStop
        ]
    ));

    let tool = concat!(
        "event: response.output_item.added\ndata: {\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"fixture\"}}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"output_index\":1,\"delta\":\"{\"}\n\n",
        "event: response.function_call_arguments.delta\ndata: {\"output_index\":1,\"delta\":\"}\"}\n\n",
    );
    let mixed = run_responses(&format!("{created}{refusal}{tool}{completed}"), "m");
    assert_eq!(text(&mixed), "cannot comply");
    let deltas: Vec<_> = mixed
        .iter()
        .filter_map(|event| match event {
            AnthropicEvent::ContentBlockDelta { index, delta } => Some((*index, delta)),
            _ => None,
        })
        .collect();
    assert!(matches!(
        deltas.as_slice(),
        [
            (0, AnthropicDelta::TextDelta { .. }),
            (1, AnthropicDelta::InputJsonDelta { partial_json: open }),
            (1, AnthropicDelta::InputJsonDelta { partial_json: close }),
        ] if open == "{" && close == "}"
    ));
    assert!(matches!(
        mixed.as_slice(),
        [.., AnthropicEvent::MessageDelta { delta, .. }, AnthropicEvent::MessageStop]
            if delta.stop_reason.as_deref() == Some("tool_use")
    ));
}

#[test]
fn responses_completed_recovers_terminal_only_text_once() {
    // magpie 8abdf53 internal/gateway/responses_ws_test.go:46-51, n=1.
    // Omit output_item.done as in the terminal-only recovery case in sub2api
    // a3eb7ef backend/internal/pkg/apicompat/responses_to_anthropic_text_recovery_test.go:96-112.
    let created = r#"data: {"type":"response.created","response":{"id":"resp_up1","object":"response","status":"in_progress","output":[]}}"#;
    let completed = r#"data: {"type":"response.completed","response":{"id":"resp_up1","object":"response","status":"completed","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer 1"}]}],"usage":{"input_tokens":300,"output_tokens":50,"total_tokens":350}}}"#;
    // Duplicate terminal and late delta are synthetic replay fault variants.
    let late = r#"data: {"type":"response.output_text.delta","delta":"late text"}"#;
    let out = run_responses(
        &format!("{created}\n\n{completed}\n\n{completed}\n\n{late}\n\n"),
        "fixture-model",
    );
    assert_eq!(
        out.iter()
            .map(AnthropicEvent::event_name)
            .collect::<Vec<_>>(),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert!(matches!(
        &out[0],
        AnthropicEvent::MessageStart { message } if message.id == "msg_resp_up1"
    ));
    assert!(matches!(
        &out[2],
        AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: AnthropicDelta::TextDelta { text },
        } if text == "answer 1"
    ));
    assert!(matches!(
        &out[4],
        AnthropicEvent::MessageDelta { delta, usage: Some(usage) }
            if delta.stop_reason.as_deref() == Some("end_turn")
                && usage.input_tokens == 300
                && usage.output_tokens == 50
    ));
}

#[test]
fn responses_completed_recovers_only_valid_pure_text_without_prior_content() {
    use serde_json::{json, Value};

    // Synthetic variations of the Magpie/sub2api replay above.
    let text_part = |text| json!({"type":"output_text","text":text});
    let message = |content| json!({"type":"message","role":"assistant","content":content});
    let pure = message(vec![text_part("terminal")]);
    let completed = json!({"status":"completed","output":[pure.clone()]});
    let run = |prefix: &str, terminal: &str, response: Value| {
        let created = r#"data: {"type":"response.created","response":{"id":"r"}}"#;
        let terminal = json!({"type":terminal,"response":response});
        let out = run_responses(
            &format!("{created}\n\n{prefix}data: {terminal}\n\n"),
            "fixture-model",
        );
        assert!(matches!(out.last(), Some(AnthropicEvent::MessageStop)));
        out.into_iter()
            .filter_map(|event| match event {
                AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::TextDelta { text },
                    ..
                } => Some(text),
                _ => None,
            })
            .collect::<String>()
    };
    let ordered = json!({"status":"completed","output":[
        message(vec![text_part("first"), text_part(" second")]),
        message(vec![text_part(" third")])
    ]});
    assert_eq!(run("", "response.completed", ordered), "first second third");

    for output in [
        json!([pure.clone(), {"type":"function_call","name":"fixture"}]),
        json!([pure.clone(), {"type":"reasoning"}]),
        json!([pure.clone(), {"type":"message","role":"user","content":[text_part("wrong role")]}]),
        json!([pure.clone(), {"type":"message","role":"assistant","status":"incomplete","content":[text_part("unfinished")]}]),
        json!([
            pure.clone(),
            message(vec![json!({"type":"refusal","refusal":"no"})])
        ]),
        json!([
            pure.clone(),
            message(vec![json!({"type":"output_text","text":42})])
        ]),
        json!({"not":"an array"}),
        json!([message(vec![text_part("")])]),
    ] {
        assert_eq!(
            run(
                "",
                "response.completed",
                json!({"status":"completed","output":output})
            ),
            ""
        );
    }
    for (terminal, response) in [
        ("response.completed", json!({"output":[pure.clone()]})),
        (
            "response.completed",
            json!({"status":"completed","error":{"message":"fixture"},"output":[pure.clone()]}),
        ),
        (
            "response.completed",
            json!({"status":"completed","incomplete_details":{"reason":"max_output_tokens"},"output":[pure.clone()]}),
        ),
        ("response.incomplete", completed.clone()),
        ("response.failed", completed.clone()),
    ] {
        assert_eq!(run("", terminal, response), "");
    }
    for (prefix, expected) in [
        (
            r#"data: {"type":"response.output_text.delta","delta":"terminal"}"#,
            "terminal",
        ),
        (
            r#"data: {"type":"response.output_text.delta","delta":"term"}"#,
            "term",
        ),
        (
            concat!(
                r#"data: {"type":"response.output_item.added","item":{"type":"message"}}"#,
                "\n\n",
                r#"data: {"type":"response.output_text.done","text":"done-only"}"#,
            ),
            "done-only",
        ),
        (
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"fixture"}}"#,
            "",
        ),
        (
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
            "",
        ),
        (
            concat!(
                r#"data: {"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
                "\n\n",
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"signature"}}"#,
            ),
            "",
        ),
        (r#"data: {"type":"error","message":"upstream error"}"#, ""),
    ] {
        assert_eq!(
            run(
                &format!("{prefix}\n\n"),
                "response.completed",
                completed.clone()
            ),
            expected,
            "{prefix}"
        );
    }
}

/// 真实语料:纯文本流(codex_text.sse)。
#[test]
fn f12_codex_real_text_stream() {
    let out = run_responses(CODEX_TEXT, "gpt-6.1-sol");
    let names: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
    // 真实流的 output_text.done 会解析,但已有非空 delta 时不重复发文本。
    // content_part.added 和 response.metadata 也不产生 Anthropic 事件。
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    // usage:in=16,cached=0,cache_write=0 → net 16(F-2)。
    match &out[4] {
        AnthropicEvent::MessageDelta { delta, usage } => {
            assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
            let usage = usage.as_ref().unwrap();
            assert_eq!(usage.input_tokens, 16);
            assert_eq!(usage.output_tokens, 5);
            assert_eq!(usage.cache_read_input_tokens, Some(0));
            assert_eq!(usage.cache_creation_input_tokens, Some(0));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// 真实语料:工具调用流(codex_tool.sse)——参数以 9 个细粒度增量
/// 送达,F-1 严格保序拼接必须得到合法 JSON。
#[test]
fn f12_codex_real_tool_call() {
    let out = run_responses(CODEX_TOOL, "gpt-6.1-sol");
    let args: String = out
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    // 增量拼接是合法 JSON 且与 output_item.done 的全量一致(真实流:
    // {"city":"Beijing"})。
    let joined: serde_json::Value = serde_json::from_str(&args).expect("增量拼接须为合法 JSON");
    assert_eq!(joined["city"], "Beijing");
    // done 的全量参数不重复补发(真实流恰好 6 个增量,done 全量与
    // 拼接一致时 delta 是权威)。
    assert_eq!(
        out.iter()
            .filter(|e| matches!(
                e,
                AnthropicEvent::ContentBlockDelta {
                    delta: AnthropicDelta::InputJsonDelta { .. },
                    ..
                }
            ))
            .count(),
        6
    );
    assert!(matches!(
        &out[1],
        AnthropicEvent::ContentBlockStart {
            content_block: ContentBlockStart::ToolUse { id, name, .. },
            ..
        } if name == "get_weather" && id.starts_with("call_")
    ));
    // 有 function_call → stop_reason=tool_use;usage in=67。
    match &out[out.len() - 2] {
        AnthropicEvent::MessageDelta { delta, usage } => {
            assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
            assert_eq!(usage.as_ref().unwrap().input_tokens, 67);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// 真实语料:reasoning 加密签名 + 工具调用(codex_reasoning.sse)。
/// reasoning 项无文本增量,encrypted_content 只随 item 事件到达——
/// thinking 块以空内容开、签名先于块停落地(F-3 原样透传)。
#[test]
fn f12_codex_real_reasoning_signature() {
    let out = run_responses(CODEX_REASONING, "gpt-6.1-sol");
    let names: Vec<&str> = out.iter().map(|e| e.event_name()).collect();
    let mut expected = vec![
        "message_start",
        // thinking 块:start + 签名 delta + stop
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        // 工具块:start + 9 个参数增量(真实流细粒度切分)+ stop
        "content_block_start",
    ];
    expected.extend(std::iter::repeat_n("content_block_delta", 9));
    expected.extend(["content_block_stop", "message_delta", "message_stop"]);
    assert_eq!(names, expected);
    // thinking 块:唯一的 delta 是签名(真实语料 1976 字节密文,
    // 长度断言避免把密文写进测试源码)。
    match &out[2] {
        AnthropicEvent::ContentBlockDelta {
            delta: AnthropicDelta::SignatureDelta { signature },
            ..
        } => assert_eq!(signature.len(), 1976),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(matches!(
        &out[1],
        AnthropicEvent::ContentBlockStart {
            content_block: ContentBlockStart::Thinking { .. },
            ..
        }
    ));
    // 工具块参数增量拼接 = {"hours_after_3pm":3}。
    let args: String = out
        .iter()
        .filter_map(|e| match e {
            AnthropicEvent::ContentBlockDelta {
                delta: AnthropicDelta::InputJsonDelta { partial_json },
                ..
            } => Some(partial_json.clone()),
            _ => None,
        })
        .collect();
    let joined: serde_json::Value = serde_json::from_str(&args).unwrap();
    assert_eq!(joined["hours_after_3pm"], 3);
    // usage in=95, out=64,无缓存 → net 95。
    match &out[15] {
        AnthropicEvent::MessageDelta { delta, usage } => {
            assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
            assert_eq!(usage.as_ref().unwrap().input_tokens, 95);
            assert_eq!(usage.as_ref().unwrap().output_tokens, 64);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

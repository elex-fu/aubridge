//! Synthetic Gemini contract cases derived from Magpie's unnamed-call regression.
use proto::protocol::*;
use serde_json::{json, Value};
use translate::gemini_input::GeminiInput;
use translate::gemini_output::{frame, GeminiEncoder};

fn parse(body: Value) -> Result<GeminiInput, &'static str> {
    GeminiInput::parse("route/model", "streamGenerateContent", &body.to_string())
}

#[test]
fn gemini_same_name_parallel_calls_pair_fifo() {
    let input = parse(json!({"contents":[
        {"role":"user","parts":[{"text":"read both"}]},
        {"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"path":"a"}}},{"functionCall":{"name":"read_file","args":{"path":"b"}}}]},
        {"role":"user","parts":[{"functionResponse":{"name":"read_file","response":{"output":"file-a"}}},{"functionResponse":{"name":"read_file","response":{"error":"file-b"}}}]}]})).unwrap();
    let InboundContent::Blocks(calls) = &input.request.messages[1].content else {
        panic!()
    };
    let InboundContent::Blocks(results) = &input.request.messages[2].content else {
        panic!()
    };
    let ids: Vec<_> = calls
        .iter()
        .map(|b| match b {
            InboundBlock::ToolUse { id, .. } => id,
            _ => panic!(),
        })
        .collect();
    assert_ne!(ids[0], ids[1]);
    for (i, b) in results.iter().enumerate() {
        assert!(
            matches!(b,InboundBlock::ToolResult{tool_use_id,is_error,..} if tool_use_id == ids[i] && *is_error == (i==1))
        );
    }
}

#[test]
fn gemini_bad_pairing_and_google_opaque_state_are_rejected() {
    for body in [
        json!({"contents":[{"parts":[{"functionResponse":{"name":"x","response":{}}}]}]}),
        json!({"contents":[{"role":"model","parts":[{"functionCall":{"id":"a","name":"x","args":{}}}]},{"parts":[{"functionResponse":{"id":"a","name":"y","response":{}}}]}]}),
        json!({"contents":[{"parts":[{"text":"x","thoughtSignature":"opaque"}]}]}),
        json!({"contents":[{"parts":[{"fileData":{"mimeType":"image/png","fileUri":"https://generativelanguage.googleapis.com/v1beta/files/x"}}]}]}),
        json!({"cachedContent":"cachedContents/x","contents":[{"parts":[{"text":"x"}]}]}),
    ] {
        assert!(parse(body).is_err());
    }
    assert!(GeminiInput::parse("m", "generateContent", "").is_err());
}

#[test]
fn gemini_tools_schema_options_and_order_survive() {
    let input=parse(json!({"systemInstruction":{"parts":[{"text":"system"}]},"contents":[{"parts":[{"text":"first"},{"inlineData":{"mimeType":"text/plain","data":"ZG9j"}},{"text":"last"}]}],"tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"OBJECT","properties":{"UPPER":{"type":"STRING"}}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["read"]}},"generationConfig":{"maxOutputTokens":123,"temperature":0.2,"topP":0.8,"stopSequences":["END"]}})).unwrap();
    assert_eq!(input.request.max_tokens, 123);
    assert_eq!(input.request.system.as_ref().unwrap().as_text(), "system");
    assert_eq!(
        input.request.tools.as_ref().unwrap()[0].input_schema["properties"]["UPPER"]["type"],
        "string"
    );
    assert!(!input.supports(translate::media::Wire::Responses));
    let body = input
        .apply_options(translate::media::Wire::Chat, json!({}))
        .unwrap();
    assert_eq!(body["stop"], json!(["END"]));
}

fn event(value: Value) -> AnthropicEvent {
    serde_json::from_value(value).unwrap()
}

#[test]
fn gemini_stream_holds_parallel_args_in_open_order_and_usage() {
    let mut enc = GeminiEncoder::new("m");
    let mut out = Vec::new();
    for v in [
        json!({"type":"message_start","message":{"id":"r","model":"m","usage":{"input_tokens":2,"cache_read_input_tokens":3}}}),
        json!({"type":"content_block_start","index":8,"content_block":{"type":"tool_use","id":"a","name":"read","input":{}}}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"b","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"x\":2}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_delta","index":8,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
        json!({"type":"content_block_delta","index":8,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
        json!({"type":"content_block_stop","index":8}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ] {
        out.extend(enc.push_raw(&v));
    }
    let calls: Vec<_> = out
        .iter()
        .flat_map(|v| v["candidates"][0]["content"]["parts"].as_array().unwrap())
        .filter_map(|v| v.get("functionCall"))
        .collect();
    assert_eq!(
        calls
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(calls[0]["args"], json!({"x":1}));
    assert_eq!(out.last().unwrap()["usageMetadata"]["totalTokenCount"], 9);
    assert_eq!(out.last().unwrap()["candidates"][0]["finishReason"], "STOP");
    let wire = out.iter().map(frame).collect::<String>();
    assert!(
        !wire.contains("event:") && !wire.contains("[DONE]") && !wire.contains("thoughtSignature")
    );
    assert_eq!(
        enc.response().unwrap()["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn gemini_bad_args_empty_truncated_and_unknown_finish_never_stop() {
    for events in [
        vec![],
        vec![
            json!({"type":"message_delta","delta":{"stop_reason":"alien"}}),
            json!({"type":"message_stop"}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"x","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"[]"}}),
            json!({"type":"content_block_stop","index":0}),
        ],
    ] {
        let mut enc = GeminiEncoder::new("m");
        let mut out = Vec::new();
        for e in events {
            out.extend(enc.push(&event(e)));
        }
        out.extend(enc.finish());
        assert!(out.iter().any(|v| v.get("error").is_some()));
        assert!(out
            .iter()
            .all(|v| v["candidates"][0]["finishReason"] != "STOP"));
        assert!(enc.response().is_err());
    }
}

#[test]
fn gemini_explicit_ids_override_fifo_and_generated_ids_do_not_collide() {
    let input=parse(json!({"contents":[
        {"role":"model","parts":[{"functionCall":{"name":"x","args":{}}},{"functionCall":{"id":"gemini_call_1","name":"x","args":{}}}]},
        {"parts":[{"functionResponse":{"id":"gemini_call_1","name":"x","response":{"output":"second"}}},{"functionResponse":{"name":"x","response":{"output":"first"}}}]}]})).unwrap();
    let InboundContent::Blocks(blocks) = &input.request.messages[1].content else {
        panic!()
    };
    assert!(
        matches!(&blocks[0],InboundBlock::ToolResult{tool_use_id,..} if tool_use_id=="gemini_call_1")
    );
    assert!(
        matches!(&blocks[1],InboundBlock::ToolResult{tool_use_id,..} if tool_use_id=="gemini_call_2")
    );
    for responses in [
        json!([{"functionResponse":{"id":"a","name":"x","response":{}}},{"functionResponse":{"id":"a","name":"x","response":{}}}]),
        json!([{"functionResponse":{"id":"not-a","name":"x","response":{}}}]),
    ] {
        assert!(parse(json!({"contents":[{"role":"model","parts":[{"functionCall":{"id":"a","name":"x","args":{}}}]},{"parts":responses}]})).is_err());
    }
}

#[test]
fn gemini_finish_mapping_thoughts_known_usage_and_limits() {
    for (reason, expected) in [
        ("end_turn", "STOP"),
        ("tool_use", "STOP"),
        ("max_tokens", "MAX_TOKENS"),
        ("refusal", "SAFETY"),
    ] {
        let mut enc = GeminiEncoder::new("m");
        for value in [
            json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"unchanged","signature":"vendor-secret"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"vendor-secret"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":reason}}),
            json!({"type":"message_stop"}),
        ] {
            enc.push_raw(&value);
        }
        let response = enc.response().unwrap();
        assert_eq!(response["candidates"][0]["finishReason"], expected);
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0],
            json!({"text":"unchanged","thought":true})
        );
        assert!(response["usageMetadata"].get("totalTokenCount").is_none());
        assert!(response["usageMetadata"]
            .get("candidatesTokenCount")
            .is_none());
        assert!(!response.to_string().contains("vendor-secret"));
    }
    let mut enc = GeminiEncoder::new("m");
    enc.push_raw(&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"x","input":{}}}));
    let out=enc.push_raw(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"x".repeat(1024*1024+1)}}));
    assert!(out[0].get("error").is_some());
}

#[test]
fn gemini_count_wrapper_and_opaque_aliases() {
    let inner = json!({"model":"models/route/model","contents":[{"parts":[{"text":"count"}]}]});
    assert!(GeminiInput::parse(
        "route/model",
        "countTokens",
        &json!({"generateContentRequest":inner}).to_string()
    )
    .is_ok());
    assert!(GeminiInput::parse(
        "other",
        "countTokens",
        &json!({"generateContentRequest":inner}).to_string()
    )
    .is_err());
    assert!(GeminiInput::parse(
        "route/model",
        "generateContent",
        &json!({"generateContentRequest":inner}).to_string()
    )
    .is_err());
    assert!(GeminiInput::parse(
        "route/model",
        "countTokens",
        &json!({"generateContentRequest":inner,"contents":[]}).to_string()
    )
    .is_err());
    for part in [
        json!({"text":"x","thought_signature":"opaque"}),
        json!({"fileData":{"mimeType":"image/png","fileUri":"https://GENERATIVELANGUAGE.GOOGLEAPIS.COM.:443/v1beta/files/id"}}),
    ] {
        assert!(parse(json!({"contents":[{"parts":[part]}]})).is_err());
    }
    let mut enc = GeminiEncoder::new("alias");
    enc.push_raw(&json!({"type":"message_start","message":{"model":"served"}}));
    enc.push_raw(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}));
    enc.push_raw(&json!({"type":"message_stop"}));
    assert_eq!(enc.response().unwrap()["modelVersion"], "served");
}

#[test]
fn gemini_unsigned_thought_history_is_wholly_dropped() {
    let input=parse(json!({"contents":[
        {"role":"model","parts":[{"text":"private","thought":true}]},
        {"role":"model","parts":[{"text":"private-2","thought":true},{"text":"visible"},{"functionCall":{"id":"a","name":"x","args":{}}}]},
        {"parts":[{"functionResponse":{"id":"a","name":"x","response":{"output":"done"}}}]}]})).unwrap();
    assert_eq!(input.request.messages.len(), 2);
    let native = serde_json::to_value(&input.request).unwrap();
    let chat = serde_json::to_value(translate::messages_request_to_chat(&input.request)).unwrap();
    let responses =
        serde_json::to_value(translate::messages_request_to_responses(&input.request)).unwrap();
    for value in [native, chat, responses] {
        assert!(!value.to_string().contains("private"));
        assert!(!value.to_string().contains("signature"));
        assert!(value.to_string().contains("visible"));
        assert!(value.to_string().contains("done"));
    }
    assert!(parse(
        json!({"contents":[{"role":"model","parts":[{"text":"private","thought":true}]}]})
    )
    .is_err());
}

// Derived from the actual 0.68 recorder request-02 (154247): only private
// prompts, file paths/results and call IDs are replaced; SDK config/schema/marker stay.
const CLI_068: &str = include_str!("fixtures/gemini-cli-068-derived.json");
#[test]
fn gemini_real_cli_068_defaults_marker_and_per_target_top_k() {
    let input = GeminiInput::parse("m", "streamGenerateContent", CLI_068).unwrap();
    assert!(!input.supports(translate::media::Wire::Chat));
    assert!(!input.supports_target(translate::media::Wire::Responses, true));
    assert!(input.supports_target(translate::media::Wire::Chat, true));
    for wire in [
        translate::media::Wire::Messages,
        translate::media::Wire::Chat,
    ] {
        let body = input
            .apply_target_options(wire, serde_json::to_value(&input.request).unwrap(), true)
            .unwrap();
        assert_eq!(body["top_k"], 64);
        assert_eq!(body["temperature"], 1);
        assert_eq!(body["top_p"], 0.95);
        assert!(!body
            .to_string()
            .contains("skip_thought_signature_validator"));
        assert!(!body.to_string().contains("includeThoughts"));
    }
    for config in [
        json!({"includeThoughts":false}),
        json!({"includeThoughts":true,"thinkingBudget":-1}),
        json!({"includeThoughts":true,"thinkingLevel":"HIGH"}),
    ] {
        let mut raw: Value = serde_json::from_str(CLI_068).unwrap();
        raw["generationConfig"]["thinkingConfig"] = config;
        assert!(parse(raw).is_err());
    }
}

#[test]
fn gemini_sdk_marker_is_allowed_only_on_model_function_call() {
    let marker = "skip_thought_signature_validator";
    for part in [
        json!({"text":"x","thoughtSignature":marker}),
        json!({"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="},"thoughtSignature":marker}),
        json!({"functionCall":{"id":"a","name":"x","args":{}},"thoughtSignature":"opaque"}),
        json!({"functionCall":{"id":"a","name":"x","args":{}},"thoughtSignature":marker,"thought_signature":"opaque"}),
        json!({"functionCall":{"id":"a","name":"x","args":{}},"thoughtSignature":marker,"otherOpaque":"opaque"}),
        json!({"functionResponse":{"id":"a","name":"x","response":{}},"thoughtSignature":marker}),
    ] {
        assert_eq!(
            parse(json!({"contents":[{"role":"model","parts":[part]}]})).err(),
            Some("Google thoughtSignature cannot cross providers")
        );
    }
    assert_eq!(parse(json!({"contents":[{"parts":[{"functionCall":{"id":"a","name":"x","args":{}},"thoughtSignature":marker}]}]})).err(), Some("Google thoughtSignature cannot cross providers"));
}

#[test]
fn gemini_real_responses_tool_fixture_crosses_canonical_null_start() {
    // Existing recorded Responses bytes, not a hand-built canonical {} start.
    let (events, report) =
        translate::parse_responses_stream(include_str!("fixtures/codex_tool.sse"));
    assert!(!report.incomplete && report.bad_json.is_empty());
    let mut canonical = translate::ResponsesToAnthropic::new("gpt-6.1-sol");
    let mut all = Vec::new();
    for event in &events {
        all.extend(canonical.push(event));
    }
    all.extend(canonical.finish());
    assert!(all.iter().any(|event|matches!(event,AnthropicEvent::ContentBlockStart{content_block:ContentBlockStart::ToolUse{input,..},..} if input.is_null())));
    let mut encoder = GeminiEncoder::new("gpt-6.1-sol");
    let mut output = Vec::new();
    for event in &all {
        output.extend(encoder.push(event));
    }
    output.extend(encoder.finish());
    assert!(
        !encoder.failed(),
        "A canonical null start is a pending argument placeholder"
    );
    assert!(output.iter().all(|value| value.get("error").is_none()));
    let response = encoder.response().unwrap();
    assert_eq!(response["candidates"][0]["finishReason"], "STOP");
    let parts = response["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap();
    let calls: Vec<_> = parts
        .iter()
        .filter_map(|part| part.get("functionCall"))
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "get_weather");
    assert!(calls[0]["args"].is_object());
    assert!(calls[0]["args"]["city"].is_string());
    assert_eq!(
        output.last().unwrap()["candidates"][0]["finishReason"],
        "STOP"
    );
}

#[test]
fn gemini_null_tool_start_only_defaults_empty_args_after_valid_block_stop() {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"zero_args","input":null}});
    let stop = json!({"type":"content_block_stop","index":0});
    let reason = json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}});
    let end = json!({"type":"message_stop"});
    let mut encoder = GeminiEncoder::new("m");
    for event in [&start, &stop, &reason, &end] {
        encoder.push_raw(event);
    }
    assert_eq!(
        encoder.response().unwrap()["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
        json!({})
    );
    for partial in ["null", "[]", "{", ""] {
        let mut encoder = GeminiEncoder::new("m");
        let mut output = encoder.push_raw(&start);
        if !partial.is_empty() {
            output.extend(encoder.push_raw(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}})));
            output.extend(encoder.push_raw(&stop));
            output.extend(encoder.push_raw(&reason));
            output.extend(encoder.push_raw(&end));
        }
        output.extend(encoder.finish());
        assert!(encoder.failed());
        assert!(output.iter().any(|value| value.get("error").is_some()));
        assert!(output
            .iter()
            .all(|value| value["candidates"][0]["finishReason"] != "STOP"));
    }
}

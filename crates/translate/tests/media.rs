//! Small protocol-contract examples, not substitutes for recorded client fixtures.
use proto::protocol::MessagesRequest;
use serde_json::{json, Value};
use translate::media::{self, Wire};

fn request(content: Value) -> MessagesRequest {
    serde_json::from_value(
        json!({"model":"vision", "max_tokens":16, "messages":[{"role":"user", "content":content}]}),
    )
    .unwrap()
}
fn image() -> Value {
    json!({"type":"image","source":{"type":"url","url":"https://example.invalid/image.png"}})
}

#[test]
fn mixed_images_preserve_their_position_and_inline_mime() {
    let req =
        request(json!([{"type":"text","text":"before"},image(),{"type":"text","text":"after"}]));
    for wire in [Wire::Chat, Wire::Responses] {
        let body = media::encode(&req, wire, json!({})).unwrap();
        let key = if wire == Wire::Chat {
            "messages"
        } else {
            "input"
        };
        let parts = &body[key][0]["content"];
        assert_eq!(parts[0]["text"], "before");
        assert_eq!(parts[2]["text"], "after");
        assert_eq!(
            parts[1]["type"],
            if wire == Wire::Chat {
                "image_url"
            } else {
                "input_image"
            }
        );
    }
    let req = request(
        json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]),
    );
    assert!(media::check(&req, Wire::Messages, true).is_ok());
    let body = media::encode(&req, Wire::Chat, json!({})).unwrap();
    assert_eq!(
        body["messages"][0]["content"][0]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[test]
fn three_ingresses_retain_order_and_tool_images() {
    let chat = translate::chat_input::ChatInput::parse(&json!({"model":"vision", "messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"image_url","image_url":{"url":"https://example.invalid/i.png"}},{"type":"text","text":"b"}]}]}).to_string()).unwrap();
    assert!(!chat.native_only);
    assert!(media::has_images(&chat.request));
    assert_eq!(
        chat.for_anthropic(&chat.request).unwrap()["messages"][0]["content"][1],
        json!({"type":"image","source":{"type":"url","url":"https://example.invalid/i.png"}})
    );
    let responses = translate::responses_input::ResponsesInput::parse(&json!({"model":"vision","stream":true,"input":[{"type":"function_call","call_id":"c","name":"read","arguments":"{}"},{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"a"},{"type":"input_image","image_url":"https://example.invalid/i.png"},{"type":"input_text","text":"b"}]}]}).to_string()).unwrap();
    assert!(responses.compatible);
    assert!(media::has_images(&responses.request));
    assert_eq!(
        media::check(&responses.request, Wire::Chat, false),
        Err(media::UNSUPPORTED)
    );
    let body = media::encode(&responses.request, Wire::Responses, json!({})).unwrap();
    assert_eq!(body["input"][1]["type"], "function_call_output");
    assert_eq!(body["input"][1]["output"][1]["type"], "input_image");
    assert_eq!(body["input"][1]["output"][2]["text"], "b");
}

#[test]
fn invalid_and_opaque_sources_fail_without_text_placeholders() {
    for source in [
        json!({"type":"base64","media_type":"image/png","data":"!!!!"}),
        json!({"type":"base64","media_type":"image/jpeg","data":"iVBORw0KGgo="}),
        json!({"type":"url","url":"file:///tmp/private.png"}),
    ] {
        assert_eq!(
            media::check(
                &request(json!([{"type":"image","source":source}])),
                Wire::Messages,
                true
            ),
            Err(media::INVALID)
        );
    }
    let req = request(
        json!([{"type":"document","source":{"type":"file","file_id":"file-vendor-owned"}}]),
    );
    assert_eq!(
        media::check(&req, Wire::Messages, true),
        Err(media::UNSUPPORTED)
    );
}

#[test]
fn plain_document_preserves_filename_and_bytes_or_rejects_wire() {
    let req = request(
        json!([{"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"Hello\n世界"}}]),
    );
    assert_eq!(
        media::check(&req, Wire::Chat, false),
        Err(media::UNSUPPORTED)
    );
    let body = media::encode(&req, Wire::Responses, json!({})).unwrap();
    assert_eq!(body["input"][0]["content"][0]["filename"], "notes.txt");
    assert_eq!(
        body["input"][0]["content"][0]["file_data"],
        "data:text/plain;base64,SGVsbG8K5LiW55WM"
    );
}

#[test]
fn source_alternatives_are_exclusive_and_text_documents_are_bidirectional() {
    for part in [
        json!({"type":"input_image","image_url":"https://example.invalid/i.png","file_id":"foreign"}),
        json!({"type":"input_file","file_data":"data:text/plain;base64,aGk=","file_id":"foreign"}),
        json!({"type":"input_file","file_url":"https://example.invalid/a.txt"}),
    ] {
        assert!(media::parse_part(&part, Wire::Responses).is_err());
    }
    let block = media::parse_part(&json!({"type":"input_file","filename":"notes.txt","file_data":"data:text/plain;base64,SGVsbG8K5LiW55WM"}),Wire::Responses).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(block).unwrap(),
        json!({"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"Hello\n世界"}})
    );
    assert!(media::parse_part(
        &json!({"type":"input_file","file_data":"data:text/plain;base64,/w=="}),
        Wire::Responses
    )
    .is_err());
}

#[test]
fn image_history_keeps_parallel_tools_in_one_chat_assistant_batch() {
    let req: MessagesRequest = serde_json::from_value(json!({"model":"m","max_tokens":16,"messages":[
        {"role":"user","content":[image()]},
        {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}},{"type":"tool_use","id":"b","name":"read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"a"},{"type":"tool_result","tool_use_id":"b","content":"b"}]}
    ]})).unwrap();
    let body = media::encode(&req, Wire::Chat, json!({})).unwrap();
    assert_eq!(
        body["messages"][1]["tool_calls"].as_array().unwrap().len(),
        2
    );
    assert_eq!(body["messages"][2]["tool_call_id"], "a");
    assert_eq!(body["messages"][3]["tool_call_id"], "b");
    let orphan =
        request(json!([{"type":"tool_result","tool_use_id":"missing","content":[image()]}]));
    assert_eq!(
        media::check(&orphan, Wire::Responses, false),
        Err(media::UNSUPPORTED)
    );
}

#[test]
fn chat_media_conversion_rejects_interrupted_or_duplicate_tool_results() {
    for results in [
        json!([{"type":"text","text":"interrupt"},{"type":"tool_result","tool_use_id":"a","content":"ok"}]),
        json!([{"type":"tool_result","tool_use_id":"a","content":"ok"},{"type":"tool_result","tool_use_id":"a","content":"again"}]),
        json!([{"type":"tool_result","tool_use_id":"orphan","content":"ok"}]),
    ] {
        let req: MessagesRequest = serde_json::from_value(json!({"model":"m","max_tokens":16,"messages":[
            {"role":"user","content":[image()]},
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
            {"role":"user","content":results}
        ]})).unwrap();
        assert_eq!(
            media::check(&req, Wire::Chat, false),
            Err(media::UNSUPPORTED)
        );
        assert!(media::check(&req, Wire::Messages, true).is_ok());
    }
}

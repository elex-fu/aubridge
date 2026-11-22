use serde_json::Value;
#[derive(Default, Debug)]
pub(super) struct Transcript {
    pub session: Option<String>,
    pub tool_started: bool,
    pub tool_result_matched: bool,
    pub final_exact: bool,
    pub successful: bool,
}
pub(super) fn parse(agent: &str, output: &str, nonce: &str) -> Transcript {
    let mut result = Transcript::default();
    let mut final_text = String::new();
    let mut failed = false;
    let mut tool_ids = std::collections::HashSet::new();
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        match agent {
            "gemini" => match event["type"].as_str() {
                Some("init") => result.session = event["session_id"].as_str().map(str::to_owned),
                Some("tool_use") => {
                    if event["tool_name"] == "read_file"
                        && event["parameters"].to_string().contains("challenge.txt")
                    {
                        if let Some(id) = event["tool_id"].as_str() {
                            tool_ids.insert(id.to_string());
                            result.tool_started = true;
                        }
                    }
                }
                Some("tool_result") => {
                    if event["tool_id"]
                        .as_str()
                        .is_some_and(|id| tool_ids.contains(id))
                        && event["status"] == "success"
                    {
                        result.tool_result_matched = true;
                    }
                    final_text.clear();
                }
                Some("message") if event["role"] == "assistant" => {
                    if let Some(text) = event["content"].as_str() {
                        final_text.push_str(text);
                    }
                }
                Some("result") => {
                    result.successful = event["status"] == "success";
                    failed |= !result.successful;
                }
                Some("error") => failed = true,
                _ => {}
            },
            "codex" => match event["type"].as_str() {
                Some("thread.started") => {
                    result.session = event["thread_id"].as_str().map(str::to_owned)
                }
                Some("item.started" | "item.completed") => {
                    let item = &event["item"];
                    if item["type"] == "command_execution"
                        && item["command"]
                            .as_str()
                            .is_some_and(|s| s.contains("challenge.txt"))
                    {
                        if let Some(id) = item["id"].as_str() {
                            tool_ids.insert(id.to_string());
                            result.tool_started = true;
                        }
                        if event["type"] == "item.completed"
                            && item["exit_code"] == 0
                            && item["aggregated_output"]
                                .as_str()
                                .is_some_and(|s| s.trim() == nonce)
                        {
                            result.tool_result_matched = true;
                        }
                    }
                    if event["type"] == "item.completed" && item["type"] == "agent_message" {
                        final_text = item["text"].as_str().unwrap_or_default().into();
                    }
                }
                Some("turn.completed") => result.successful = true,
                Some("error" | "turn.failed") => failed = true,
                _ => {}
            },
            "claude-code" => {
                if let Some(s) = event["session_id"].as_str() {
                    result.session = Some(s.into());
                }
                match event["type"].as_str() {
                    Some("assistant") => {
                        if let Some(blocks) = event["message"]["content"].as_array() {
                            for b in blocks {
                                if b["type"] == "tool_use"
                                    && b["name"] == "Read"
                                    && b["input"].to_string().contains("challenge.txt")
                                {
                                    if let Some(id) = b["id"].as_str() {
                                        tool_ids.insert(id.to_owned());
                                        result.tool_started = true;
                                    }
                                }
                            }
                        }
                    }
                    Some("user") => {
                        if let Some(blocks) = event["message"]["content"].as_array() {
                            for b in blocks {
                                if b["type"] == "tool_result"
                                    && b["is_error"] != true
                                    && b["tool_use_id"]
                                        .as_str()
                                        .is_some_and(|id| tool_ids.contains(id))
                                {
                                    result.tool_result_matched = true;
                                }
                            }
                        }
                    }
                    Some("result") => {
                        result.successful =
                            event["subtype"] == "success" && event["is_error"] != true;
                        failed |= !result.successful;
                        final_text = event["result"].as_str().unwrap_or_default().into();
                    }
                    Some("error") => failed = true,
                    _ => {}
                }
            }
            "opencode" => {
                if let Some(s) = event["sessionID"].as_str() {
                    result.session = Some(s.into());
                }
                let part = &event["part"];
                match event["type"].as_str() {
                    Some("tool_use") => {
                        if part["tool"] == "read"
                            && part["state"]["input"].to_string().contains("challenge.txt")
                        {
                            result.tool_started = true;
                            if part["state"]["status"] == "completed" {
                                result.tool_result_matched = true;
                            }
                        }
                    }
                    Some("text") => final_text = part["text"].as_str().unwrap_or_default().into(),
                    Some("step_finish") => result.successful = part["reason"] == "stop",
                    Some("error") => failed = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    result.final_exact = final_text.trim() == nonce;
    result.successful &= !failed;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn answer_without_successful_tool_result_is_not_file_evidence() {
        let p=parse("gemini", "{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"nonce\"}\n{\"type\":\"result\",\"status\":\"success\"}", "nonce");
        assert!(p.final_exact && p.successful);
        assert!(!p.tool_started && !p.tool_result_matched);
    }
    #[test]
    fn tool_result_from_unrelated_call_and_late_error_fail_closed() {
        let p=parse("gemini", "{\"type\":\"tool_use\",\"tool_id\":\"a\",\"tool_name\":\"read_file\",\"parameters\":{\"path\":\"challenge.txt\"}}\n{\"type\":\"tool_result\",\"tool_id\":\"b\",\"status\":\"success\",\"output\":\"nonce\"}\n{\"type\":\"result\",\"status\":\"success\"}\n{\"type\":\"error\"}", "nonce");
        assert!(p.tool_started);
        assert!(!p.tool_result_matched && !p.successful);
    }
    #[test]
    fn gemini_failed_terminal_cannot_be_overwritten_by_later_success() {
        let output = [
            serde_json::json!({"type":"init","session_id":"session-fixture"}),
            serde_json::json!({"type":"tool_use","tool_id":"read-1","tool_name":"read_file","parameters":{"file_path":"challenge.txt"}}),
            serde_json::json!({"type":"tool_result","tool_id":"read-1","status":"success"}),
            serde_json::json!({"type":"result","status":"error"}),
            serde_json::json!({"type":"message","role":"assistant","content":"fixture-nonce"}),
            serde_json::json!({"type":"result","status":"success"}),
        ].map(|event| event.to_string()).join("\n");
        let transcript = parse("gemini", &output, "fixture-nonce");
        assert!(
            transcript.tool_started && transcript.tool_result_matched && transcript.final_exact
        );
        assert!(
            !transcript.successful,
            "a failed terminal result must remain failed"
        );
    }

    #[test]
    fn claude_failed_terminal_cannot_be_overwritten_by_later_success() {
        let output = [
            serde_json::json!({"type":"assistant","session_id":"session-fixture","message":{"content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"challenge.txt"}}]}}),
            serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"read-1","content":"fixture-nonce","is_error":false}]}}),
            serde_json::json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"fixture-nonce"}),
            serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":"fixture-nonce"}),
        ].map(|event| event.to_string()).join("\n");
        let transcript = parse("claude-code", &output, "fixture-nonce");
        assert!(
            transcript.tool_started && transcript.tool_result_matched && transcript.final_exact
        );
        assert!(
            !transcript.successful,
            "a failed terminal result must remain failed"
        );
    }
    // Recorded 0.70.0 stream shapes only. Native record continuity is tested separately.
    #[test]
    fn gemini_070_recorded_seed_and_resume_stream_shapes_preserve_file_evidence() {
        for (output, expected) in [
            (
                include_str!("../../tests/fixtures/gemini-0.70.0-seed-stream.jsonl"),
                "fixture-seed-xxxxxxxxxxxxxxxxx",
            ),
            (
                include_str!("../../tests/fixtures/gemini-0.70.0-resume-stream.jsonl"),
                "fixture-resume-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            ),
        ] {
            let parsed = parse("gemini", output, expected);
            assert_eq!(parsed.session.as_deref(), Some("fixture-session-070"));
            assert!(
                parsed.tool_started
                    && parsed.tool_result_matched
                    && parsed.final_exact
                    && parsed.successful
            );
        }
    }
    #[test]
    fn gemini_070_recorded_stream_does_not_borrow_an_unrelated_tool_result() {
        let original = include_str!("../../tests/fixtures/gemini-0.70.0-seed-stream.jsonl");
        let output = original
            .lines()
            .map(|line| {
                let mut event: Value = serde_json::from_str(line).unwrap();
                if event["type"] == "tool_result" {
                    event["tool_id"] = serde_json::json!("unrelated-call");
                }
                event.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse("gemini", &output, "fixture-seed-xxxxxxxxxxxxxxxxx");
        assert!(parsed.tool_started && parsed.final_exact && parsed.successful);
        assert!(!parsed.tool_result_matched);
    }
}

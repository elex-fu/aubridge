use agents::native_sessions::{inspect_jsonl, NativeAgent, NativeIssue};
use std::path::Path;

#[test]
fn codex_supported_literal_metadata_versions_remain_separate() {
    let file = Path::new("rollout-2026-c18365a3-0000-4000-8000-000000000001.jsonl");
    for bytes in [
        include_bytes!("fixtures/native-codex-0.169.1.jsonl").as_slice(),
        include_bytes!("fixtures/native-codex-0.169.0-alpha.11.1.jsonl").as_slice(),
    ] {
        assert_eq!(
            inspect_jsonl(NativeAgent::Codex, file, bytes)
                .unwrap()
                .native_id,
            "c18365a3-0000-4000-8000-000000000001"
        );
    }
}

#[test]
fn diagnostic_origin_cannot_replace_codex_native_metadata() {
    let input = br#"{"type":"session_meta","payload":{"id":"abc","cwd":"/tmp","model_provider":"aubridge"}}"#;
    assert_eq!(
        inspect_jsonl(NativeAgent::Codex, Path::new("rollout-abc.jsonl"), input).unwrap_err(),
        NativeIssue::InvalidSessionId
    );
}

#[test]
fn codex_requires_matching_rollout_uuid_and_managed_provider() {
    let id = "c18365a3-0000-4000-8000-000000000001";
    let input = format!(
        r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"/tmp","source":"cli","cli_version":"0.169.0-alpha.11.1","model_provider":"aubridge"}}}}"#
    );
    let record = inspect_jsonl(
        NativeAgent::Codex,
        Path::new(&format!("rollout-2026-11-19-{id}.jsonl")),
        input.as_bytes(),
    )
    .unwrap();
    assert_eq!(record.native_id, id);
    let mismatch = Path::new("rollout-2026-11-19-c18365a3-0000-4000-8000-000000000002.jsonl");
    assert_eq!(
        inspect_jsonl(NativeAgent::Codex, mismatch, input.as_bytes()).unwrap_err(),
        NativeIssue::IdentityMismatch
    );
    assert_eq!(
        inspect_jsonl(
            NativeAgent::Codex,
            Path::new(&format!("rollout-{id}.jsonl")),
            input.replace("aubridge", "openai").as_bytes()
        )
        .unwrap_err(),
        NativeIssue::ProfileMismatch
    );
    let child = input.replace(
        "\"source\":\"cli\"",
        "\"source\":\"exec\",\"parent_thread_id\":\"parent\"",
    );
    assert_eq!(
        inspect_jsonl(
            NativeAgent::Codex,
            Path::new(&format!("rollout-{id}.jsonl")),
            child.as_bytes()
        )
        .unwrap_err(),
        NativeIssue::ChildSession
    );
}

#[test]
fn claude_sidechains_and_ambiguous_roots_cannot_resume() {
    let id = "c18365a3-0000-4000-8000-000000000001";
    let path = format!("{id}.jsonl");
    let row = format!(r#"{{"sessionId":"{id}","cwd":"/tmp","isSidechain":true}}"#);
    assert_eq!(
        inspect_jsonl(NativeAgent::Claude, Path::new(&path), row.as_bytes()).unwrap_err(),
        NativeIssue::ChildSession
    );
    let rows = format!(
        "{}\n{}",
        row.replace("true", "false"),
        row.replace("true", "false").replace("/tmp", "/other")
    );
    assert_eq!(
        inspect_jsonl(NativeAgent::Claude, Path::new(&path), rows.as_bytes()).unwrap_err(),
        NativeIssue::IdentityMismatch
    );
}

#[test]
fn cwd_is_required_and_shell_text_is_not_an_id() {
    let row = br#"{"sessionId":"$(touch surprise)","cwd":"/tmp","isSidechain":false}"#;
    assert_eq!(
        inspect_jsonl(NativeAgent::Claude, Path::new("a.jsonl"), row).unwrap_err(),
        NativeIssue::InvalidSessionId
    );
    let row = br#"{"sessionId":"c18365a3-0000-4000-8000-000000000001","isSidechain":false}"#;
    assert_eq!(
        inspect_jsonl(
            NativeAgent::Claude,
            Path::new("c18365a3-0000-4000-8000-000000000001.jsonl"),
            row
        )
        .unwrap_err(),
        NativeIssue::WorkspaceMissing
    );
}

#[test]
fn bounded_inspection_does_not_accept_partial_or_oversized_records() {
    assert_eq!(
        inspect_jsonl(NativeAgent::Codex, Path::new("a.jsonl"), b"{bad json").unwrap_err(),
        NativeIssue::UnsupportedSchema
    );
    let oversized = vec![b' '; 8 * 1024 * 1024 + 1];
    assert_eq!(
        inspect_jsonl(NativeAgent::Claude, Path::new("a.jsonl"), &oversized).unwrap_err(),
        NativeIssue::ScanLimit
    );
}

#[test]
fn changed_native_file_and_replaced_workspace_invalidate_candidate() {
    use agents::native_sessions::read_jsonl;
    let root = std::env::temp_dir().join(format!("aubridge-native-cas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    let id = "c18365a3-0000-4000-8000-000000000001";
    let file = root.join(format!("{id}.jsonl"));
    let row = serde_json::json!({"sessionId":id,"cwd":root.join("project"),"isSidechain":false})
        .to_string();
    std::fs::write(&file, &row).unwrap();
    let record = read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
    std::fs::write(&file, format!("{row}\n")).unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    let record = read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
    std::fs::rename(root.join("project"), root.join("previous-project")).unwrap();
    std::fs::create_dir(root.join("project")).unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn native_record_symlink_cannot_escape_trusted_storage() {
    use agents::native_sessions::read_jsonl;
    let root = std::env::temp_dir().join(format!("aubridge-native-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("trusted")).unwrap();
    std::fs::write(root.join("outside.jsonl"), "{}").unwrap();
    std::os::unix::fs::symlink(
        root.join("outside.jsonl"),
        root.join("trusted/linked.jsonl"),
    )
    .unwrap();
    assert_eq!(
        read_jsonl(
            NativeAgent::Claude,
            &root.join("trusted"),
            &root.join("trusted/linked.jsonl")
        )
        .unwrap_err(),
        NativeIssue::UnsafePath
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sealed_history_requires_a_checked_owner_and_unknown_signatures_fail_closed() {
    use agents::native_sessions::read_jsonl;
    let root = std::env::temp_dir().join(format!("aubridge-native-sealed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let id = "c18365a3-0000-4000-8000-000000000001";
    let file = root.join(format!("{id}.jsonl"));
    let row = serde_json::json!({"sessionId":id,"cwd":root,"isSidechain":false}).to_string();
    std::fs::write(
        &file,
        format!(
            "{row}\n{}",
            serde_json::json!({"type":"compaction","encrypted_content":"opaque-test"})
        ),
    )
    .unwrap();
    let record = read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
    assert!(record.metadata.contains_opaque);
    assert_eq!(
        record
            .check_sealed(|_| Err(NativeIssue::OpaqueUnverified))
            .unwrap_err(),
        NativeIssue::OpaqueUnverified
    );
    let mut checked = 0;
    record
        .check_sealed(|item| {
            assert_eq!(item["type"], "compaction");
            checked += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(checked, 1);
    std::fs::write(
        &file,
        format!(
            "{row}\n{}",
            serde_json::json!({"signature":"unknown-sealed-shape"})
        ),
    )
    .unwrap();
    let record = read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
    assert_eq!(
        record.check_sealed(|_| Ok(())).unwrap_err(),
        NativeIssue::OpaqueUnverified
    );
    std::fs::write(
        &file,
        format!(
            "{row}\n{}",
            serde_json::json!({"providerMetadata":{"newSealedShape":"opaque"}})
        ),
    )
    .unwrap();
    let record = read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
    assert!(record.metadata.contains_opaque);
    assert_eq!(
        record.check_sealed(|_| Ok(())).unwrap_err(),
        NativeIssue::OpaqueUnverified
    );
    std::fs::remove_dir_all(root).unwrap();
}

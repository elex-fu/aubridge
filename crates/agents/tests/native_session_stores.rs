use agents::native_sessions::{scan_gemini, scan_opencode, NativeIssue};
use sha2::{Digest, Sha256};

#[test]
fn opencode_reads_wal_and_rechecks_exact_root_row_without_writing() {
    let dir = std::env::temp_dir().join(format!("aubridge-opencode-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("project")).unwrap();
    let db = dir.join("opencode.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,parent_id TEXT,fork_session_id TEXT,model TEXT,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session_part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,data TEXT);").unwrap();
    c.execute(
        "INSERT INTO session_v2 VALUES(?1,?2,NULL,NULL,?3,1000)",
        rusqlite::params![
            "ses_Root",
            dir.join("project").to_str().unwrap(),
            r#"{"providerID":"aubridge","id":"model","variant":""}"#
        ],
    )
    .unwrap();
    c.execute(
        "INSERT INTO session_message VALUES('msg1','ses_Root',?1)",
        [r#"{"metadata":{"model":{"providerID":"aubridge","modelID":"model"}}}"#],
    )
    .unwrap();
    let scan = scan_opencode(&dir, &db);
    assert!(scan.issues.is_empty());
    assert_eq!(scan.records.len(), 1);
    let record = &scan.records[0];
    assert_eq!(record.metadata.native_id, "ses_Root");
    assert!(!record.metadata.contains_opaque);
    c.execute(
        "UPDATE session_v2 SET parent_id='ses_Parent' WHERE id='ses_Root'",
        [],
    )
    .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::ChildSession);
    assert!(scan_opencode(&dir, &db).records.is_empty());
    drop(c);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn gemini_requires_registry_hash_explicit_main_and_direct_chat_file() {
    let dir = std::env::temp_dir().join(format!("aubridge-gemini-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tmp/project/chats")).unwrap();
    std::fs::create_dir(dir.join("workspace")).unwrap();
    let cwd = std::fs::canonicalize(dir.join("workspace")).unwrap();
    let hash = format!("{:x}", Sha256::digest(cwd.to_str().unwrap().as_bytes()));
    std::fs::write(
        dir.join("projects.json"),
        serde_json::json!({"projects":{cwd.to_str().unwrap():"project"}}).to_string(),
    )
    .unwrap();
    let id = "c18365a3-0000-4000-8000-000000000001";
    let file = dir.join("tmp/project/chats/session-2026-c18365a3.jsonl");
    let metadata = serde_json::json!({"sessionId":id,"projectHash":hash,"kind":"main","startTime":"2026-11-19T00:00:00Z"});
    std::fs::write(&file, metadata.to_string()).unwrap();
    let scan = scan_gemini(&dir);
    assert!(scan.issues.is_empty());
    assert_eq!(scan.records.len(), 1);
    assert_eq!(scan.records[0].canonical_cwd, cwd);
    let mut legacy = metadata.clone();
    legacy.as_object_mut().unwrap().remove("kind");
    std::fs::write(&file, legacy.to_string()).unwrap();
    let scan = scan_gemini(&dir);
    assert!(scan.records.is_empty());
    assert!(scan.issues.contains(&NativeIssue::UnsupportedSchema));
    let mut child = metadata;
    child["kind"] = serde_json::json!("subagent");
    std::fs::write(&file, child.to_string()).unwrap();
    assert!(scan_gemini(&dir).records.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn gemini_literal_metadata_versions_share_schema_and_fail_closed_on_wrong_project_or_child() {
    let provenance: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native-gemini-provenance.json")).unwrap();
    for (version, literal) in [
        (
            "0.68.0",
            include_str!("fixtures/native-gemini-0.68.0.jsonl"),
        ),
        (
            "0.70.0",
            include_str!("fixtures/native-gemini-0.70.0.jsonl"),
        ),
    ] {
        assert_eq!(
            format!("{:x}", Sha256::digest(literal.as_bytes())),
            provenance["fixtures"][version]["header_sha256"]
                .as_str()
                .unwrap()
        );
        let mut metadata: serde_json::Value = serde_json::from_str(literal).unwrap();
        assert_eq!(metadata.as_object().unwrap().len(), 5);
        assert!(
            metadata.get("cli_version").is_none(),
            "the source version is receipt provenance, not a native header field"
        );
        let id = metadata["sessionId"].as_str().unwrap().to_owned();
        let dir = std::env::temp_dir().join(format!(
            "aubridge-gemini-literal-{version}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let chats = dir.join("tmp/literal-project/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::create_dir(dir.join("workspace")).unwrap();
        let cwd = std::fs::canonicalize(dir.join("workspace")).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            serde_json::json!({"projects":{cwd.to_str().unwrap():"literal-project"}}).to_string(),
        )
        .unwrap();
        // Only the source project's hash needs rebinding to this isolated test workspace.
        metadata["projectHash"] = serde_json::json!(format!(
            "{:x}",
            Sha256::digest(cwd.to_str().unwrap().as_bytes())
        ));
        let file = chats.join(format!("session-2026-{}.jsonl", &id[..8]));
        std::fs::write(&file, metadata.to_string()).unwrap();
        let mut scan = scan_gemini(&dir);
        assert!(scan.issues.is_empty(), "{version}: {:?}", scan.issues);
        assert_eq!(scan.records.len(), 1);
        let record = scan.records.remove(0);
        assert_eq!(record.metadata.native_id, id);
        assert_eq!(record.canonical_cwd, cwd);
        record.recheck().unwrap();
        let mut wrong_project = metadata.clone();
        wrong_project["projectHash"] = serde_json::json!("0".repeat(64));
        std::fs::write(&file, wrong_project.to_string()).unwrap();
        let scan = scan_gemini(&dir);
        assert!(scan.records.is_empty());
        assert!(scan.issues.contains(&NativeIssue::IdentityMismatch));
        assert_eq!(record.recheck().unwrap_err(), NativeIssue::IdentityMismatch);
        metadata["kind"] = serde_json::json!("subagent");
        std::fs::write(&file, metadata.to_string()).unwrap();
        let scan = scan_gemini(&dir);
        assert!(scan.records.is_empty());
        assert!(scan.issues.contains(&NativeIssue::ChildSession));
        metadata.as_object_mut().unwrap().remove("kind");
        std::fs::write(&file, metadata.to_string()).unwrap();
        let scan = scan_gemini(&dir);
        assert!(scan.records.is_empty());
        assert!(scan.issues.contains(&NativeIssue::UnsupportedSchema));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

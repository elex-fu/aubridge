use agents::native_sessions::{scan_opencode, NativeIssue, NativeRecord};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;

struct Case {
    root: PathBuf,
    db: PathBuf,
    connection: Connection,
}
impl Case {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "aubridge-opencode-v2-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        let db = root.join("opencode.db");
        let connection = Connection::open(&db).unwrap();
        // This synthetic three-table fixture deliberately includes orphan parts.
        // Keep the captured DDL intact; only this test connection disables FKs.
        connection
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA journal_mode=WAL;")
            .unwrap();
        connection
            .execute_batch(include_str!("fixtures/native-opencode-2.2.5-v2.sql"))
            .unwrap();
        connection.execute("INSERT INTO session_v2(id,project_id,slug,directory,version,time_created,time_updated,time_busy,time_idle) VALUES('ses_Root','project','fixture',?1,'2.2.5',1000,2000,1000,2000)", [root.join("project").to_str().unwrap()]).unwrap();
        Self {
            root,
            db,
            connection,
        }
    }
    fn message(&self, id: &str, seq: i64, kind: &str, value: Value) {
        self.connection.execute("INSERT INTO session_message(id,session_id,type,seq,time_created,time_updated,data) VALUES(?1,'ses_Root',?2,?3,1000,1000,?4)", params![id,kind,seq,value.to_string()]).unwrap();
    }
    fn assistants(&self) {
        self.message("msg_seed", 5, "assistant", json!({"agent":"build","model":{"providerID":"aubridge","id":"first"},"finish":"tool-calls","content":[]}));
        self.message("msg_stop", 14, "assistant", json!({"agent":"build","model":{"providerID":"aubridge","id":"second"},"finish":"stop","content":[]}));
    }
    fn part(&self, ordinal: i64, value: Value) {
        // No matching message on purpose: an independent cursor must not lose it.
        self.connection.execute("INSERT INTO session_part(message_id,session_id,ordinal,type,time_created,time_updated,data) VALUES('msg_orphan','ses_Root',?1,'text',1000,1000,?2)", params![ordinal,value.to_string()]).unwrap();
    }
    fn record(&self) -> NativeRecord {
        let mut scan = scan_opencode(&self.root, &self.db);
        assert!(scan.issues.is_empty(), "{:?}", scan.issues);
        assert_eq!(scan.records.len(), 1);
        scan.records.remove(0)
    }
    fn rejected(&self, issue: NativeIssue) {
        let scan = scan_opencode(&self.root, &self.db);
        assert!(scan.records.is_empty());
        assert!(scan.issues.contains(&issue), "{:?}", scan.issues);
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn literal_v2_null_header_uses_all_assistants_and_allows_same_provider_model_changes() {
    let case = Case::new("null-root");
    case.message(
        "msg_user",
        4,
        "user",
        json!({"model":{"providerID":"foreign","id":"fake"}}),
    );
    case.assistants();
    case.message("msg_idle", 19, "idle", json!({"outcome":"success"}));
    let record = case.record();
    assert_eq!(record.metadata.native_id, "ses_Root");
    assert!(!record.metadata.contains_opaque);
    record.recheck().unwrap();
    case.connection
        .execute(
            "UPDATE session_v2 SET model=?1",
            [r#"{"providerID":"aubridge","id":"second"}"#],
        )
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
}

#[test]
fn null_header_refuses_missing_mixed_and_content_only_provider_claims() {
    for (name, value) in [
        ("missing", json!({"agent":"aubridge","content":[]})),
        (
            "foreign",
            json!({"model":{"providerID":"foreign","id":"other"}}),
        ),
        ("empty", json!({"model":{"providerID":"aubridge","id":""}})),
        (
            "nested",
            json!({"content":[{"model":{"providerID":"aubridge","id":"fake"}}]}),
        ),
    ] {
        let case = Case::new(name);
        case.assistants();
        case.message("msg_unproven", 20, "assistant", value);
        case.rejected(NativeIssue::ProfileMismatch);
    }
    let case = Case::new("no-assistant");
    case.message(
        "msg_user",
        1,
        "user",
        json!({"model":{"providerID":"aubridge","id":"fake"}}),
    );
    case.part(0, json!({"model":{"providerID":"aubridge","id":"fake"}}));
    case.rejected(NativeIssue::ProfileMismatch);
}

#[test]
fn explicit_bad_header_never_falls_back_and_null_requires_exact_native_version() {
    let case = Case::new("header");
    case.assistants();
    for header in [r#"{"providerID":"foreign","id":"other"}"#, "null", "{}"] {
        case.connection
            .execute("UPDATE session_v2 SET model=?1", [header])
            .unwrap();
        case.rejected(NativeIssue::ProfileMismatch);
    }
    case.connection
        .execute("UPDATE session_v2 SET model='broken'", [])
        .unwrap();
    case.rejected(NativeIssue::UnsupportedSchema);
    case.connection
        .execute("UPDATE session_v2 SET model=NULL,version='2.2.6'", [])
        .unwrap();
    case.rejected(NativeIssue::UnsupportedSchema);
}

#[test]
fn composite_parts_are_independently_checked_for_opaque_content_and_drift() {
    let case = Case::new("parts");
    case.assistants();
    case.part(0, json!({"text":"safe fixture"}));
    let record = case.record();
    record
        .check_sealed(|_| panic!("no sealed checkpoint"))
        .unwrap();
    case.connection
        .execute("UPDATE session_part SET ordinal=1", [])
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    let record = case.record();
    case.connection
        .execute(
            "UPDATE session_part SET data=?1",
            [json!({"providerMetadata":{"fixture":true}}).to_string()],
        )
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    let record = case.record();
    assert!(record.metadata.contains_opaque);
    assert_eq!(
        record.check_sealed(|_| Ok(())).unwrap_err(),
        NativeIssue::OpaqueUnverified
    );
}

#[test]
fn snapshot_includes_message_order_and_shared_message_part_row_budget() {
    let case = Case::new("budget");
    case.assistants();
    let record = case.record();
    case.connection
        .execute("UPDATE session_message SET seq=15 WHERE id='msg_stop'", [])
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    case.connection.execute_batch("BEGIN").unwrap();
    for index in 0..4093 {
        case.message(
            &format!("msg_extra_{index}"),
            100 + index,
            "user",
            json!({}),
        );
    }
    case.part(0, json!({}));
    case.part(1, json!({}));
    case.connection.execute_batch("COMMIT").unwrap();
    case.rejected(NativeIssue::ScanLimit);
}

#[test]
fn legacy_parts_need_only_native_id_even_when_message_id_is_null_or_absent() {
    for (name, optional_column) in [("null-message", ",message_id TEXT"), ("no-message", "")] {
        let root = std::env::temp_dir().join(format!(
            "aubridge-opencode-legacy-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        let db = root.join("opencode.db");
        let connection = Connection::open(&db).unwrap();
        connection.execute_batch(&format!("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,parent_id TEXT,fork_session_id TEXT,model TEXT,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT); CREATE TABLE session_part(id TEXT PRIMARY KEY,session_id TEXT,data TEXT{optional_column});")).unwrap();
        connection
            .execute(
                "INSERT INTO session_v2 VALUES('ses_Root',?1,NULL,NULL,?2,1000)",
                params![
                    root.join("project").to_str().unwrap(),
                    r#"{"providerID":"aubridge","id":"legacy"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_part(id,session_id,data) VALUES('part1','ses_Root',?1)",
                [json!({"text":"legacy fixture"}).to_string()],
            )
            .unwrap();
        let mut scan = scan_opencode(&root, &db);
        assert!(scan.issues.is_empty(), "{name}: {:?}", scan.issues);
        assert_eq!(scan.records.len(), 1);
        let record = scan.records.remove(0);
        record.recheck().unwrap();
        connection
            .execute(
                "UPDATE session_part SET data=?1",
                [json!({"providerMetadata":{"fixture":true}}).to_string()],
            )
            .unwrap();
        assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
        let record = scan_opencode(&root, &db).records.remove(0);
        assert!(record.metadata.contains_opaque);
        assert_eq!(
            record.check_sealed(|_| Ok(())).unwrap_err(),
            NativeIssue::OpaqueUnverified
        );
        connection.execute_batch("BEGIN").unwrap();
        for index in 0..4096 {
            connection
                .execute(
                    "INSERT INTO session_part(id,session_id,data) VALUES(?1,'ses_Root','{}')",
                    [format!("extra{index}")],
                )
                .unwrap();
        }
        connection.execute_batch("COMMIT").unwrap();
        let scan = scan_opencode(&root, &db);
        assert!(scan.records.is_empty());
        assert!(scan.issues.contains(&NativeIssue::ScanLimit));
        drop(connection);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn nullable_agent_is_not_provider_evidence_but_agent_only_changes_invalidate_cas() {
    let case = Case::new("agent-cas");
    case.assistants();
    let record = case.record();
    case.connection
        .execute("UPDATE session_v2 SET agent='foreign-role'", [])
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
    let record = case.record();
    case.connection
        .execute("UPDATE session_v2 SET agent=NULL", [])
        .unwrap();
    assert_eq!(record.recheck().unwrap_err(), NativeIssue::RecordChanged);
}

#[test]
fn v2_fallback_requires_real_message_and_composite_part_primary_keys() {
    for (name, table, ddl) in [
        ("no-message-pk", "session_message", "CREATE TABLE session_message(id TEXT,session_id TEXT,type TEXT,seq INTEGER,data TEXT)"),
        ("reverse-part-pk", "session_part", "CREATE TABLE session_part(message_id TEXT,session_id TEXT,ordinal INTEGER,type TEXT,data TEXT,PRIMARY KEY(ordinal,message_id))"),
    ] {
        let case = Case::new(name);
        case.connection.execute_batch(&format!("DROP TABLE {table}; {ddl};")).unwrap();
        if table == "session_message" {
            case.connection.execute("INSERT INTO session_message VALUES('msg_assistant','ses_Root','assistant',1,?1)", [json!({"model":{"providerID":"aubridge","id":"fixture"}}).to_string()]).unwrap();
        } else {
            case.assistants();
        }
        case.rejected(NativeIssue::UnsupportedSchema);
    }
}

#[test]
fn sql_bounds_multibyte_keys_and_sql_types_before_returning_row_values() {
    for (name, table, field) in [
        ("message-key", "session_message", "id"),
        ("message-type", "session_message", "type"),
        ("part-key", "session_part", "message_id"),
        ("part-type", "session_part", "type"),
    ] {
        let case = Case::new(name);
        case.assistants();
        case.part(0, json!({}));
        // Multibyte text proves the limit is bytes rather than SQLite characters.
        let oversized = "界".repeat(8 * 1024 * 1024 / 3 + 1);
        let predicate = if table == "session_message" {
            " WHERE id='msg_stop'"
        } else {
            ""
        };
        case.connection
            .execute(
                &format!("UPDATE {table} SET {field}=?1{predicate}"),
                [&oversized],
            )
            .unwrap();
        case.rejected(NativeIssue::ScanLimit);
    }
}

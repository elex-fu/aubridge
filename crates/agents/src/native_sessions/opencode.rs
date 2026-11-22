//! OpenCode 2.2.5 native SQLite, opened read-only with ordinary WAL visibility.
use super::*;
use rusqlite::{params, Connection, OpenFlags};

fn open(path: &Path) -> Result<Connection, NativeIssue> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| NativeIssue::RecordUnreadable)?;
    connection
        .busy_timeout(std::time::Duration::from_millis(100))
        .map_err(|_| NativeIssue::RecordUnreadable)?;
    Ok(connection)
}
fn table(connection: &Connection) -> Result<&'static str, NativeIssue> {
    for name in ["session_v2", "session"] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [name],
                |r| r.get(0),
            )
            .map_err(|_| NativeIssue::UnsupportedSchema)?;
        if !exists {
            continue;
        }
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({name})"))
            .map_err(|_| NativeIssue::UnsupportedSchema)?;
        let fields = statement
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|_| NativeIssue::UnsupportedSchema)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| NativeIssue::UnsupportedSchema)?;
        if [
            "id",
            "directory",
            "parent_id",
            "fork_session_id",
            "model",
            "time_updated",
        ]
        .iter()
        .all(|f| fields.iter().any(|v| v == f))
        {
            return Ok(name);
        }
    }
    Err(NativeIssue::UnsupportedSchema)
}
fn valid_id(id: &str) -> bool {
    id.starts_with("ses_")
        && id.len() <= 128
        && id.len() > 4
        && id.bytes().all(|v| v.is_ascii_alphanumeric() || v == b'_')
}

fn columns(connection: &Connection, name: &str) -> Result<Vec<(String, i64)>, NativeIssue> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({name})"))
        .map_err(|_| NativeIssue::UnsupportedSchema)?;
    let result = statement
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
        .map_err(|_| NativeIssue::UnsupportedSchema)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| NativeIssue::UnsupportedSchema);
    result
}
fn fields(connection: &Connection, name: &str) -> Result<Vec<String>, NativeIssue> {
    Ok(columns(connection, name)?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}
#[derive(Clone, Copy)]
enum MessageSchema {
    V2,
    Legacy,
}
fn message_schema(connection: &Connection) -> Result<MessageSchema, NativeIssue> {
    let messages = columns(connection, "session_message")?;
    let parts = columns(connection, "session_part")?;
    let has = |columns: &[(String, i64)], required: &[&str]| {
        required
            .iter()
            .all(|key| columns.iter().any(|(name, _)| name == key))
    };
    let primary_key = |columns: &[(String, i64)], expected: &[(&str, i64)]| {
        columns.iter().filter(|(_, pk)| *pk > 0).count() == expected.len()
            && expected.iter().all(|(key, ordinal)| {
                columns
                    .iter()
                    .any(|(name, pk)| name == key && pk == ordinal)
            })
    };
    if has(&messages, &["id", "session_id", "type", "seq", "data"])
        && has(
            &parts,
            &["message_id", "session_id", "ordinal", "type", "data"],
        )
        && primary_key(&messages, &[("id", 1)])
        && primary_key(&parts, &[("message_id", 1), ("ordinal", 2)])
    {
        Ok(MessageSchema::V2)
    } else if has(&messages, &["id", "session_id", "data"])
        && has(&parts, &["id", "session_id", "data"])
    {
        Ok(MessageSchema::Legacy)
    } else {
        Err(NativeIssue::UnsupportedSchema)
    }
}
fn managed_model(model: &Value) -> bool {
    model["providerID"] == "aubridge" && model["id"].as_str().is_some_and(|v| !v.is_empty())
}
fn messages(
    connection: &Connection,
    id: &str,
    schema: MessageSchema,
    mut visit: impl FnMut(bool, &Value) -> Result<(), NativeIssue>,
) -> Result<u64, NativeIssue> {
    // Separate cursors include orphan parts too; neither an inner join nor a UNION
    // may hide rows or assume that V2 parts have an id column.
    let selections = match schema {
        MessageSchema::V2 => [
            ("session_message", "id", "seq", "type", "seq,id"),
            (
                "session_part",
                "message_id",
                "ordinal",
                "type",
                "message_id,ordinal",
            ),
        ],
        MessageSchema::Legacy => [
            ("session_message", "id", "0", "''", "id"),
            ("session_part", "id", "0", "''", "id"),
        ],
    };
    let mut size = 0u64;
    let mut count = 0;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for (kind, (table, key, order, row_type, sort)) in selections.into_iter().enumerate() {
        // These expressions are fixed schema literals. Bound every returned text/blob
        // in SQLite before the driver can allocate it; the original byte count still
        // participates in the shared message/part budget.
        let length = format!("length(CAST({key} AS BLOB))+length(CAST({row_type} AS BLOB))+length(CAST(data AS BLOB))");
        let query = format!("SELECT CASE WHEN {length}<=?3 THEN {key} END,'',{order},CASE WHEN {length}<=?3 THEN {row_type} END,CASE WHEN {length}<=?3 THEN substr(CAST(data AS BLOB),1,?3) END,{length} FROM {table} WHERE session_id=?1 ORDER BY {sort} LIMIT ?2");
        let mut statement = connection
            .prepare(&query)
            .map_err(|_| NativeIssue::UnsupportedSchema)?;
        let mut rows = statement
            .query(params![id, 4097, BYTE_LIMIT])
            .map_err(|_| NativeIssue::RecordUnreadable)?;
        while let Some(row) = rows.next().map_err(|_| NativeIssue::RecordUnreadable)? {
            count += 1;
            let total: u64 = row.get(5).map_err(|_| NativeIssue::UnsupportedSchema)?;
            size = size.saturating_add(total);
            if count > 4096 || size > BYTE_LIMIT as u64 {
                return Err(NativeIssue::ScanLimit);
            }
            let key: String = row.get(0).map_err(|_| NativeIssue::UnsupportedSchema)?;
            let parent: String = row.get(1).map_err(|_| NativeIssue::UnsupportedSchema)?;
            let order: i64 = row.get(2).map_err(|_| NativeIssue::UnsupportedSchema)?;
            let row_type: String = row.get(3).map_err(|_| NativeIssue::UnsupportedSchema)?;
            let data: Vec<u8> = row.get(4).map_err(|_| NativeIssue::UnsupportedSchema)?;
            (kind, key, parent, order, &row_type, &data).hash(&mut hash);
            let value: Value =
                serde_json::from_slice(&data).map_err(|_| NativeIssue::UnsupportedSchema)?;
            visit(kind == 0 && row_type == "assistant", &value)?;
        }
    }
    Ok(hash.finish())
}
pub(super) fn read(root: &Path, file: &Path, id: &str) -> Result<NativeRecord, NativeIssue> {
    if !valid_id(id) {
        return Err(NativeIssue::InvalidSessionId);
    }
    let file = contained(root, file)?;
    let connection = open(&file)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| NativeIssue::RecordUnreadable)?;
    let table = table(&transaction)?;
    let (native_id,cwd,parent,fork,model,updated):(String,String,Option<String>,Option<String>,Option<String>,i64)=transaction.query_row(&format!("SELECT id,directory,parent_id,fork_session_id,model,time_updated FROM {table} WHERE id=?1"),[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|_|NativeIssue::RecordUnreadable)?;
    if parent.is_some() || fork.is_some() {
        return Err(NativeIssue::ChildSession);
    }
    let schema = message_schema(&transaction)?;
    let version: Option<String> = if fields(&transaction, table)?.iter().any(|v| v == "version") {
        transaction
            .query_row(
                &format!("SELECT version FROM {table} WHERE id=?1"),
                [id],
                |r| r.get(0),
            )
            .map_err(|_| NativeIssue::UnsupportedSchema)?
    } else {
        None
    };
    let agent: Option<String> = if fields(&transaction, table)?.iter().any(|v| v == "agent") {
        let (agent, length): (Option<String>, Option<u64>) = transaction.query_row(
            &format!("SELECT CASE WHEN length(CAST(agent AS BLOB))<=?2 THEN agent END,length(CAST(agent AS BLOB)) FROM {table} WHERE id=?1"),
            params![id, BYTE_LIMIT], |r| Ok((r.get(0)?, r.get(1)?)),
        ).map_err(|_| NativeIssue::UnsupportedSchema)?;
        if length.is_some_and(|length| length > BYTE_LIMIT as u64) {
            return Err(NativeIssue::ScanLimit);
        }
        agent
    } else {
        None
    };
    let fallback = model.is_none();
    if let Some(raw) = &model {
        let value: Value = serde_json::from_str(raw).map_err(|_| NativeIssue::UnsupportedSchema)?;
        if !managed_model(&value) {
            return Err(NativeIssue::ProfileMismatch);
        }
    } else if table != "session_v2" || !matches!(schema, MessageSchema::V2) {
        return Err(NativeIssue::ProfileMismatch);
    } else if version.as_deref() != Some("2.2.5") {
        return Err(NativeIssue::UnsupportedSchema);
    }
    let cwd = workspace(Some(&cwd))?;
    let canonical_cwd = fs::canonicalize(&cwd).map_err(|_| NativeIssue::WorkspaceMissing)?;
    if !canonical_cwd.is_dir() {
        return Err(NativeIssue::WorkspaceMissing);
    }
    let mut contains_opaque = false;
    let mut assistants = 0;
    let message_snapshot = messages(&transaction, id, schema, |assistant, value| {
        if fallback && assistant {
            if !managed_model(&value["model"]) {
                return Err(NativeIssue::ProfileMismatch);
            }
            assistants += 1;
        }
        contains_opaque |= opaque(value);
        Ok(())
    })?;
    if fallback && assistants == 0 {
        return Err(NativeIssue::ProfileMismatch);
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    native_id.hash(&mut hash);
    cwd.hash(&mut hash);
    model.hash(&mut hash);
    agent.hash(&mut hash);
    version.hash(&mut hash);
    parent.hash(&mut hash);
    fork.hash(&mut hash);
    updated.hash(&mut hash);
    message_snapshot.hash(&mut hash);
    let workspace_snapshot = workspace_stamp(&canonical_cwd)?;
    Ok(NativeRecord {
        metadata: NativeMetadata {
            agent: NativeAgent::OpenCode,
            native_id,
            cwd,
            contains_opaque,
        },
        canonical_cwd,
        updated_at: u64::try_from(updated).unwrap_or(0) / 1000,
        root: fs::canonicalize(root).map_err(|_| NativeIssue::UnsafePath)?,
        file,
        snapshot: hash.finish(),
        workspace_snapshot,
        storage: Storage::OpenCode,
    })
}
pub(super) fn scan(root: &Path, file: &Path) -> NativeScan {
    let mut scan = NativeScan {
        records: vec![],
        issues: vec![],
    };
    if !file.exists() {
        return scan;
    }
    let ids = (|| {
        let file = contained(root, file)?;
        let connection = open(&file)?;
        let table = table(&connection)?;
        let mut statement=connection.prepare(&format!("SELECT id FROM {table} WHERE parent_id IS NULL AND fork_session_id IS NULL ORDER BY time_updated DESC LIMIT ?1")).map_err(|_|NativeIssue::UnsupportedSchema)?;
        let result = statement
            .query_map([201], |r| r.get::<_, String>(0))
            .map_err(|_| NativeIssue::RecordUnreadable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| NativeIssue::RecordUnreadable)?;
        Ok::<_, NativeIssue>(result)
    })();
    match ids {
        Ok(ids) => {
            if ids.len() > 200 {
                scan.issues.push(NativeIssue::ScanLimit);
            }
            for id in ids.into_iter().take(200) {
                match read(root, file, &id) {
                    Ok(record) => scan.records.push(record),
                    Err(issue) => {
                        if !scan.issues.contains(&issue) {
                            scan.issues.push(issue);
                        }
                    }
                }
            }
        }
        Err(issue) => scan.issues.push(issue),
    }
    scan
}
pub(super) fn sealed(
    record: &NativeRecord,
    check: &mut impl FnMut(&Value) -> Result<(), NativeIssue>,
) -> Result<(), NativeIssue> {
    let connection = open(&record.file)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| NativeIssue::RecordUnreadable)?;
    let schema = message_schema(&transaction)?;
    messages(
        &transaction,
        &record.metadata.native_id,
        schema,
        |_, value| check_sealed_value(value, check),
    )?;
    Ok(())
}

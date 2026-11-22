//! Minimal native identity evidence. No transcript or executable command is serialized.
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
mod gemini;
pub mod gemini_headers;
mod opencode;

const BYTE_LIMIT: usize = 8 * 1024 * 1024;
const ENTRY_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeAgent {
    Codex,
    Claude,
    OpenCode,
    Gemini,
}
impl NativeAgent {
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude-code",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeIssue {
    InvalidSessionId,
    UnsupportedSchema,
    WorkspaceMissing,
    IdentityMismatch,
    ProfileMismatch,
    ChildSession,
    ScanLimit,
    RecordUnreadable,
    RecordChanged,
    UnsafePath,
    OpaqueUnverified,
}
impl NativeIssue {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidSessionId => "invalid_session_id",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::WorkspaceMissing => "workspace_missing",
            Self::IdentityMismatch => "identity_mismatch",
            Self::ProfileMismatch => "profile_mismatch",
            Self::ChildSession => "child_session",
            Self::ScanLimit => "scan_limit",
            Self::RecordUnreadable => "record_unreadable",
            Self::RecordChanged => "record_changed",
            Self::UnsafePath => "unsafe_path",
            Self::OpaqueUnverified => "opaque_unverified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMetadata {
    pub agent: NativeAgent,
    pub native_id: String,
    pub cwd: PathBuf,
    pub contains_opaque: bool,
}

fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
pub fn safe_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.contains(char::is_control)
}
fn workspace(value: Option<&str>) -> Result<PathBuf, NativeIssue> {
    let value = value
        .filter(|v| safe_value(v))
        .ok_or(NativeIssue::WorkspaceMissing)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(NativeIssue::WorkspaceMissing);
    }
    Ok(path)
}
fn opaque(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, v)| {
            key == "encrypted_content"
                || key == "thoughtSignature"
                || key == "signature"
                || key == "providerMetadata"
                || key == "provider_metadata"
                || opaque(v)
        }),
        Value::Array(items) => items.iter().any(opaque),
        _ => false,
    }
}

/// Parse bounded native records, accepting identity only from the native schema.
pub fn inspect_jsonl(
    agent: NativeAgent,
    file: &Path,
    bytes: &[u8],
) -> Result<NativeMetadata, NativeIssue> {
    if bytes.len() > BYTE_LIMIT {
        return Err(NativeIssue::ScanLimit);
    }
    let mut found: Option<NativeMetadata> = None;
    let mut has_opaque = false;
    for line in bytes
        .split(|c| *c == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let row: Value =
            serde_json::from_slice(line).map_err(|_| NativeIssue::UnsupportedSchema)?;
        has_opaque |= opaque(&row);
        let candidate = match agent {
            NativeAgent::Codex if row["type"] == "session_meta" => {
                let p = &row["payload"];
                let id = p["id"]
                    .as_str()
                    .filter(|id| uuid(id))
                    .ok_or(NativeIssue::InvalidSessionId)?;
                let cwd = workspace(p["cwd"].as_str())?;
                if !p["parent_thread_id"].is_null()
                    || !p["forked_from_id"].is_null()
                    || p["thread_source"] == "subagent"
                    || p["source"].get("subagent").is_some()
                {
                    return Err(NativeIssue::ChildSession);
                }
                if !matches!(p["source"].as_str(), Some("cli" | "exec"))
                    || !p["cli_version"]
                        .as_str()
                        .is_some_and(|v| crate::readiness::version_supported("codex", v))
                {
                    return Err(NativeIssue::UnsupportedSchema);
                }
                if p["model_provider"] != "aubridge" {
                    return Err(NativeIssue::ProfileMismatch);
                }
                let suffix = format!("{id}.jsonl");
                if !file
                    .file_name()
                    .and_then(|v| v.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
                {
                    return Err(NativeIssue::IdentityMismatch);
                }
                Some(NativeMetadata {
                    agent,
                    native_id: id.into(),
                    cwd,
                    contains_opaque: false,
                })
            }
            NativeAgent::Claude
                if row.get("sessionId").is_some()
                    && (row.get("cwd").is_some()
                        || row.get("type").is_none()
                        || matches!(row["type"].as_str(), Some("user" | "assistant"))) =>
            {
                let id = row["sessionId"]
                    .as_str()
                    .filter(|id| uuid(id))
                    .ok_or(NativeIssue::InvalidSessionId)?;
                let cwd = workspace(row["cwd"].as_str())?;
                match row["isSidechain"].as_bool() {
                    Some(false) => {}
                    Some(true) => return Err(NativeIssue::ChildSession),
                    None => return Err(NativeIssue::UnsupportedSchema),
                }
                if file.file_stem().and_then(|v| v.to_str()) != Some(id) {
                    return Err(NativeIssue::IdentityMismatch);
                }
                Some(NativeMetadata {
                    agent,
                    native_id: id.into(),
                    cwd,
                    contains_opaque: false,
                })
            }
            NativeAgent::Claude | NativeAgent::Codex => None,
            _ => return Err(NativeIssue::UnsupportedSchema),
        };
        if let Some(candidate) = candidate {
            if found.as_ref().is_some_and(|old| old != &candidate) {
                return Err(NativeIssue::IdentityMismatch);
            }
            found = Some(candidate);
        }
    }
    let mut found = found.ok_or(NativeIssue::UnsupportedSchema)?;
    found.contains_opaque = has_opaque;
    Ok(found)
}

#[derive(Debug, Clone)]
pub struct NativeRecord {
    pub metadata: NativeMetadata,
    pub canonical_cwd: PathBuf,
    pub updated_at: u64,
    root: PathBuf,
    file: PathBuf,
    snapshot: u64,
    workspace_snapshot: u64,
    storage: Storage,
}
#[derive(Debug, Clone)]
enum Storage {
    Jsonl,
    OpenCode,
    Gemini,
}
fn stamp(path: &Path) -> Result<u64, NativeIssue> {
    let m = fs::metadata(path).map_err(|_| NativeIssue::RecordUnreadable)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    m.len().hash(&mut hash);
    m.modified().ok().hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.ino().hash(&mut hash);
        m.dev().hash(&mut hash);
    }
    Ok(hash.finish())
}
fn contained(root: &Path, file: &Path) -> Result<PathBuf, NativeIssue> {
    if fs::symlink_metadata(root)
        .map_err(|_| NativeIssue::RecordUnreadable)?
        .file_type()
        .is_symlink()
    {
        return Err(NativeIssue::UnsafePath);
    }
    let relative = file
        .strip_prefix(root)
        .map_err(|_| NativeIssue::UnsafePath)?;
    let root = fs::canonicalize(root).map_err(|_| NativeIssue::RecordUnreadable)?;
    let canonical = fs::canonicalize(file).map_err(|_| NativeIssue::RecordUnreadable)?;
    if !canonical.starts_with(&root) {
        return Err(NativeIssue::UnsafePath);
    }
    let mut check = root.clone();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(NativeIssue::UnsafePath);
        }
        check.push(component);
        if fs::symlink_metadata(&check)
            .map_err(|_| NativeIssue::RecordUnreadable)?
            .file_type()
            .is_symlink()
        {
            return Err(NativeIssue::UnsafePath);
        }
    }
    Ok(canonical)
}
pub fn read_jsonl(
    agent: NativeAgent,
    root: &Path,
    file: &Path,
) -> Result<NativeRecord, NativeIssue> {
    if fs::symlink_metadata(root)
        .map_err(|_| NativeIssue::RecordUnreadable)?
        .file_type()
        .is_symlink()
    {
        return Err(NativeIssue::UnsafePath);
    }
    let file = contained(root, file)?;
    let root = fs::canonicalize(root).map_err(|_| NativeIssue::RecordUnreadable)?;
    let before = stamp(&file)?;
    let mut bytes = Vec::new();
    fs::File::open(&file)
        .map_err(|_| NativeIssue::RecordUnreadable)?
        .take((BYTE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| NativeIssue::RecordUnreadable)?;
    let metadata = inspect_jsonl(agent, &file, &bytes)?;
    if stamp(&file)? != before {
        return Err(NativeIssue::RecordChanged);
    }
    let canonical_cwd =
        fs::canonicalize(&metadata.cwd).map_err(|_| NativeIssue::WorkspaceMissing)?;
    if !canonical_cwd.is_dir() {
        return Err(NativeIssue::WorkspaceMissing);
    }
    let workspace_snapshot = workspace_stamp(&canonical_cwd)?;
    let updated_at = fs::metadata(&file)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_secs());
    Ok(NativeRecord {
        metadata,
        canonical_cwd,
        updated_at,
        root,
        file,
        snapshot: before,
        workspace_snapshot,
        storage: Storage::Jsonl,
    })
}
fn workspace_stamp(path: &Path) -> Result<u64, NativeIssue> {
    let m = fs::metadata(path).map_err(|_| NativeIssue::WorkspaceMissing)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.ino().hash(&mut h);
        m.dev().hash(&mut h);
    }
    Ok(h.finish())
}
impl NativeRecord {
    pub fn database_path(&self) -> Option<&Path> {
        if matches!(self.storage, Storage::OpenCode) {
            Some(&self.file)
        } else {
            None
        }
    }
    pub fn recheck(&self) -> Result<(), NativeIssue> {
        let next = match self.storage {
            Storage::Jsonl => read_jsonl(self.metadata.agent, &self.root, &self.file)?,
            Storage::OpenCode => opencode::read(&self.root, &self.file, &self.metadata.native_id)?,
            Storage::Gemini => gemini::read(&self.root, &self.file)?,
        };
        if next.metadata != self.metadata
            || next.canonical_cwd != self.canonical_cwd
            || next.snapshot != self.snapshot
            || next.workspace_snapshot != self.workspace_snapshot
        {
            return Err(NativeIssue::RecordChanged);
        }
        Ok(())
    }
    pub fn project_alias(&self) -> String {
        self.canonical_cwd
            .file_name()
            .and_then(|v| v.to_str())
            .filter(|v| safe_value(v))
            .unwrap_or("项目")
            .chars()
            .take(80)
            .collect()
    }
    /// Check sealed items transiently; callers must not retain their contents.
    pub fn check_sealed(
        &self,
        mut check: impl FnMut(&Value) -> Result<(), NativeIssue>,
    ) -> Result<(), NativeIssue> {
        self.recheck()?;
        if matches!(self.storage, Storage::OpenCode) {
            opencode::sealed(self, &mut check)?;
            return self.recheck();
        }
        let mut bytes = Vec::new();
        fs::File::open(&self.file)
            .map_err(|_| NativeIssue::RecordUnreadable)?
            .take((BYTE_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| NativeIssue::RecordUnreadable)?;
        if bytes.len() > BYTE_LIMIT {
            return Err(NativeIssue::ScanLimit);
        }
        for line in bytes
            .split(|c| *c == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        {
            let row: Value =
                serde_json::from_slice(line).map_err(|_| NativeIssue::UnsupportedSchema)?;
            check_sealed_value(&row, &mut check)?;
        }
        self.recheck()
    }
}
pub fn scan_opencode(root: &Path, database: &Path) -> NativeScan {
    opencode::scan(root, database)
}
pub fn scan_gemini(home: &Path) -> NativeScan {
    gemini::scan(home)
}
fn check_sealed_value(
    value: &Value,
    check: &mut impl FnMut(&Value) -> Result<(), NativeIssue>,
) -> Result<(), NativeIssue> {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("encrypted_content") {
                if !matches!(
                    value["type"].as_str(),
                    Some("compaction" | "compaction_summary" | "context_compaction")
                ) {
                    return Err(NativeIssue::OpaqueUnverified);
                }
                return check(value);
            }
            if fields.contains_key("signature")
                || fields.contains_key("thoughtSignature")
                || fields.contains_key("providerMetadata")
                || fields.contains_key("provider_metadata")
            {
                return Err(NativeIssue::OpaqueUnverified);
            }
            for child in fields.values() {
                check_sealed_value(child, check)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                check_sealed_value(child, check)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub struct NativeScan {
    pub records: Vec<NativeRecord>,
    pub issues: Vec<NativeIssue>,
}
pub fn scan_jsonl(agent: NativeAgent, root: &Path) -> NativeScan {
    let mut result = NativeScan {
        records: Vec::new(),
        issues: Vec::new(),
    };
    if !root.exists() {
        return result;
    }
    if fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        result.issues.push(NativeIssue::UnsafePath);
        return result;
    }
    let Ok(root) = fs::canonicalize(root) else {
        result.issues.push(NativeIssue::RecordUnreadable);
        return result;
    };
    let mut pending = vec![(root.clone(), 0)];
    let mut seen = 0;
    let mut bytes_seen = 0u64;
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            result.issues.push(NativeIssue::RecordUnreadable);
            continue;
        };
        for entry in entries {
            seen += 1;
            if seen > ENTRY_LIMIT || result.records.len() >= 200 {
                result.issues.push(NativeIssue::ScanLimit);
                return result;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                result.issues.push(NativeIssue::UnsafePath);
                continue;
            }
            if kind.is_dir() && depth < 8 {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                let size = entry.metadata().map_or(u64::MAX, |m| m.len());
                if size > BYTE_LIMIT as u64 || bytes_seen.saturating_add(size) > 32 * 1024 * 1024 {
                    if !result.issues.contains(&NativeIssue::ScanLimit) {
                        result.issues.push(NativeIssue::ScanLimit);
                    }
                    continue;
                }
                bytes_seen += size;
                match read_jsonl(agent, &root, &entry.path()) {
                    Ok(record) => result.records.push(record),
                    Err(issue) => {
                        if !result.issues.contains(&issue) {
                            result.issues.push(issue);
                        }
                    }
                }
            }
        }
    }
    result
        .records
        .sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    result
}

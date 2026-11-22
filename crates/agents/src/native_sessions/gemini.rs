//! Gemini 0.68 registry path→slug, canonical-root hash, explicitly main records.
use super::*;
use sha2::{Digest, Sha256};
type RegistryProject = (PathBuf, String, String);
type RegistrySnapshot = (Vec<RegistryProject>, u64);
fn bounded(file: &Path) -> Result<Vec<u8>, NativeIssue> {
    let mut bytes = vec![];
    fs::File::open(file)
        .map_err(|_| NativeIssue::RecordUnreadable)?
        .take((BYTE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| NativeIssue::RecordUnreadable)?;
    if bytes.len() > BYTE_LIMIT {
        return Err(NativeIssue::ScanLimit);
    }
    Ok(bytes)
}
fn registry(home: &Path) -> Result<RegistrySnapshot, NativeIssue> {
    let file = contained(home, &home.join("projects.json"))?;
    let bytes = bounded(&file)?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| NativeIssue::UnsupportedSchema)?;
    let projects = value["projects"]
        .as_object()
        .ok_or(NativeIssue::UnsupportedSchema)?;
    if projects.len() > 1024 {
        return Err(NativeIssue::ScanLimit);
    }
    let mut result = vec![];
    for (path, slug) in projects {
        let slug = slug
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 256
                    && s.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            })
            .ok_or(NativeIssue::UnsafePath)?;
        let path = workspace(Some(path))?;
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if path != canonical || !canonical.is_dir() {
            continue;
        }
        let raw = canonical.to_str().ok_or(NativeIssue::UnsafePath)?;
        let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
        result.push((canonical, slug.to_owned(), hash));
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hash);
    Ok((result, hash.finish()))
}
fn metadata(file: &Path, bytes: &[u8]) -> Result<Value, NativeIssue> {
    let mut metadata = serde_json::Map::new();
    let rows = if file.extension().is_some_and(|e| e == "json") {
        vec![serde_json::from_slice::<Value>(bytes).map_err(|_| NativeIssue::UnsupportedSchema)?]
    } else {
        bytes
            .split(|c| *c == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            .map(|line| {
                serde_json::from_slice::<Value>(line).map_err(|_| NativeIssue::UnsupportedSchema)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    for row in rows {
        let update = if row["$set"].is_object() {
            &row["$set"]
        } else {
            &row
        };
        if let Some(update) = update.as_object() {
            for key in ["sessionId", "projectHash", "kind"] {
                if let Some(value) = update.get(key) {
                    if metadata.get(key).is_some_and(|old| old != value) {
                        return Err(NativeIssue::IdentityMismatch);
                    }
                    metadata.insert(key.into(), value.clone());
                }
            }
        }
    }
    Ok(Value::Object(metadata))
}
pub(super) fn read(home: &Path, file: &Path) -> Result<NativeRecord, NativeIssue> {
    let file = contained(home, file)?;
    let home = fs::canonicalize(home).map_err(|_| NativeIssue::UnsafePath)?;
    let (projects, registry_snapshot) = registry(&home)?;
    let before = stamp(&file)?;
    let bytes = bounded(&file)?;
    let meta = metadata(&file, &bytes)?;
    let id = meta["sessionId"]
        .as_str()
        .filter(|s| uuid(s))
        .ok_or(NativeIssue::InvalidSessionId)?;
    match meta["kind"].as_str() {
        Some("main") => {}
        Some("subagent") => return Err(NativeIssue::ChildSession),
        _ => return Err(NativeIssue::UnsupportedSchema),
    }
    let project_hash = meta["projectHash"]
        .as_str()
        .ok_or(NativeIssue::UnsupportedSchema)?;
    let matches = projects
        .into_iter()
        .filter(|(_, slug, hash)| {
            hash == project_hash
                && file.parent() == Some(home.join("tmp").join(slug).join("chats").as_path())
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(NativeIssue::IdentityMismatch);
    }
    let (canonical_cwd, _, _) = matches
        .into_iter()
        .next()
        .ok_or(NativeIssue::IdentityMismatch)?;
    let filename = file
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or(NativeIssue::UnsafePath)?;
    if !filename.starts_with("session-") || !filename.contains(&id[..8]) {
        return Err(NativeIssue::IdentityMismatch);
    }
    let contains_opaque = if file.extension().is_some_and(|e| e == "json") {
        opaque(
            &serde_json::from_slice::<Value>(&bytes).map_err(|_| NativeIssue::UnsupportedSchema)?,
        )
    } else {
        let mut found = false;
        for line in bytes
            .split(|c| *c == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        {
            found |= opaque(
                &serde_json::from_slice::<Value>(line)
                    .map_err(|_| NativeIssue::UnsupportedSchema)?,
            );
        }
        found
    };
    if stamp(&file)? != before {
        return Err(NativeIssue::RecordChanged);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    registry_snapshot.hash(&mut h);
    before.hash(&mut h);
    let updated_at = fs::metadata(&file)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_secs());
    let workspace_snapshot = workspace_stamp(&canonical_cwd)?;
    Ok(NativeRecord {
        metadata: NativeMetadata {
            agent: NativeAgent::Gemini,
            native_id: id.into(),
            cwd: canonical_cwd.clone(),
            contains_opaque,
        },
        canonical_cwd,
        updated_at,
        root: home,
        file,
        snapshot: h.finish(),
        workspace_snapshot,
        storage: Storage::Gemini,
    })
}
pub(super) fn scan(home: &Path) -> NativeScan {
    let mut scan = NativeScan {
        records: vec![],
        issues: vec![],
    };
    if !home.join("projects.json").exists() {
        return scan;
    }
    let projects = match registry(home) {
        Ok((p, _)) => p,
        Err(e) => {
            scan.issues.push(e);
            return scan;
        }
    };
    let mut seen = 0;
    let mut size = 0u64;
    for (_, slug, _) in projects {
        let dir = home.join("tmp").join(slug).join("chats");
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 4096 || scan.records.len() >= 200 {
                scan.issues.push(NativeIssue::ScanLimit);
                return scan;
            }
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_file())
                || !path
                    .extension()
                    .is_some_and(|e| e == "json" || e == "jsonl")
            {
                continue;
            }
            let bytes = entry.metadata().map_or(u64::MAX, |m| m.len());
            size = size.saturating_add(bytes);
            if size > 32 * 1024 * 1024 {
                scan.issues.push(NativeIssue::ScanLimit);
                return scan;
            }
            match read(home, &path) {
                Ok(record) => scan.records.push(record),
                Err(e) => {
                    if !scan.issues.contains(&e) {
                        scan.issues.push(e);
                    }
                }
            }
        }
    }
    scan.records
        .sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    scan
}

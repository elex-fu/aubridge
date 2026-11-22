//! Narrow, confirmed project dotenv additions and digest-owned undo (decision 53).
use crate::AgentsPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
const KEYS: [&str; 2] = ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"];
const LIMIT: u64 = 1_048_576;
fn conflict() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "project environment changed or conflicts",
    )
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "ambiguous project environment")
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    dev: u64,
    ino: u64,
}
#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}
#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Identity {
    Identity { dev: 0, ino: 0 }
}
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    digest: String,
    mode: u32,
}
struct Document {
    bytes: Vec<u8>,
    stamp: Stamp,
    lines: Vec<Line>,
    values: [Option<String>; 2],
    newline: &'static str,
}
struct Line {
    start: usize,
    end: usize,
    key: Option<usize>,
}
#[derive(Clone, Serialize)]
pub struct Change {
    pub key: String,
    pub action: String,
}
#[derive(Clone)]
pub struct Plan {
    project: PathBuf,
    project_identity: Identity,
    target: PathBuf,
    parent_identity: Identity,
    stamp: Stamp,
    receipt_digest: Option<String>,
    managed_digest: Option<String>,
    operation: String,
    pub changes: Vec<Change>,
    pub layer: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct OwnedLine {
    key: String,
    digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Separator {
    newline: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    schema: u8,
    project: PathBuf,
    project_identity: Identity,
    target_relative: String,
    target_identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_identity: Option<Identity>,
    written_digest: String,
    added: Vec<OwnedLine>,
    separator: Option<Separator>,
    phase: String,
    before_digest: String,
    after_digest: String,
}
fn directory(path: &Path) -> io::Result<Identity> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(conflict());
    }
    Ok(file_identity(&meta))
}
fn target_path(project: &Path, target: &Path) -> io::Result<(&'static str, Identity)> {
    if std::fs::canonicalize(project)? != project {
        return Err(conflict());
    }
    let layer = if target == project.join(".env") {
        "project_plain"
    } else if target == project.join(".gemini/.env") {
        "project_gemini"
    } else {
        return Err(conflict());
    };
    let parent = target.parent().ok_or_else(conflict)?;
    let identity = directory(parent)?;
    if std::fs::canonicalize(parent)? != parent {
        return Err(conflict());
    }
    Ok((layer, identity))
}
fn read_file(path: &Path) -> io::Result<(Vec<u8>, Stamp)> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(conflict());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(conflict());
        }
    }
    #[cfg(not(unix))]
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe project writes require Unix file identity",
        ));
    }
    if meta.len() > LIMIT {
        return Err(invalid());
    }
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if file_identity(&opened) != file_identity(&meta) {
        return Err(conflict());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid());
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        opened.permissions().mode()
    };
    #[cfg(not(unix))]
    let mode = 0;
    let stamp = Stamp {
        identity: file_identity(&opened),
        digest: digest(&bytes),
        mode,
    };
    Ok((bytes, stamp))
}
pub(crate) fn value(text: &str) -> io::Result<String> {
    let text = text.trim();
    if let Some(quote) = text
        .chars()
        .next()
        .filter(|c| matches!(c, '\'' | '"' | '`'))
    {
        let rest = &text[quote.len_utf8()..];
        let end = rest.find(quote).ok_or_else(invalid)?;
        let result = &rest[..end];
        let tail = rest[end + quote.len_utf8()..].trim_start();
        if (!tail.is_empty() && !tail.starts_with('#')) || result.contains('\\') {
            return Err(invalid());
        }
        Ok(result.to_owned())
    } else {
        if text.contains(['\'', '"', '`']) {
            return Err(invalid());
        }
        Ok(text.split('#').next().unwrap_or("").trim_end().to_owned())
    }
}
fn document(path: &Path) -> io::Result<Document> {
    let (bytes, stamp) = read_file(path)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
    let mut lines = Vec::new();
    let mut values: [Option<String>; 2] = [None, None];
    let mut offset = 0;
    let mut newline = None;
    for raw in text.split_inclusive('\n') {
        if newline.is_none() && raw.ends_with('\n') {
            newline = Some(if raw.ends_with("\r\n") { "\r\n" } else { "\n" });
        }
        let body = raw
            .strip_suffix("\r\n")
            .or_else(|| raw.strip_suffix('\n'))
            .unwrap_or(raw);
        if body.contains('\r') || body.contains('\0') {
            return Err(invalid());
        }
        let body = if offset == 0 {
            body.strip_prefix('\u{feff}').unwrap_or(body)
        } else {
            body
        };
        let body = body.trim_start();
        let mut key = None;
        if !body.is_empty() && !body.starts_with('#') {
            let body = body
                .strip_prefix("export ")
                .or_else(|| body.strip_prefix("export\t"))
                .unwrap_or(body)
                .trim_start();
            let (name, rhs) = body.split_once('=').ok_or_else(invalid)?;
            let name = name.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            {
                return Err(invalid());
            }
            let decoded = value(rhs)?;
            if let Some(index) = KEYS.iter().position(|candidate| *candidate == name) {
                if values[index].is_some() {
                    return Err(conflict());
                }
                values[index] = Some(decoded);
                key = Some(index);
            }
        }
        lines.push(Line {
            start: offset,
            end: offset + raw.len(),
            key,
        });
        offset += raw.len();
    }
    Ok(Document {
        bytes,
        stamp,
        lines,
        values,
        newline: newline.unwrap_or("\n"),
    })
}
fn managed(paths: &AgentsPaths) -> io::Result<[String; 2]> {
    let env = document(&paths.gemini_env)?;
    let base = env.values[0].clone().ok_or_else(conflict)?;
    let token = env.values[1].clone().ok_or_else(conflict)?;
    let port = base
        .strip_prefix("http://127.0.0.1:")
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .ok_or_else(conflict)?;
    if token != "aubridge-gemini"
        || !crate::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            port,
        )
        .wired()
    {
        return Err(conflict());
    }
    Ok([base, token])
}
pub fn matches_managed(paths: &AgentsPaths, target: &Path) -> io::Result<bool> {
    let expected = managed(paths)?;
    let doc = document(target)?;
    Ok(doc
        .values
        .iter()
        .zip(expected.iter())
        .all(|(value, expected)| value.as_deref() == Some(expected.as_str())))
}
fn receipt_path(paths: &AgentsPaths, project: &Path) -> PathBuf {
    paths.state_dir.join("project-env").join(format!(
        "{}.json",
        digest(project.as_os_str().as_encoded_bytes())
    ))
}
fn receipt_bytes(paths: &AgentsPaths, project: &Path) -> io::Result<Option<Vec<u8>>> {
    match read_file(&receipt_path(paths, project)) {
        Ok((bytes, _)) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn receipt(paths: &AgentsPaths, project: &Path) -> io::Result<(Option<Receipt>, Option<String>)> {
    let Some(bytes) = receipt_bytes(paths, project)? else {
        return Ok((None, None));
    };
    let raw_digest = Some(digest(&bytes));
    let mut proof: Receipt = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let valid_digest =
        |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if proof.added.is_empty()
        || proof.added.len() > 2
        || !valid_digest(&proof.before_digest)
        || !valid_digest(&proof.after_digest)
        || !valid_digest(&proof.written_digest)
        || proof
            .added
            .iter()
            .any(|line| !KEYS.contains(&line.key.as_str()) || !valid_digest(&line.digest))
        || proof.added.iter().enumerate().any(|(index, line)| {
            proof.added[..index]
                .iter()
                .any(|other| other.key == line.key)
        })
        || proof
            .separator
            .as_ref()
            .is_some_and(|separator| !matches!(separator.newline.as_str(), "\n" | "\r\n"))
    {
        return Err(conflict());
    }
    if proof.schema != 1
        || proof.project != project
        || proof.project_identity != directory(project)?
        || !matches!(proof.target_relative.as_str(), ".env" | ".gemini/.env")
    {
        return Err(conflict());
    }
    let target = project.join(&proof.target_relative);
    target_path(project, &target)?;
    let (_, stamp) = read_file(&target)?;
    match proof.phase.as_str() {
        "applied" => {}
        "add_intent"
            if stamp.digest == proof.before_digest && stamp.identity == proof.target_identity =>
        {
            return Ok((None, raw_digest))
        }
        "add_intent"
            if stamp.digest == proof.after_digest
                && proof.prepared_identity.as_ref() == Some(&stamp.identity) =>
        {
            proof.phase = "applied".into();
            proof.target_identity = stamp.identity;
            proof.prepared_identity = None;
        }
        "undo_intent"
            if stamp.digest == proof.before_digest && stamp.identity == proof.target_identity =>
        {
            proof.phase = "applied".into();
            proof.prepared_identity = None;
        }
        "undo_intent"
            if stamp.digest == proof.after_digest
                && proof.prepared_identity.as_ref() == Some(&stamp.identity) =>
        {
            return Ok((None, raw_digest))
        }
        _ => return Err(conflict()),
    }
    Ok((Some(proof), raw_digest))
}
fn verify_owned(proof: &Receipt, doc: &Document) -> io::Result<Vec<usize>> {
    if doc.stamp.identity != proof.target_identity {
        return Err(conflict());
    }
    let mut indexes = Vec::new();
    for owned in &proof.added {
        let key = KEYS
            .iter()
            .position(|key| *key == owned.key)
            .ok_or_else(invalid)?;
        let (index, line) = doc
            .lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.key == Some(key))
            .ok_or_else(conflict)?;
        if digest(&doc.bytes[line.start..line.end]) != owned.digest {
            return Err(conflict());
        }
        indexes.push(index);
    }
    if indexes.is_empty() {
        return Err(conflict());
    }
    Ok(indexes)
}
pub fn preview_add(paths: &AgentsPaths, project: &Path, target: &Path) -> io::Result<Plan> {
    let (layer, parent_identity) = target_path(project, target)?;
    let doc = document(target)?;
    let expected = managed(paths)?;
    let (proof, receipt_digest) = receipt(paths, project)?;
    if let Some(proof) = proof.as_ref() {
        if project.join(&proof.target_relative) != target {
            return Err(conflict());
        }
        verify_owned(proof, &doc)?;
    }
    let mut changes = Vec::new();
    for (index, key) in KEYS.iter().enumerate() {
        let action = match &doc.values[index] {
            None => "add",
            Some(value) if value == &expected[index] => "keep",
            _ => return Err(conflict()),
        };
        changes.push(Change {
            key: (*key).into(),
            action: action.into(),
        });
    }
    if proof.is_some() && changes.iter().any(|change| change.action == "add") {
        return Err(conflict());
    }
    Ok(Plan {
        project: project.into(),
        project_identity: directory(project)?,
        target: target.into(),
        parent_identity,
        stamp: doc.stamp,
        receipt_digest,
        managed_digest: Some(digest(
            &serde_json::to_vec(&expected).map_err(io::Error::other)?,
        )),
        operation: "add".into(),
        changes,
        layer: layer.into(),
    })
}
pub fn preview_undo(paths: &AgentsPaths, project: &Path) -> io::Result<Plan> {
    let (proof, receipt_digest) = receipt(paths, project)?;
    let proof = proof.ok_or_else(conflict)?;
    let target = project.join(&proof.target_relative);
    let (layer, parent_identity) = target_path(project, &target)?;
    let doc = document(&target)?;
    verify_owned(&proof, &doc)?;
    let changes = proof
        .added
        .iter()
        .map(|owned| Change {
            key: owned.key.clone(),
            action: "remove_owned".into(),
        })
        .collect();
    Ok(Plan {
        project: project.into(),
        project_identity: directory(project)?,
        target,
        parent_identity,
        stamp: doc.stamp,
        receipt_digest,
        managed_digest: None,
        operation: "undo".into(),
        changes,
        layer: layer.into(),
    })
}
fn save_receipt(paths: &AgentsPaths, proof: &Receipt) -> io::Result<()> {
    let path = receipt_path(paths, &proof.project);
    let parent = path.parent().ok_or_else(invalid)?;
    std::fs::create_dir_all(parent)?;
    directory(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    crate::atomic_write(&path, &serde_json::to_vec(proof).map_err(io::Error::other)?)?;
    std::fs::File::open(parent)?.sync_all()
}
fn remove_receipt(paths: &AgentsPaths, project: &Path) -> io::Result<()> {
    match std::fs::remove_file(receipt_path(paths, project)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
struct Prepared {
    path: PathBuf,
    stamp: Stamp,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
fn prepare_write(plan: &Plan, bytes: &[u8]) -> io::Result<Prepared> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = plan.target.parent().ok_or_else(invalid)?;
    let temp = parent.join(format!(
        ".cli-env-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(plan.stamp.mode & 0o7777))?;
        }
        file.sync_all()?;
        drop(file);
        let (_, stamp) = read_file(&temp)?;
        if stamp.digest != digest(bytes) {
            return Err(conflict());
        }
        Ok(Prepared {
            path: temp.clone(),
            stamp,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
fn publish(plan: &Plan, prepared: &Prepared) -> io::Result<Stamp> {
    let parent = plan.target.parent().ok_or_else(invalid)?;
    if directory(&plan.project)? != plan.project_identity
        || directory(parent)? != plan.parent_identity
        || read_file(&plan.target)?.1 != plan.stamp
        || read_file(&prepared.path)?.1 != prepared.stamp
    {
        return Err(conflict());
    }
    std::fs::rename(&prepared.path, &plan.target)?;
    std::fs::File::open(parent)?.sync_all()?;
    let (_, after) = read_file(&plan.target)?;
    if after != prepared.stamp {
        return Err(conflict());
    }
    Ok(after)
}
fn append(
    doc: &Document,
    changes: &[Change],
    expected: &[String; 2],
) -> io::Result<(Vec<u8>, Vec<OwnedLine>, Option<Separator>)> {
    let mut bytes = doc.bytes.clone();
    let separator = if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let proof = Separator {
            newline: doc.newline.into(),
        };
        bytes.extend_from_slice(doc.newline.as_bytes());
        Some(proof)
    } else {
        None
    };
    let mut added = Vec::new();
    for change in changes.iter().filter(|change| change.action == "add") {
        let index = KEYS
            .iter()
            .position(|key| *key == change.key)
            .ok_or_else(invalid)?;
        let line = format!("{}={}{}", KEYS[index], expected[index], doc.newline);
        added.push(OwnedLine {
            key: change.key.clone(),
            digest: digest(line.as_bytes()),
        });
        bytes.extend_from_slice(line.as_bytes());
    }
    if bytes.len() as u64 > LIMIT {
        return Err(invalid());
    }
    Ok((bytes, added, separator))
}
fn subtract(doc: &Document, proof: &Receipt) -> io::Result<Vec<u8>> {
    let owned = verify_owned(proof, doc)?;
    let mut bytes = Vec::new();
    for (index, line) in doc.lines.iter().enumerate() {
        if !owned.contains(&index) {
            bytes.extend_from_slice(&doc.bytes[line.start..line.end]);
        }
    }
    if doc.stamp.digest == proof.written_digest {
        if let Some(separator) = &proof.separator {
            if bytes.ends_with(separator.newline.as_bytes()) {
                bytes.truncate(bytes.len() - separator.newline.len());
            }
        }
    }
    Ok(bytes)
}
impl Plan {
    pub fn target(&self) -> &Path {
        &self.target
    }
}
pub fn apply(paths: &AgentsPaths, plan: &Plan) -> io::Result<()> {
    if directory(&plan.project)? != plan.project_identity
        || directory(plan.target.parent().ok_or_else(invalid)?)? != plan.parent_identity
    {
        return Err(conflict());
    }
    let doc = document(&plan.target)?;
    if doc.stamp != plan.stamp {
        return Err(conflict());
    }
    let (existing, receipt_digest) = receipt(paths, &plan.project)?;
    if receipt_digest != plan.receipt_digest {
        return Err(conflict());
    }
    if plan.operation == "add" {
        let expected = managed(paths)?;
        if Some(digest(
            &serde_json::to_vec(&expected).map_err(io::Error::other)?,
        )) != plan.managed_digest
        {
            return Err(conflict());
        }
        let fresh = preview_add(paths, &plan.project, &plan.target)?;
        if fresh
            .changes
            .iter()
            .map(|c| (&c.key, &c.action))
            .collect::<Vec<_>>()
            != plan
                .changes
                .iter()
                .map(|c| (&c.key, &c.action))
                .collect::<Vec<_>>()
        {
            return Err(conflict());
        }
        if !plan.changes.iter().any(|change| change.action == "add") {
            return Ok(());
        }
        let (bytes, added, separator) = append(&doc, &plan.changes, &expected)?;
        let prepared = prepare_write(plan, &bytes)?;
        let mut proof = Receipt {
            schema: 1,
            project: plan.project.clone(),
            project_identity: plan.project_identity.clone(),
            target_relative: plan
                .target
                .strip_prefix(&plan.project)
                .map_err(|_| conflict())?
                .to_string_lossy()
                .into_owned(),
            target_identity: plan.stamp.identity.clone(),
            prepared_identity: Some(prepared.stamp.identity.clone()),
            written_digest: digest(&bytes),
            added,
            separator,
            phase: "add_intent".into(),
            before_digest: doc.stamp.digest.clone(),
            after_digest: digest(&bytes),
        };
        save_receipt(paths, &proof)?;
        let after = publish(plan, &prepared)?;
        if after.digest != proof.after_digest
            || proof.prepared_identity.as_ref() != Some(&after.identity)
        {
            return Err(conflict());
        }
        proof.target_identity = after.identity;
        proof.prepared_identity = None;
        proof.phase = "applied".into();
        save_receipt(paths, &proof)
    } else if plan.operation == "undo" {
        let mut proof = existing.ok_or_else(conflict)?;
        let bytes = subtract(&doc, &proof)?;
        let prepared = prepare_write(plan, &bytes)?;
        proof.phase = "undo_intent".into();
        proof.prepared_identity = Some(prepared.stamp.identity.clone());
        proof.before_digest = doc.stamp.digest;
        proof.after_digest = digest(&bytes);
        save_receipt(paths, &proof)?;
        let after = publish(plan, &prepared)?;
        if after.digest != proof.after_digest
            || proof.prepared_identity.as_ref() != Some(&after.identity)
        {
            return Err(conflict());
        }
        remove_receipt(paths, &plan.project)
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        paths: AgentsPaths,
        project: PathBuf,
        target: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn fixture(original: &[u8]) -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "project-env-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let paths = AgentsPaths {
            claude_settings: root.join("claude/settings.json"),
            codex_config: root.join("codex/config.toml"),
            opencode_config: root.join("opencode/config.json"),
            gemini_settings: root.join("home/.gemini/settings.json"),
            gemini_env: root.join("home/.gemini/.env"),
            state_dir: root.join("state"),
        };
        crate::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            3427,
        )
        .apply()
        .unwrap();
        let target = project.join(".env");
        std::fs::write(&target, original).unwrap();
        Fixture {
            root,
            paths,
            project,
            target,
        }
    }
    #[test]
    fn addition_preserves_unrelated_crlf_permissions_and_keeps_values_out_of_receipt() {
        let original = b"# comment\r\nexport UNRELATED='private-user-value'\r\n";
        let f = fixture(original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f.target, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let plan = preview_add(&f.paths, &f.project, &f.target).unwrap();
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
        assert!(plan.changes.iter().all(|c| c.action == "add"));
        apply(&f.paths, &plan).unwrap();
        let expected = [
            original.as_slice(),
            b"GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:3427\r\nGEMINI_API_KEY=aubridge-gemini\r\n",
        ]
        .concat();
        assert_eq!(std::fs::read(&f.target).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&f.target).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(
                std::fs::metadata(receipt_path(&f.paths, &f.project))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let proof = std::fs::read_to_string(receipt_path(&f.paths, &f.project)).unwrap();
        for excluded in ["private-user-value", "http://127.0.0.1", "aubridge-gemini"] {
            assert!(!proof.contains(excluded));
        }
    }
    #[test]
    fn keep_existing_value_without_claiming_it_and_restore_missing_final_separator() {
        let original = b"GOOGLE_GEMINI_BASE_URL='http://127.0.0.1:3427' # keep\nUNRELATED=value";
        let f = fixture(original);
        let plan = preview_add(&f.paths, &f.project, &f.target).unwrap();
        assert_eq!(plan.changes[0].action, "keep");
        assert_eq!(plan.changes[1].action, "add");
        apply(&f.paths, &plan).unwrap();
        let undo = preview_undo(&f.paths, &f.project).unwrap();
        assert_eq!(undo.changes.len(), 1);
        assert_eq!(undo.changes[0].key, "GEMINI_API_KEY");
        apply(&f.paths, &undo).unwrap();
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
        assert!(apply(&f.paths, &plan).is_err());
    }
    #[test]
    fn external_duplicate_or_ambiguous_values_are_rejected_without_writes() {
        for original in [
            "GOOGLE_GEMINI_BASE_URL=https://external.invalid\n",
            "GEMINI_API_KEY=aubridge-gemini\nGEMINI_API_KEY=another\n",
            "UNRELATED=\"multiline\nGEMINI_API_KEY=value\n",
            "UNRELATED=value\rGEMINI_API_KEY=hidden\n",
        ] {
            let f = fixture(original.as_bytes());
            assert!(preview_add(&f.paths, &f.project, &f.target).is_err());
            assert_eq!(std::fs::read(&f.target).unwrap(), original.as_bytes());
            assert!(!receipt_path(&f.paths, &f.project).exists());
        }
    }
    #[test]
    fn any_change_between_preview_and_apply_is_rejected() {
        let f = fixture(b"UNRELATED=before\n");
        let plan = preview_add(&f.paths, &f.project, &f.target).unwrap();
        let changed = b"UNRELATED=after\n";
        std::fs::write(&f.target, changed).unwrap();
        assert!(apply(&f.paths, &plan).is_err());
        assert_eq!(std::fs::read(&f.target).unwrap(), changed);
        assert!(!receipt_path(&f.paths, &f.project).exists());
    }
    #[test]
    fn undo_survives_disconnect_and_port_change_and_preserves_unrelated_append() {
        let original = b"UNRELATED=before\n";
        let f = fixture(original);
        apply(
            &f.paths,
            &preview_add(&f.paths, &f.project, &f.target).unwrap(),
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.target)
            .unwrap()
            .write_all(b"LATER=external-user-edit\n")
            .unwrap();
        let old = crate::gemini::GeminiCli::new(
            f.paths.gemini_settings.clone(),
            f.paths.gemini_env.clone(),
            f.paths.state_dir.clone(),
            3427,
        );
        old.disconnect().unwrap();
        let undo = preview_undo(&f.paths, &f.project).unwrap();
        crate::gemini::GeminiCli::new(
            f.paths.gemini_settings.clone(),
            f.paths.gemini_env.clone(),
            f.paths.state_dir.clone(),
            4971,
        )
        .apply()
        .unwrap();
        apply(&f.paths, &undo).unwrap();
        assert_eq!(
            std::fs::read(&f.target).unwrap(),
            [original.as_slice(), b"LATER=external-user-edit\n"].concat()
        );
        assert!(preview_undo(&f.paths, &f.project).is_err());
    }
    #[test]
    fn undo_refuses_modified_owned_line_or_replaced_file() {
        let f = fixture(b"UNRELATED=before\n");
        apply(
            &f.paths,
            &preview_add(&f.paths, &f.project, &f.target).unwrap(),
        )
        .unwrap();
        let added = std::fs::read_to_string(&f.target).unwrap();
        let changed = added.replace("aubridge-gemini", "external-replacement");
        std::fs::write(&f.target, &changed).unwrap();
        assert!(preview_undo(&f.paths, &f.project).is_err());
        assert_eq!(std::fs::read_to_string(&f.target).unwrap(), changed);
        std::fs::write(&f.target, &added).unwrap();
        let replacement = f.project.join("replacement");
        std::fs::write(&replacement, &added).unwrap();
        std::fs::rename(&replacement, &f.target).unwrap();
        assert!(preview_undo(&f.paths, &f.project).is_err());
        assert_eq!(std::fs::read_to_string(&f.target).unwrap(), added);
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_hardlinks_and_symlinked_target_parent_are_not_written() {
        use std::os::unix::fs::symlink;
        let f = fixture(b"UNRELATED=before\n");
        let other = f.root.join("other.env");
        std::fs::write(&other, b"UNRELATED=before\n").unwrap();
        std::fs::remove_file(&f.target).unwrap();
        symlink(&other, &f.target).unwrap();
        assert!(preview_add(&f.paths, &f.project, &f.target).is_err());
        std::fs::remove_file(&f.target).unwrap();
        std::fs::hard_link(&other, &f.target).unwrap();
        assert!(preview_add(&f.paths, &f.project, &f.target).is_err());
        let directory = f.root.join("outside");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join(".env"), b"UNRELATED=before\n").unwrap();
        symlink(&directory, f.project.join(".gemini")).unwrap();
        assert!(preview_add(&f.paths, &f.project, &f.project.join(".gemini/.env")).is_err());
    }
    #[test]
    fn interrupted_add_promotion_recovers_ownership_without_an_env_backup() {
        let original = b"UNRELATED=value";
        let f = fixture(original);
        let plan = preview_add(&f.paths, &f.project, &f.target).unwrap();
        apply(&f.paths, &plan).unwrap();
        let (proof, _) = receipt(&f.paths, &f.project).unwrap();
        let mut proof = proof.unwrap();
        proof.phase = "add_intent".into();
        proof.prepared_identity = Some(proof.target_identity.clone());
        proof.target_identity = plan.stamp.identity;
        save_receipt(&f.paths, &proof).unwrap();
        let undo = preview_undo(&f.paths, &f.project).unwrap();
        apply(&f.paths, &undo).unwrap();
        assert_eq!(std::fs::read(&f.target).unwrap(), original);
    }
    #[test]
    fn pending_intent_does_not_claim_an_external_same_byte_replacement() {
        let f = fixture(b"UNRELATED=before\n");
        let plan = preview_add(&f.paths, &f.project, &f.target).unwrap();
        apply(&f.paths, &plan).unwrap();
        let (proof, _) = receipt(&f.paths, &f.project).unwrap();
        let mut proof = proof.unwrap();
        proof.phase = "add_intent".into();
        proof.prepared_identity = Some(proof.target_identity.clone());
        save_receipt(&f.paths, &proof).unwrap();
        let bytes = std::fs::read(&f.target).unwrap();
        let external = f.project.join("external-replacement");
        std::fs::write(&external, &bytes).unwrap();
        std::fs::rename(&external, &f.target).unwrap();
        assert!(
            preview_undo(&f.paths, &f.project).is_err(),
            "equal bytes do not prove the pending write's file identity"
        );
        assert_eq!(std::fs::read(&f.target).unwrap(), bytes);
    }
    #[test]
    fn undo_keeps_a_users_later_identical_last_line_newline() {
        let f = fixture(b"OTHER=1");
        apply(
            &f.paths,
            &preview_add(&f.paths, &f.project, &f.target).unwrap(),
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.target)
            .unwrap()
            .write_all(b"OTHER=1\n")
            .unwrap();
        let undo = preview_undo(&f.paths, &f.project).unwrap();
        apply(&f.paths, &undo).unwrap();
        assert_eq!(std::fs::read(&f.target).unwrap(), b"OTHER=1\nOTHER=1\n");
    }
}

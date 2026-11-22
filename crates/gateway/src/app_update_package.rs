//! Schema-2 release receipt and complete bundle checks (decision 56).
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, String>;
pub const LIMIT: u64 = 1024 * 1024 * 1024;
pub const BUNDLE_ID: &str = "dev.aubridge.smoke";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FileRecord {
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
}
pub type Manifest = BTreeMap<String, FileRecord>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Archive {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub schema: u32,
    pub app: String,
    pub version: String,
    pub target: String,
    pub git_revision: String,
    pub git_tree: String,
    pub mode: String,
    pub profile: String,
    pub signing: String,
    pub team_id: Option<String>,
    pub files: Manifest,
    pub archive: Archive,
}

impl Receipt {
    pub fn validate(&self, development: bool, team: Option<&str>) -> Result<()> {
        if self.schema != 2
            || self.app != "aubridge.app"
            || self.files.is_empty()
            || self.archive.size == 0
            || self.archive.size > LIMIT
            || self.archive.sha256.len() != 64
            || self.git_revision.len() != 40
            || self.git_tree.len() != 40
            || semver::Version::parse(&self.version).is_err()
            || self.target != format!("{}-apple-darwin", std::env::consts::ARCH)
        {
            return Err("invalid-schema2-receipt".into());
        }
        if development {
            if self.mode != "development" || self.signing != "local-ad-hoc" {
                return Err("development-identity-required".into());
            }
        } else if self.mode != "distribution"
            || self.profile != "release"
            || self.signing != "developer-id-notarized"
            || team.is_none()
            || self.team_id.as_deref() != team
        {
            return Err("trusted-developer-team-required".into());
        }
        Ok(())
    }
}

pub fn hash(path: &Path) -> Result<String> {
    let mut input = File::open(path).map_err(err)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let read = input.read(&mut buffer).map_err(err)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !fs::symlink_metadata(path)
        .map_err(err)?
        .file_type()
        .is_file()
    {
        return Err("update-input-not-regular-file".into());
    }
    let file = File::open(path).map_err(err)?;
    if !file.metadata().map_err(err)?.is_file() {
        return Err("update-input-not-regular-file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() as u64 > limit {
        return Err("update-input-size-limit".into());
    }
    Ok(bytes)
}

pub fn safe_name(name: &str) -> Result<&str> {
    // Reject non-ASCII names rather than depending on filesystem Unicode folding.
    let name = name.trim_end_matches('/');
    let parts: Vec<_> = name.split('/').collect();
    if !name.is_ascii()
        || name.contains(['\\', '\0', ':'])
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        || !matches!(parts.first(), Some(&"aubridge.app") | Some(&"__MACOSX"))
        || (parts[0] == "__MACOSX"
            && parts.len() > 1
            && !matches!(parts[1], "aubridge.app" | "._aubridge.app"))
    {
        return Err("unsafe-archive-path".into());
    }
    Ok(name)
}

pub fn unique_name(name: &str, names: &mut BTreeSet<String>) -> Result<()> {
    let name = safe_name(name)?;
    for existing in names.iter() {
        let mut first = name.split('/');
        let mut second = existing.split('/');
        while let (Some(a), Some(b)) = (first.next(), second.next()) {
            if !a.eq_ignore_ascii_case(b) {
                break;
            }
            if a != b {
                return Err("archive-path-alias".into());
            }
        }
    }
    if !names.insert(name.into()) {
        return Err("archive-path-alias".into());
    }
    Ok(())
}

pub fn safe_link(name: &str, target: &str) -> Result<()> {
    if !target.is_ascii() || target.starts_with('/') || target.contains(['\\', '\0', ':']) {
        return Err("unsafe-archive-link".into());
    }
    let mut parts: Vec<_> = name.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() <= 1 {
                    return Err("archive-link-escape".into());
                }
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    if parts.first() != Some(&"aubridge.app") {
        return Err("archive-link-escape".into());
    }
    Ok(())
}

pub fn manifest(app: &Path) -> Result<Manifest> {
    let root = app.canonicalize().map_err(err)?;
    if fs::symlink_metadata(app)
        .map_err(err)?
        .file_type()
        .is_symlink()
    {
        return Err("bundle-root-is-symlink".into());
    }
    let mut result = Manifest::new();
    let mut pending = vec![app.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(directory).map_err(err)? {
            let path = item.map_err(err)?.path();
            let metadata = fs::symlink_metadata(&path).map_err(err)?;
            let relative = path
                .strip_prefix(app)
                .map_err(err)?
                .to_str()
                .ok_or("non-utf8-bundle-path")?
                .to_owned();
            let mut record = FileRecord {
                mode: metadata.permissions().mode() & 0o7777,
                sha256: None,
                directory: None,
                symlink: None,
            };
            if metadata.file_type().is_symlink() {
                if !path.canonicalize().map_err(err)?.starts_with(&root) {
                    return Err("bundle-link-escape".into());
                }
                record.symlink = Some(
                    fs::read_link(&path)
                        .map_err(err)?
                        .to_str()
                        .ok_or("non-utf8-link")?
                        .into(),
                );
            } else if metadata.is_dir() {
                record.directory = Some(true);
                pending.push(path);
            } else if metadata.is_file() {
                record.sha256 = Some(hash(&path)?);
            } else {
                return Err("bundle-special-file".into());
            }
            result.insert(relative, record);
            if result.len() > 10000 {
                return Err("bundle-entry-limit".into());
            }
        }
    }
    Ok(result)
}

pub fn command(program: &str, arguments: &[&std::ffi::OsStr]) -> Result<String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(err)?;
    if !output.status.success() {
        return Err(format!("native-check-failed:{program}"));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

pub fn executable(app: &Path) -> Result<PathBuf> {
    let info = plist::Value::from_reader(std::io::Cursor::new(read_bounded(
        &app.join("Contents/Info.plist"),
        4 * 1024 * 1024,
    )?))
    .map_err(err)?;
    let info = info.as_dictionary().ok_or("invalid-info-plist")?;
    if info
        .get("CFBundleIdentifier")
        .and_then(plist::Value::as_string)
        != Some(BUNDLE_ID)
    {
        return Err("unexpected-bundle-id".into());
    }
    let name = info
        .get("CFBundleExecutable")
        .and_then(plist::Value::as_string)
        .ok_or("missing-executable")?;
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err("unsafe-executable-name".into());
    }
    let path = app.join("Contents/MacOS").join(name);
    let metadata = fs::symlink_metadata(&path).map_err(err)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("invalid-app-executable".into());
    }
    Ok(path)
}

pub fn check_app(
    app: &Path,
    receipt: &Receipt,
    development: bool,
    team: Option<&str>,
) -> Result<()> {
    receipt.validate(development, team)?;
    executable(app)?;
    if manifest(app)? != receipt.files {
        return Err("bundle-manifest-mismatch".into());
    }
    let info = plist::Value::from_reader(std::io::Cursor::new(read_bounded(
        &app.join("Contents/Info.plist"),
        4 * 1024 * 1024,
    )?))
    .map_err(err)?;
    let info = info.as_dictionary().ok_or("invalid-info-plist")?;
    if info
        .get("AubridgeGitRevision")
        .and_then(plist::Value::as_string)
        != Some(receipt.git_revision.as_str())
        || info
            .get("CFBundleShortVersionString")
            .and_then(plist::Value::as_string)
            != Some(receipt.version.as_str())
    {
        return Err("bundle-version-mismatch".into());
    }
    let build: serde_json::Value = serde_json::from_slice(&read_bounded(
        &app.join("Contents/Resources/aubridge-build.json"),
        4 * 1024 * 1024,
    )?)
    .map_err(err)?;
    for (key, value) in [
        ("version", &receipt.version),
        ("target", &receipt.target),
        ("git_revision", &receipt.git_revision),
        ("git_tree", &receipt.git_tree),
        ("mode", &receipt.mode),
        ("profile", &receipt.profile),
    ] {
        if build.get(key).and_then(serde_json::Value::as_str) != Some(value.as_str()) {
            return Err("bundle-build-mismatch".into());
        }
    }
    command(
        "/usr/bin/codesign",
        &[
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            app.as_os_str(),
        ],
    )?;
    if !development {
        let details = command(
            "/usr/bin/codesign",
            &[
                "--display".as_ref(),
                "--verbose=4".as_ref(),
                app.as_os_str(),
            ],
        )?;
        if !details
            .lines()
            .any(|line| line == format!("TeamIdentifier={}", team.unwrap_or_default()))
            || !details
                .lines()
                .any(|line| line.starts_with("Authority=Developer ID Application:"))
            || !details
                .lines()
                .filter_map(|line| line.find("flags=").map(|index| &line[index + 6..]))
                .any(|flags| {
                    flags
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|word| word == "runtime")
                })
        {
            return Err("developer-signature-mismatch".into());
        }
        command(
            "/usr/sbin/spctl",
            &[
                "--assess".as_ref(),
                "--type".as_ref(),
                "execute".as_ref(),
                app.as_os_str(),
            ],
        )?;
    }
    Ok(())
}

pub fn preflight_zip(path: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(path).map_err(err)?).map_err(err)?;
    if archive.len() > 10000 {
        return Err("archive-entry-limit".into());
    }
    archive.validate_header_consistency().map_err(err)?;
    let mut names = BTreeSet::new();
    let mut links = BTreeSet::new();
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index_with_options(
                index,
                zip::read::ZipReadOptions::new().enforce_header_consistency(true),
            )
            .map_err(err)?;
        let name = file.name().map_err(err)?.trim_end_matches('/').to_owned();
        unique_name(&name, &mut names)?;
        if file.encrypted()
            || !matches!(
                file.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
        {
            return Err("unsupported-archive-entry".into());
        }
        let kind = file.unix_mode().unwrap_or(0) & 0o170000;
        if !matches!(kind, 0 | 0o100000 | 0o040000 | 0o120000) {
            return Err("archive-special-file".into());
        }
        let declared = file.size();
        expanded = expanded
            .checked_add(declared)
            .ok_or("archive-size-overflow")?;
        if expanded > LIMIT {
            return Err("archive-expanded-limit".into());
        }
        if kind == 0o120000 {
            if name.starts_with("__MACOSX/") || declared > 4096 {
                return Err("unsafe-archive-link".into());
            }
            let mut target = String::new();
            file.by_ref()
                .take(4097)
                .read_to_string(&mut target)
                .map_err(err)?;
            if target.len() as u64 != declared {
                return Err("archive-link-size-mismatch".into());
            }
            safe_link(&name, &target)?;
            links.insert(name.to_ascii_lowercase());
        } else {
            let read = io::copy(
                &mut file.by_ref().take(declared.saturating_add(1)),
                &mut io::sink(),
            )
            .map_err(err)?;
            if read != declared {
                return Err("archive-inflated-size-mismatch".into());
            }
        }
    }
    for name in &names {
        let mut parent = Path::new(name).parent();
        while let Some(path) = parent {
            if links.contains(
                &path
                    .to_str()
                    .ok_or("invalid-archive-parent")?
                    .to_ascii_lowercase(),
            ) {
                return Err("archive-link-parent".into());
            }
            parent = path.parent();
        }
    }
    Ok(())
}

pub fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
#[path = "app_update_package_tests.rs"]
mod tests;

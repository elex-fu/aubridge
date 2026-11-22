//! Explicit trusted-channel discovery and manual idle update coordination (D56).
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::app_update_helper::{self as helper, Journal};
use crate::app_update_package::{self as package, err, Receipt, Result, LIMIT};
use crate::state::{GatewayState, UpdateMaintenanceLease};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// Production channel is intentionally unavailable until independently provisioned.
// These are compile-time trust roots, never environment/UI/receipt overrides.
const FEED: Option<&str> = None;
const TEAM: Option<&str> = None;
const KEY: Option<[u8; 32]> = None;
const CHANNEL: &str = "stable";

#[derive(Clone, Serialize)]
pub struct UpdateStatus {
    pub state: String,
    pub version: Option<String>,
    pub offer_handle: Option<String>,
    pub prepared_handle: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct Offer {
    pub channel: String,
    pub platform: String,
    pub arch: String,
    pub version: String,
    pub expires_unix: u64,
    pub receipt_path: String,
    pub receipt_sha256: String,
    pub archive_path: String,
    pub size: u64,
    pub sha256: String,
}

struct Inner {
    fresh: bool,
    status: UpdateStatus,
    offer: Option<(String, Offer)>,
    prepared: Option<(String, Journal)>,
}

pub struct UpdateService {
    inner: Mutex<Inner>,
}

impl Default for UpdateService {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                fresh: true,
                status: UpdateStatus {
                    state: if FEED.is_some() && KEY.is_some() && TEAM.is_some() {
                        "checking"
                    } else {
                        "not_configured"
                    }
                    .into(),
                    version: None,
                    offer_handle: None,
                    prepared_handle: None,
                    reason: None,
                },
                offer: None,
                prepared: None,
            }),
        }
    }

    pub fn status(&self) -> UpdateStatus {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            .clone()
    }

    pub fn persisted_status(&self, app: &Path) -> UpdateStatus {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut status = inner.status.clone();
        if !inner.fresh {
            return status;
        }
        drop(inner);
        if let Ok(journal) = helper::load(app) {
            status.state = match journal.phase.as_str() {
                "installed" => "installed",
                "rolled-back" => "rolled_back",
                "failed" => "failed",
                "old-saved"
                | "candidate-installed"
                | "starting"
                | "rollback-starting"
                | "handshake" => "installing",
                _ => return status,
            }
            .into();
            status.reason = journal.failure;
            status.version = Some(if journal.phase == "rolled-back" {
                journal.previous_receipt.version
            } else {
                journal.receipt.version
            });
        }
        status
    }

    fn transition(&self, state: &str, reason: Option<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.fresh = false;
        inner.status.state = state.into();
        inner.status.reason = reason;
    }

    pub async fn check(&self) -> Result<UpdateStatus> {
        {
            let mut inner = self.inner.lock().map_err(err)?;
            inner.offer = None;
            inner.status.offer_handle = None;
        }
        let (Some(feed), Some(key), Some(_)) = (FEED, KEY, TEAM) else {
            self.transition(
                "not_configured",
                Some("trusted-channel-not-configured".into()),
            );
            return Ok(self.status());
        };
        self.transition("checking", None);
        let result: Result<UpdateStatus> = async {
            let bytes = fetch_bytes(&client()?, feed, 256 * 1024, 15).await?;
            let offer = verify_envelope(&bytes, &key, now())?;
            if offer.channel != CHANNEL
                || offer.platform != "macos"
                || offer.arch != std::env::consts::ARCH
                || offer.size > LIMIT
            {
                return Err("feed-platform-channel-mismatch".into());
            }
            same_origin(feed, &offer.receipt_path)?;
            same_origin(feed, &offer.archive_path)?;
            if !is_newer(crate::build_info::current().version, &offer.version)? {
                self.transition("current", None);
                return Ok(self.status());
            }
            let handle = token()?;
            let mut inner = self.inner.lock().map_err(err)?;
            inner.status.state = "available".into();
            inner.status.version = Some(offer.version.clone());
            inner.status.offer_handle = Some(handle.clone());
            inner.offer = Some((handle, offer));
            Ok(inner.status.clone())
        }
        .await;
        if let Err(error) = &result {
            self.transition("failed", Some(error.clone()));
        }
        result
    }

    pub async fn prepare(&self, handle: &str, app: &Path, data: &Path) -> Result<UpdateStatus> {
        let offer = {
            let inner = self.inner.lock().map_err(err)?;
            let (expected, offer) = inner.offer.as_ref().ok_or("offer-unavailable")?;
            if expected != handle || offer.expires_unix <= now() {
                return Err("offer-expired-or-unbound".into());
            }
            offer.clone()
        };
        self.transition("preparing", None);
        let result: Result<UpdateStatus> = async {
            let feed = FEED.ok_or("trusted-channel-not-configured")?;
            let client = client()?;
            let receipt_bytes = fetch_bytes(
                &client,
                same_origin(feed, &offer.receipt_path)?.as_str(),
                4 * 1024 * 1024,
                30,
            )
            .await?;
            if format!("{:x}", Sha256::digest(&receipt_bytes)) != offer.receipt_sha256 {
                return Err("receipt-hash-mismatch".into());
            }
            let receipt: Receipt = serde_json::from_slice(&receipt_bytes).map_err(err)?;
            receipt.validate(false, TEAM)?;
            if receipt.version != offer.version
                || receipt.archive.size != offer.size
                || receipt.archive.sha256 != offer.sha256
            {
                return Err("feed-receipt-mismatch".into());
            }
            let transaction = transaction(app)?;
            let archive = transaction.join("package.zip");
            download(
                &client,
                same_origin(feed, &offer.archive_path)?.as_str(),
                &archive,
                offer.size,
                &offer.sha256,
            )
            .await?;
            self.finish_prepare(app, data, transaction, &archive, receipt, false)
        }
        .await;
        if let Err(error) = &result {
            self.transition("failed", Some(error.clone()));
        }
        result
    }

    /// Caller supplies paths from an explicit native local-file selection and an independently confirmed SHA.
    /// This operation never accepts remote URLs or weakens public-channel verification.
    pub fn prepare_development(
        &self,
        archive: &Path,
        receipt_path: &Path,
        independent_sha256: &str,
        confirmed: bool,
        app: &Path,
        data: &Path,
    ) -> Result<UpdateStatus> {
        if !confirmed || independent_sha256.len() != 64 {
            return Err("explicit-development-confirmation-required".into());
        }
        let receipt_bytes = package::read_bounded(receipt_path, 4 * 1024 * 1024)?;
        let receipt: Receipt = serde_json::from_slice(&receipt_bytes).map_err(err)?;
        receipt.validate(true, None)?;
        if receipt.archive.sha256 != independent_sha256 {
            return Err("independent-archive-hash-mismatch".into());
        }
        let transaction = transaction(app)?;
        let trusted = transaction.join("package.zip");
        if !fs::symlink_metadata(archive)
            .map_err(err)?
            .file_type()
            .is_file()
        {
            return Err("archive-not-regular-file".into());
        }
        let mut source = File::open(archive).map_err(err)?;
        if !source.metadata().map_err(err)?.is_file() {
            return Err("archive-not-regular-file".into());
        }
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&trusted)
            .map_err(err)?;
        let copied =
            std::io::copy(&mut (&mut source).take(LIMIT + 1), &mut destination).map_err(err)?;
        destination.sync_all().map_err(err)?;
        if copied != receipt.archive.size
            || copied > LIMIT
            || package::hash(&trusted)? != independent_sha256
        {
            return Err("archive-hash-size-mismatch".into());
        }
        self.finish_prepare(app, data, transaction, &trusted, receipt, true)
    }

    fn finish_prepare(
        &self,
        app: &Path,
        data: &Path,
        transaction: PathBuf,
        archive: &Path,
        receipt: Receipt,
        development: bool,
    ) -> Result<UpdateStatus> {
        let space = package::command("/bin/df", &["-Pk".as_ref(), transaction.as_os_str()])?;
        let available = space
            .lines()
            .last()
            .and_then(|row| row.split_whitespace().nth(3))
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("cannot-check-install-space")?
            .saturating_mul(1024);
        if available
            < receipt
                .archive
                .size
                .saturating_mul(4)
                .saturating_add(128 * 1024 * 1024)
        {
            return Err("insufficient-update-space".into());
        }
        package::preflight_zip(archive)?;
        let extracted = transaction.join("unpacked");
        fs::create_dir(&extracted).map_err(err)?;
        package::command(
            "/usr/bin/ditto",
            &[
                "-x".as_ref(),
                "-k".as_ref(),
                archive.as_os_str(),
                extracted.as_os_str(),
            ],
        )?;
        let pending = transaction.join("pending.app");
        fs::rename(extracted.join("aubridge.app"), &pending).map_err(err)?;
        package::check_app(&pending, &receipt, development, TEAM)?;
        let previous_files = package::manifest(app)?;
        if previous_files == receipt.files {
            return Err("update-no-change".into());
        }
        let build: serde_json::Value = serde_json::from_slice(&package::read_bounded(
            &app.join("Contents/Resources/aubridge-build.json"),
            4 * 1024 * 1024,
        )?)
        .map_err(err)?;
        let field = |key: &str| {
            build
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(String::from)
                .ok_or_else(|| format!("installed-build-missing:{key}"))
        };
        let previous_receipt = Receipt {
            schema: 2,
            app: "aubridge.app".into(),
            version: field("version")?,
            target: field("target")?,
            git_revision: field("git_revision")?,
            git_tree: field("git_tree")?,
            mode: field("mode")?,
            profile: field("profile")?,
            signing: if development {
                "local-ad-hoc"
            } else {
                "developer-id-notarized"
            }
            .into(),
            team_id: TEAM.map(String::from),
            files: previous_files.clone(),
            archive: receipt.archive.clone(),
        };
        package::check_app(app, &previous_receipt, development, TEAM)?;
        let _lock = helper::install_lock(app)?;
        helper::legacy_install_idle(app)?;
        if helper::journal_path(app)?.exists() {
            let existing = helper::load(app)?;
            if !matches!(
                existing.phase.as_str(),
                "installed" | "rolled-back" | "aborted" | "prepared"
            ) {
                return Err("interrupted-update-needs-recovery".into());
            }
        }
        let mut journal = Journal {
            schema: 1,
            app: app.to_path_buf(),
            data_dir: data.canonicalize().map_err(err)?,
            transaction,
            phase: "prepared".into(),
            candidate_files: receipt.files.clone(),
            previous_files,
            receipt,
            previous_receipt,
            parent_pid: std::process::id(),
            helper_pid: None,
            pid: None,
            development,
            team: TEAM.map(String::from),
            failure: None,
        };
        helper::save(&mut journal, "prepared")?;
        let handle = token()?;
        let mut inner = self.inner.lock().map_err(err)?;
        inner.fresh = false;
        inner.status.state = "ready".into();
        inner.status.reason = None;
        inner.status.prepared_handle = Some(handle.clone());
        inner.status.version = Some(journal.receipt.version.clone());
        inner.prepared = Some((handle, journal));
        Ok(inner.status.clone())
    }

    fn prepared(&self, handle: &str, confirmed: bool) -> Result<Journal> {
        if !confirmed {
            return Err("update-confirmation-required".into());
        }
        let inner = self.inner.lock().map_err(err)?;
        let (expected, journal) = inner
            .prepared
            .as_ref()
            .ok_or("prepared-update-unavailable")?;
        if handle != expected {
            return Err("prepared-handle-unbound".into());
        }
        Ok(journal.clone())
    }

    pub fn install(
        &self,
        state: &GatewayState,
        handle: &str,
        confirmed: bool,
    ) -> Result<TrustedHelperReady> {
        let journal = self.prepared(handle, confirmed)?;
        let lease = match state.try_update_maintenance() {
            Ok(lease) => lease,
            Err(error) => {
                self.transition("busy", Some(error.into()));
                return Err(error.into());
            }
        };
        if package::manifest(&journal.app)? != journal.previous_files {
            return Err("installed-app-changed-since-prepare".into());
        }
        let persisted = helper::load(&journal.app)?;
        if persisted != journal || persisted.parent_pid != std::process::id() {
            return Err("prepared-journal-changed".into());
        }
        package::check_app(
            &journal.transaction.join("pending.app"),
            &journal.receipt,
            journal.development,
            TEAM,
        )?;
        let mut child = Command::new(package::executable(&journal.app)?)
            .arg("--aubridge-update-helper")
            .arg(&journal.app)
            .arg(helper::binding(&journal)?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(err)?;
        let output = child.stdout.take().ok_or("helper-stdout-unavailable")?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output)
                .take(128)
                .read_line(&mut line)
                .map(|_| line);
            let _ = sender.send(result);
        });
        match receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(line)) if line.trim() == "READY" && child.try_wait().map_err(err)?.is_none() => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                self.transition("ready", Some("trusted-helper-not-ready".into()));
                return Err("trusted-helper-not-ready".into());
            }
        }
        self.transition("installing", None);
        Ok(TrustedHelperReady {
            _lease: lease,
            _helper: child,
        })
    }
}

/// Desktop must retain this value until app.exit(0), after READY, through natural shutdown.
pub struct TrustedHelperReady {
    _lease: UpdateMaintenanceLease,
    _helper: Child,
}

pub fn installed_app() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(err)?;
    let app = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("not-running-in-app-bundle")?;
    if app.extension().and_then(|s| s.to_str()) != Some("app") || package::executable(app)? != exe {
        return Err("not-running-in-app-bundle".into());
    }
    Ok(app.to_path_buf())
}

fn transaction(app: &Path) -> Result<PathBuf> {
    let parent = app
        .parent()
        .ok_or("missing-app-parent")?
        .canonicalize()
        .map_err(err)?;
    if app.parent() != Some(parent.as_path())
        || fs::symlink_metadata(app)
            .map_err(err)?
            .file_type()
            .is_symlink()
    {
        return Err("unsafe-install-path".into());
    }
    let path = parent.join(format!(".aubridge-upgrade-{}", token()?));
    fs::create_dir(&path).map_err(err)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(err)?;
    // Same-volume staging and a real writable allocation precede the idle handoff.
    Ok(path)
}

pub fn is_newer(installed: &str, offered: &str) -> Result<bool> {
    let mut old = semver::Version::parse(installed).map_err(err)?;
    let mut new = semver::Version::parse(offered).map_err(err)?;
    old.build = semver::BuildMetadata::EMPTY;
    new.build = semver::BuildMetadata::EMPTY;
    Ok(new > old)
}

pub fn verify_envelope(bytes: &[u8], key: &[u8; 32], timestamp: u64) -> Result<Offer> {
    #[derive(Deserialize)]
    struct Envelope {
        payload: String,
        signature: String,
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(err)?;
    let signature = decode_hex(&envelope.signature)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(envelope.payload.as_bytes(), &signature)
        .map_err(|_| "feed-signature-invalid")?;
    let offer: Offer = serde_json::from_str(&envelope.payload).map_err(err)?;
    semver::Version::parse(&offer.version).map_err(err)?;
    if offer.expires_unix <= timestamp {
        return Err("feed-expired".into());
    }
    Ok(offer)
}

fn decode_hex(input: &str) -> Result<Vec<u8>> {
    if input.len() != 128 || !input.is_ascii() {
        return Err("invalid-signature-encoding".into());
    }
    (0..input.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&input[i..i + 2], 16).map_err(err))
        .collect()
}

fn token() -> Result<String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(err)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(err)
}
fn same_origin(feed: &str, path: &str) -> Result<reqwest::Url> {
    let base = reqwest::Url::parse(feed).map_err(err)?;
    let url = base.join(path).map_err(err)?;
    if url.scheme() != "https"
        || url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("feed-url-origin-mismatch".into());
    }
    Ok(url)
}
async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
    seconds: u64,
) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(err)?
            .error_for_status()
            .map_err(err)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(err)? {
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err("update-download-limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| "update-download-timeout".to_owned())?
}
async fn download(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    size: u64,
    sha: &str,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(600), async {
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(err)?
            .error_for_status()
            .map_err(err)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(err)?;
        let mut total = 0_u64;
        let mut digest = Sha256::new();
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(30), response.chunk())
                .await
                .map_err(|_| "update-download-idle-timeout")?
                .map_err(err)?;
            let Some(chunk) = chunk else {
                break;
            };
            total = total.saturating_add(chunk.len() as u64);
            if total > size || total > LIMIT {
                return Err("update-download-size-limit".into());
            }
            digest.update(&chunk);
            output.write_all(&chunk).map_err(err)?;
        }
        output.sync_all().map_err(err)?;
        if total != size || format!("{:x}", digest.finalize()) != sha {
            return Err("update-download-hash-mismatch".into());
        }
        Ok(())
    })
    .await
    .map_err(|_| "update-download-total-timeout".to_owned())?
}

#[cfg(test)]
#[path = "app_update_tests.rs"]
mod tests;

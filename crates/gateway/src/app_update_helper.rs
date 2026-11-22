//! Trusted installed executable owns replacement, journal recovery and launch verification.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::app_update_package::{self as package, err, Manifest, Receipt, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Journal {
    pub schema: u32,
    pub app: PathBuf,
    pub data_dir: PathBuf,
    pub transaction: PathBuf,
    pub phase: String,
    pub candidate_files: Manifest,
    pub previous_files: Manifest,
    pub receipt: Receipt,
    pub previous_receipt: Receipt,
    pub parent_pid: u32,
    pub helper_pid: Option<u32>,
    pub pid: Option<u32>,
    pub development: bool,
    pub team: Option<String>,
    pub failure: Option<String>,
}

pub fn journal_path(app: &Path) -> Result<PathBuf> {
    // Python release_install.py owns .upgrade.json; its schema-1 shape differs.
    // Keep the same installer lock, but never reinterpret/overwrite its journal.
    Ok(app.with_file_name(format!(
        ".{}.native-upgrade.json",
        app.file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid-app-name")?
    )))
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(err)?;
    file.write_all(&serde_json::to_vec(value).map_err(err)?)
        .map_err(err)?;
    file.sync_all().map_err(err)?;
    fs::rename(&temp, path).map_err(err)?;
    File::open(path.parent().ok_or("missing-journal-parent")?)
        .map_err(err)?
        .sync_all()
        .map_err(err)
}

pub fn save(journal: &mut Journal, phase: &str) -> Result<()> {
    journal.phase = phase.into();
    atomic_json(&journal.transaction.join("journal.json"), journal)?;
    atomic_json(&journal_path(&journal.app)?, journal)
}

fn validate(journal: &Journal) -> Result<()> {
    if journal.schema != 1
        || journal.transaction.parent() != journal.app.parent()
        || !journal
            .transaction
            .file_name()
            .and_then(|p| p.to_str())
            .is_some_and(|p| p.starts_with(".aubridge-upgrade-"))
        || fs::symlink_metadata(&journal.transaction)
            .map_err(err)?
            .file_type()
            .is_symlink()
        || journal.transaction.canonicalize().map_err(err)? != journal.transaction
        || journal.data_dir.canonicalize().map_err(err)? != journal.data_dir
        || journal
            .app
            .parent()
            .ok_or("invalid-app-parent")?
            .canonicalize()
            .map_err(err)?
            != journal.app.parent().ok_or("invalid-app-parent")?
    {
        return Err("invalid-update-journal-binding".into());
    }
    if journal.receipt.files != journal.candidate_files
        || journal.previous_receipt.files != journal.previous_files
    {
        return Err("invalid-update-journal-manifest".into());
    }
    Ok(())
}

pub fn load(app: &Path) -> Result<Journal> {
    let path = journal_path(app)?;
    if fs::symlink_metadata(&path)
        .map_err(err)?
        .file_type()
        .is_symlink()
    {
        return Err("symlink-journal".into());
    }
    let bytes = package::read_bounded(&path, 16 * 1024 * 1024)?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(err)?;
    if journal.app != app {
        return Err("journal-target-mismatch".into());
    }
    validate(&journal)?;
    Ok(journal)
}

fn lock(journal: &Journal) -> Result<File> {
    install_lock(&journal.app)
}

pub fn install_lock(app: &Path) -> Result<File> {
    let path = app.with_file_name(format!(
        ".{}.upgrade.lock",
        app.file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid-app-name")?
    ));
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("symlink-install-lock".into());
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(err)?;
    file.try_lock().map_err(err)?;
    Ok(file)
}

pub fn legacy_install_idle(app: &Path) -> Result<()> {
    let path = app.with_file_name(format!(
        ".{}.upgrade.json",
        app.file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid-app-name")?
    ));
    if !path.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(&path)
        .map_err(err)?
        .file_type()
        .is_symlink()
    {
        return Err("legacy-journal-is-symlink".into());
    }
    let bytes = package::read_bounded(&path, 16 * 1024 * 1024)?;
    let journal: serde_json::Value = serde_json::from_slice(&bytes).map_err(err)?;
    if !matches!(
        journal["phase"].as_str(),
        Some("installed" | "rolled-back" | "rolled-back-empty" | "aborted")
    ) {
        return Err("legacy-install-transaction-active".into());
    }
    Ok(())
}

pub fn process_executable(pid: u32) -> Option<PathBuf> {
    package::command(
        "/bin/ps",
        &[
            "-ww".as_ref(),
            "-p".as_ref(),
            pid.to_string().as_ref(),
            "-o".as_ref(),
            "comm=".as_ref(),
        ],
    )
    .ok()
    .map(|s| PathBuf::from(s.trim()))
    .filter(|p| !p.as_os_str().is_empty())
}

fn require_only_pid(app: &Path, allowed: &[u32]) -> Result<()> {
    let exe = package::executable(app)?;
    let rows = package::command(
        "/bin/ps",
        &["-ww".as_ref(), "-axo".as_ref(), "pid=,comm=".as_ref()],
    )?;
    for row in rows.lines() {
        let row = row.trim();
        let Some(index) = row.find(char::is_whitespace) else {
            continue;
        };
        if Path::new(row[index..].trim()) == exe {
            let pid = row[..index].parse::<u32>().map_err(err)?;
            if !allowed.contains(&pid) {
                return Err("unowned-app-process-running".into());
            }
        }
    }
    Ok(())
}

fn rename(source: &Path, target: &Path) -> Result<()> {
    if target.symlink_metadata().is_ok() {
        return Err("rename-target-exists".into());
    }
    fs::rename(source, target).map_err(err)?;
    File::open(source.parent().ok_or("missing-source-parent")?)
        .map_err(err)?
        .sync_all()
        .map_err(err)?;
    File::open(target.parent().ok_or("missing-target-parent")?)
        .map_err(err)?
        .sync_all()
        .map_err(err)
}

fn matches(path: &Path, expected: &Manifest) -> Result<bool> {
    if path.symlink_metadata().is_err() {
        return Ok(false);
    }
    Ok(package::manifest(path)? == *expected)
}

fn launch(journal: &mut Journal, rollback: bool) -> Result<Child> {
    let exe = package::executable(&journal.app)?;
    let log = journal.transaction.join(if rollback {
        "rollback-launch.log"
    } else {
        "launch.log"
    });
    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .map_err(err)?;
    // A child may inspect the journal immediately after spawn. Publish the new
    // recovery helper and launch phase before creating any owned process.
    record_launch_intent(journal, rollback)?;
    let mut child = Command::new(exe)
        .arg("--aubridge-update-launch")
        .arg(&journal.app)
        .env("PLUGINS_DIR", journal.data_dir.join("plugins"))
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(err)?)
        .stderr(output)
        .spawn()
        .map_err(err)?;
    journal.pid = Some(child.id());
    if let Err(error) = save(
        journal,
        if rollback {
            "rollback-starting"
        } else {
            "starting"
        },
    ) {
        if child.try_wait().map_err(err)?.is_none() {
            child.kill().map_err(err)?;
            child.wait().map_err(err)?;
        }
        return Err(error);
    }
    Ok(child)
}

fn record_launch_intent(journal: &mut Journal, rollback: bool) -> Result<()> {
    journal.helper_pid = Some(std::process::id());
    journal.pid = None;
    save(
        journal,
        if rollback {
            "rollback-starting"
        } else {
            "starting"
        },
    )
}

fn verify_runtime(journal: &Journal, pid: u32, rollback: bool) -> Result<()> {
    let expected = if rollback {
        &journal.previous_receipt
    } else {
        &journal.receipt
    };
    let exe = package::executable(&journal.app)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let runtime = tokio::runtime::Runtime::new().map_err(err)?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(err)?;
        while Instant::now() < deadline {
            if process_executable(pid).as_deref() != Some(exe.as_path()) {
                return Err("candidate-process-exited".into());
            }
            let check = async {
                let bytes =
                    package::read_bounded(&journal.data_dir.join("gateway-session.json"), 65536)?;
                let session: crate::runtime::GatewayAccess =
                    serde_json::from_slice(&bytes).map_err(err)?;
                let url = reqwest::Url::parse(&session.endpoint).map_err(err)?;
                if url.scheme() != "http"
                    || url.host_str() != Some("127.0.0.1")
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || !matches!(url.path(), "" | "/")
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err("invalid-runtime-endpoint".into());
                }
                let port = url.port().ok_or("missing-runtime-port")?;
                let listeners = package::command(
                    "/usr/sbin/lsof",
                    &[
                        "-nP".as_ref(),
                        format!("-iTCP:{port}").as_ref(),
                        "-sTCP:LISTEN".as_ref(),
                        "-Fp".as_ref(),
                    ],
                )?;
                let pids: std::collections::BTreeSet<_> = listeners
                    .lines()
                    .filter_map(|l| l.strip_prefix('p'))
                    .collect();
                let pid_string = pid.to_string();
                if pids.len() != 1 || !pids.contains(pid_string.as_str()) {
                    return Err("runtime-socket-owner-mismatch".into());
                }
                let base = session.endpoint.trim_end_matches('/');
                client
                    .get(format!("{base}/healthz"))
                    .send()
                    .await
                    .map_err(err)?
                    .error_for_status()
                    .map_err(err)?;
                let response = client
                    .get(format!("{base}/admin/build"))
                    .bearer_auth(session.admin_token)
                    .send()
                    .await
                    .map_err(err)?
                    .error_for_status()
                    .map_err(err)?;
                let build: serde_json::Value = response.json().await.map_err(err)?;
                for (key, value) in [
                    ("version", &expected.version),
                    ("git_revision", &expected.git_revision),
                    ("git_tree", &expected.git_tree),
                    ("mode", &expected.mode),
                    ("profile", &expected.profile),
                ] {
                    if build.get(key).and_then(serde_json::Value::as_str) != Some(value.as_str()) {
                        return Err("runtime-build-mismatch".into());
                    }
                }
                if build["pid"].as_u64() != Some(pid.into()) || build["source_verified"] != true {
                    return Err("runtime-build-unverified".into());
                }
                if package::manifest(&journal.app)? != expected.files {
                    return Err("runtime-manifest-mismatch".into());
                }
                Ok::<(), String>(())
            }
            .await;
            if check.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Err("runtime-health-deadline".into())
    })
}

fn recover(journal: &mut Journal, mut child: Option<&mut Child>) -> Result<()> {
    // Manifest CAS is authoritative even when power failed between rename and checkpoint.
    let previous = journal.transaction.join("previous.app");
    let current_old = matches(&journal.app, &journal.previous_files)?;
    let current_new = matches(&journal.app, &journal.candidate_files)?;
    if current_old && current_new {
        return Err("ambiguous-identical-update-manifests".into());
    }
    if journal.app.exists() && !current_old && !current_new {
        return Err("installed-files-changed-backup-preserved".into());
    }
    if previous.exists() && !matches(&previous, &journal.previous_files)? {
        return Err("backup-manifest-mismatch".into());
    }
    if current_new {
        // Never move the only viable bundle before proving rollback is available.
        if !previous.exists() {
            return Err("previous-app-unavailable-current-preserved".into());
        }
        package::check_app(
            &previous,
            &journal.previous_receipt,
            journal.development,
            journal.team.as_deref(),
        )?;
        if journal
            .transaction
            .join("failed.app")
            .symlink_metadata()
            .is_ok()
        {
            return Err("failed-candidate-already-preserved".into());
        }
        if let Some(child) = child.as_mut() {
            // Child is an owned process handle, never a journal PID reused after reboot.
            if child.try_wait().map_err(err)?.is_none() {
                child.kill().map_err(err)?;
                child.wait().map_err(err)?;
            }
        }
        require_only_pid(&journal.app, &[std::process::id()])?;
        rename(&journal.app, &journal.transaction.join("failed.app"))?;
    }
    if !current_old {
        if !previous.exists() {
            return Err("previous-app-unavailable".into());
        }
        rename(&previous, &journal.app)?;
    }
    package::check_app(
        &journal.app,
        &journal.previous_receipt,
        journal.development,
        journal.team.as_deref(),
    )?;
    require_only_pid(&journal.app, &[std::process::id()])?;
    let mut child = launch(journal, true)?;
    if let Err(error) = verify_runtime(journal, child.id(), true) {
        if child.try_wait().map_err(err)?.is_none() {
            child.kill().map_err(err)?;
            child.wait().map_err(err)?;
        }
        return Err(error);
    }
    save(journal, "rolled-back")
}

/// Invoked only by the previously verified installed Mach-O, before Tauri initializes.
pub fn run(app: &Path, expected_binding: &str) -> Result<()> {
    let mut journal = load(app)?;
    if binding(&journal)? != expected_binding {
        return Err("helper-journal-binding-mismatch".into());
    }
    let _lock = lock(&journal)?;
    let own = std::env::current_exe().map_err(err)?;
    let expected = package::executable(app)?;
    if own != expected {
        return Err("helper-is-not-installed-executable".into());
    }
    let parent = package::command(
        "/bin/ps",
        &[
            "-p".as_ref(),
            std::process::id().to_string().as_ref(),
            "-o".as_ref(),
            "ppid=".as_ref(),
        ],
    )?;
    if parent.trim().parse::<u32>().map_err(err)? != journal.parent_pid
        || process_executable(journal.parent_pid).as_deref() != Some(own.as_path())
    {
        return Err("helper-parent-identity-mismatch".into());
    }
    if !matches!(journal.phase.as_str(), "prepared" | "handshake") {
        return Err("helper-phase-mismatch".into());
    }
    if package::manifest(app)? != journal.previous_files {
        return Err("installed-app-changed".into());
    }
    package::check_app(
        &journal.transaction.join("pending.app"),
        &journal.receipt,
        journal.development,
        journal.team.as_deref(),
    )?;
    require_only_pid(app, &[journal.parent_pid, std::process::id()])?;
    journal.helper_pid = Some(std::process::id());
    save(&mut journal, "handshake")?;
    println!("READY");
    std::io::stdout().flush().map_err(err)?;
    // Parent keeps Admission closed and exits only after this trusted helper is READY.
    let deadline = Instant::now() + Duration::from_secs(30);
    while process_executable(journal.parent_pid).is_some() {
        if Instant::now() >= deadline {
            save(&mut journal, "aborted")?;
            return Err("old-app-exit-timeout-no-files-changed".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    require_only_pid(app, &[std::process::id()])?;
    let mut child: Option<Child> = None;
    let result = (|| {
        if !matches(app, &journal.previous_files)? {
            return Err("installed-app-changed".into());
        }
        rename(app, &journal.transaction.join("previous.app"))?;
        save(&mut journal, "old-saved")?;
        rename(&journal.transaction.join("pending.app"), app)?;
        save(&mut journal, "candidate-installed")?;
        child = Some(launch(&mut journal, false)?);
        verify_runtime(
            &journal,
            child.as_ref().ok_or("candidate-child-missing")?.id(),
            false,
        )?;
        save(&mut journal, "installed")
    })();
    if let Err(error) = result {
        journal.failure = Some(error);
        // Journal I/O failure must not bypass recovery or discard the owned Child.
        let _ = save(&mut journal, "failed");
        recover(&mut journal, child.as_mut())?;
    }
    Ok(())
}

pub fn binding(journal: &Journal) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(journal).map_err(err)?)
    ))
}

/// Startup recovery runs before acquiring the gateway singleton or accepting requests.
pub fn recover_at_startup(app: &Path) -> Result<bool> {
    // A crash between the two renames leaves the trusted previous bundle in the
    // transaction directory. Opening that bundle must restore the original path,
    // rather than silently hosting a gateway from its backup location.
    if app.file_name().is_some_and(|name| name == "previous.app") {
        let transaction = app.parent().ok_or("missing-transaction-parent")?;
        let bytes = package::read_bounded(&transaction.join("journal.json"), 16 * 1024 * 1024)?;
        let recorded: Journal = serde_json::from_slice(&bytes).map_err(err)?;
        let mut journal = load(&recorded.app)?;
        validate_backup_recovery(&journal, app)?;
        let _lock = lock(&journal)?;
        journal.helper_pid = Some(std::process::id());
        recover(&mut journal, None)?;
        return Ok(true);
    }
    if !journal_path(app)?.exists() {
        return Ok(false);
    }
    let mut journal = load(app)?;
    if matches!(
        journal.phase.as_str(),
        "installed" | "rolled-back" | "aborted" | "prepared"
    ) {
        return Ok(false);
    }
    let _lock = lock(&journal)?;
    if journal.phase == "handshake" && matches(app, &journal.previous_files)? {
        save(&mut journal, "aborted")?;
        return Ok(false);
    }
    journal.helper_pid = Some(std::process::id());
    recover(&mut journal, None)?;
    Ok(true)
}

fn validate_backup_recovery(journal: &Journal, backup: &Path) -> Result<()> {
    if backup != journal.transaction.join("previous.app")
        || package::manifest(backup)? != journal.previous_files
    {
        return Err("backup-launch-binding-mismatch".into());
    }
    // The first rename can be durable while its following checkpoint is not.
    // Missing original + exact old manifest proves that handshake moved forward.
    if journal.app.symlink_metadata().is_ok()
        || !matches!(
            journal.phase.as_str(),
            "handshake"
                | "old-saved"
                | "candidate-installed"
                | "starting"
                | "failed"
                | "rollback-starting"
        )
    {
        return Err("backup-launch-is-not-interrupted-recovery".into());
    }
    Ok(())
}

/// Candidate bootstrap may bypass startup recovery only while the trusted helper owns its parentage.
pub fn validate_owned_launch(app: &Path) -> Result<()> {
    let journal = load(app)?;
    let parent = package::command(
        "/bin/ps",
        &[
            "-p".as_ref(),
            std::process::id().to_string().as_ref(),
            "-o".as_ref(),
            "ppid=".as_ref(),
        ],
    )?;
    let helper_exe = process_executable(journal.helper_pid.ok_or("missing-helper-pid")?)
        .ok_or("helper-parent-exited")?;
    let mapped_previous = journal
        .transaction
        .join("previous.app/Contents/MacOS")
        .join(
            package::executable(app)?
                .file_name()
                .ok_or("missing-executable-name")?,
        );
    let mapped_failed = journal.transaction.join("failed.app/Contents/MacOS").join(
        package::executable(app)?
            .file_name()
            .ok_or("missing-executable-name")?,
    );
    if Some(parent.trim().parse::<u32>().map_err(err)?) != journal.helper_pid
        || (helper_exe != package::executable(app)?
            && helper_exe != mapped_previous
            && helper_exe != mapped_failed)
        || std::env::current_exe().map_err(err)? != package::executable(app)?
        || !matches!(
            journal.phase.as_str(),
            "candidate-installed" | "starting" | "rollback-starting" | "failed"
        )
    {
        return Err("unowned-candidate-launch".into());
    }
    let actual = package::manifest(app)?;
    if actual != journal.candidate_files && actual != journal.previous_files {
        return Err("launch-manifest-mismatch".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "app_update_helper_tests.rs"]
mod tests;

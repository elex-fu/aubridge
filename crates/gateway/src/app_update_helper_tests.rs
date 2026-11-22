use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 12];
        getrandom::fill(&mut random).unwrap();
        let path = std::env::temp_dir().join(format!(
            "aubridge-helper-test-{:x}",
            u128::from_le_bytes({
                let mut value = [0; 16];
                value[..12].copy_from_slice(&random);
                value
            })
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn receipt(files: Manifest) -> Receipt {
    Receipt {
        schema: 2,
        app: "aubridge.app".into(),
        version: "0.1.0".into(),
        target: format!("{}-apple-darwin", std::env::consts::ARCH),
        git_revision: "a".repeat(40),
        git_tree: "b".repeat(40),
        mode: "development".into(),
        profile: "release".into(),
        signing: "local-ad-hoc".into(),
        team_id: None,
        files,
        archive: package::Archive {
            name: "test.zip".into(),
            size: 1,
            sha256: "a".repeat(64),
        },
    }
}

fn journal(root: &Path) -> Journal {
    let app = root.join("aubridge.app");
    fs::create_dir(&app).unwrap();
    fs::write(app.join("original"), b"old").unwrap();
    let previous_files = package::manifest(&app).unwrap();
    let candidate = root.join("candidate.app");
    fs::create_dir(&candidate).unwrap();
    fs::write(candidate.join("new"), b"new").unwrap();
    let candidate_files = package::manifest(&candidate).unwrap();
    let transaction = root.join(".aubridge-upgrade-test");
    fs::create_dir(&transaction).unwrap();
    let data_dir = root.join("data");
    fs::create_dir(&data_dir).unwrap();
    Journal {
        schema: 1,
        app,
        data_dir,
        transaction,
        phase: "old-saved".into(),
        receipt: receipt(candidate_files.clone()),
        previous_receipt: receipt(previous_files.clone()),
        candidate_files,
        previous_files,
        parent_pid: std::process::id(),
        helper_pid: None,
        pid: None,
        development: true,
        team: None,
        failure: None,
    }
}

#[test]
fn app_update_recovery_does_not_overwrite_external_changes() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    fs::write(journal.app.join("original"), b"externally edited").unwrap();
    let backup = journal.transaction.join("previous.app");
    fs::create_dir(&backup).unwrap();
    fs::write(backup.join("original"), b"old").unwrap();
    assert_eq!(
        recover(&mut journal, None).unwrap_err(),
        "installed-files-changed-backup-preserved"
    );
    assert_eq!(
        fs::read(journal.app.join("original")).unwrap(),
        b"externally edited"
    );
    assert_eq!(fs::read(backup.join("original")).unwrap(), b"old");
}

#[test]
fn app_update_journal_cannot_change_install_target() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    save(&mut journal, "prepared").unwrap();
    assert!(load(&journal.app).is_ok());
    let other = root.0.join("other.app");
    journal.app = other;
    atomic_json(
        &journal_path(&root.0.join("aubridge.app")).unwrap(),
        &journal,
    )
    .unwrap();
    assert_eq!(
        load(&root.0.join("aubridge.app")).err().unwrap(),
        "journal-target-mismatch"
    );
}

#[test]
fn app_update_recovery_checks_backup_manifest_before_touching_installed_bundle() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    let backup = journal.transaction.join("previous.app");
    fs::create_dir(&backup).unwrap();
    fs::write(backup.join("original"), b"tampered backup").unwrap();
    assert_eq!(
        recover(&mut journal, None).unwrap_err(),
        "backup-manifest-mismatch"
    );
    assert_eq!(fs::read(journal.app.join("original")).unwrap(), b"old");
}

#[test]
fn app_update_recovery_preserves_only_candidate_when_backup_missing() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    fs::remove_file(journal.app.join("original")).unwrap();
    fs::write(journal.app.join("new"), b"new").unwrap();
    assert_eq!(
        recover(&mut journal, None).unwrap_err(),
        "previous-app-unavailable-current-preserved"
    );
    assert!(journal.app.join("new").exists());
    assert!(!journal.transaction.join("failed.app").exists());
}

#[test]
fn app_update_recovery_rejects_identical_manifests_without_moving_app() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    journal.candidate_files = journal.previous_files.clone();
    journal.receipt.files = journal.previous_files.clone();
    assert_eq!(
        recover(&mut journal, None).unwrap_err(),
        "ambiguous-identical-update-manifests"
    );
    assert!(journal.app.join("original").exists());
}

#[test]
fn app_update_legacy_python_journal_does_not_break_native_bootstrap() {
    let root = Fixture::new();
    let journal = journal(&root.0);
    let legacy = journal.app.with_file_name(".aubridge.app.upgrade.json");
    let raw = br#"{"schema":1,"phase":"starting","app":"historical-python-shape"}"#;
    fs::write(&legacy, raw).unwrap();
    assert!(!recover_at_startup(&journal.app).unwrap());
    assert_eq!(
        legacy_install_idle(&journal.app).unwrap_err(),
        "legacy-install-transaction-active"
    );
    assert_eq!(fs::read(&legacy).unwrap(), raw);
    fs::write(&legacy, br#"{"schema":1,"phase":"installed"}"#).unwrap();
    assert!(legacy_install_idle(&journal.app).is_ok());
    assert!(!recover_at_startup(&journal.app).unwrap());
}

#[test]
fn app_update_terminal_backup_cannot_trigger_silent_rollback() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    let backup = journal.transaction.join("previous.app");
    fs::create_dir(&backup).unwrap();
    fs::write(backup.join("original"), b"old").unwrap();
    save(&mut journal, "installed").unwrap();
    assert_eq!(
        recover_at_startup(&backup).unwrap_err(),
        "backup-launch-is-not-interrupted-recovery"
    );
    assert!(journal.app.join("original").exists());
    assert!(backup.join("original").exists());
}

#[test]
fn app_update_handshake_checkpoint_recovers_rename_before_phase_save() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    save(&mut journal, "handshake").unwrap();
    let backup = journal.transaction.join("previous.app");
    rename(&journal.app, &backup).unwrap();
    let durable = load(&journal.app).unwrap();
    assert_eq!(durable.phase, "handshake");
    assert!(validate_backup_recovery(&durable, &backup).is_ok());
    assert!(!journal.app.exists());
    assert_eq!(package::manifest(&backup).unwrap(), journal.previous_files);
    let mut terminal = durable;
    terminal.phase = "installed".into();
    assert_eq!(
        validate_backup_recovery(&terminal, &backup).unwrap_err(),
        "backup-launch-is-not-interrupted-recovery"
    );
}

#[test]
fn app_update_launch_intent_is_durable_before_child_can_read_it() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    journal.helper_pid = Some(123);
    journal.pid = Some(456);
    save(&mut journal, "old-saved").unwrap();
    record_launch_intent(&mut journal, true).unwrap();
    let child_view = load(&journal.app).unwrap();
    assert_eq!(child_view.helper_pid, Some(std::process::id()));
    assert_eq!(child_view.pid, None);
    assert_eq!(child_view.phase, "rollback-starting");
    record_launch_intent(&mut journal, false).unwrap();
    assert_eq!(load(&journal.app).unwrap().phase, "starting");
}

#[test]
fn app_update_launch_intent_failure_does_not_claim_a_child_pid() {
    let root = Fixture::new();
    let mut journal = journal(&root.0);
    journal.helper_pid = Some(123);
    journal.pid = Some(456);
    fs::create_dir(journal.transaction.join("journal.json")).unwrap();
    assert!(record_launch_intent(&mut journal, true).is_err());
    assert_eq!(journal.pid, None);
    assert!(!journal_path(&journal.app).unwrap().exists());
}

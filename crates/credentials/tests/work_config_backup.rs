use credentials::work_config_backup::{Archive, Group, Payload, Source, SourceKind, WiringIntent};

fn payload() -> Payload {
    Payload {
        sources: vec![Source {
            id: "private".into(),
            source: SourceKind::Custom,
            base: "https://tenant.example/v1".into(),
            auth: "bearer".into(),
            protocol: "chat".into(),
            kind: "free_tier".into(),
            models: vec!["m".into()],
        }],
        groups: vec![Group {
            name: "daily".into(),
            members: vec!["m".into()],
            rule: "sequential".into(),
        }],
        model_selections: Default::default(),
        paused_providers: vec!["private".into()],
        chatgpt_login_source: "codex".into(),
        client_wiring: vec![WiringIntent {
            agent: "codex".into(),
            enabled: true,
            model: "daily".into(),
        }],
        cost_intent: None,
    }
}

#[test]
fn corrupt_checksum_unknown_version_duplicate_keys_and_secret_fields_are_rejected() {
    let bytes = Archive::encode(payload()).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["sha256"] = serde_json::json!("0".repeat(64));
    assert!(Archive::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    value = serde_json::from_slice(&bytes).unwrap();
    value["version"] = serde_json::json!(99);
    assert!(Archive::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let text = String::from_utf8(bytes).unwrap();
    assert!(Archive::decode(
        text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1)
            .as_bytes()
    )
    .is_err());
    assert!(Archive::decode(
        text.replacen("\"payload\":{", "\"payload\":{\"key\":\"secret\",", 1)
            .as_bytes()
    )
    .is_err());
}

#[test]
fn userinfo_query_fragment_duplicate_ids_and_missing_dependencies_are_rejected() {
    for base in [
        "https://user:secret@tenant.example/v1",
        "https://tenant.example/v1?key=secret",
        "https://tenant.example/v1#secret",
    ] {
        let mut value = payload();
        value.sources[0].base = base.into();
        assert!(Archive::encode(value).is_err());
    }
    let mut value = payload();
    value.sources.push(value.sources[0].clone());
    assert!(Archive::encode(value).is_err());
    let mut value = payload();
    value.groups[0].members = vec!["missing".into()];
    assert!(Archive::encode(value).is_err());
}

#[test]
fn preview_cas_rejects_external_edit_and_undo_preserves_unrelated_fields() {
    use credentials::work_config_backup::{read_document, RestorePlan};
    let dir = std::env::temp_dir().join(format!("aubridge-restore-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"port":3427,"share_login":true,"future":17}"#).unwrap();
    let archive = Archive::decode(&Archive::encode(payload()).unwrap()).unwrap();
    let plan = RestorePlan::preview(&archive, &read_document(&config).unwrap(), &[]).unwrap();
    std::fs::write(
        &config,
        r#"{"port":3427,"share_login":true,"future":17,"providers":[]}"#,
    )
    .unwrap();
    assert!(plan.commit(&config).is_err());
    let plan = RestorePlan::preview(&archive, &read_document(&config).unwrap(), &[]).unwrap();
    let undo = plan.commit(&config).unwrap();
    let mut current = read_document(&config).unwrap();
    assert_eq!(current["share_login"], true);
    assert_eq!(current["port"], 3427);
    assert!(
        current["providers"].as_array().is_none_or(|p| p.is_empty()),
        "pending is not active"
    );
    assert!(current["pending_work_config_restore"].is_object());
    current["future"] = serde_json::json!(42);
    std::fs::write(&config, serde_json::to_vec(&current).unwrap()).unwrap();
    undo.apply(&config).unwrap();
    let current = read_document(&config).unwrap();
    assert_eq!(current["future"], 42);
    assert_eq!(current["share_login"], true);
    assert!(current.get("pending_work_config_restore").is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn same_id_different_full_endpoint_cannot_inherit_credentials() {
    use credentials::work_config_backup::RestorePlan;
    let archive = Archive::decode(&Archive::encode(payload()).unwrap()).unwrap();
    let target = serde_json::json!({"providers":[{"id":"private","base":"https://tenant.example/other-tenant","auth":"bearer","protocol":"chat","kind":"free_tier","models":["m"]}]});
    assert!(RestorePlan::preview(&archive, &target, &[]).is_err());
}

#[cfg(unix)]
#[test]
fn external_export_rejects_symlink_into_app_dir_and_never_overwrites() {
    use credentials::work_config_backup::export;
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = std::env::temp_dir().join(format!("aubridge-export-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let external = root.join("external");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    symlink(&app, external.join("inside")).unwrap();
    let bytes = Archive::encode(payload()).unwrap();
    assert!(export(&external.join("inside/backup.json"), &app, &bytes).is_err());
    let destination = external.join("backup.json");
    export(&destination, &app, &bytes).unwrap();
    assert_eq!(
        std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(export(&destination, &app, &bytes).is_err());
    assert_eq!(std::fs::read(destination).unwrap(), bytes);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn oversized_and_nonregular_archive_are_rejected_before_reading() {
    use credentials::work_config_backup::read_archive_file;
    let dir = std::env::temp_dir().join(format!("aubridge-backup-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(read_archive_file(&dir).is_err());
    let path = dir.join("large.json");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(4 * 1024 * 1024 + 1).unwrap();
    assert!(read_archive_file(&path).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn undo_refuses_an_external_edit_to_affected_fields() {
    use credentials::work_config_backup::{read_document, RestorePlan};
    let dir = std::env::temp_dir().join(format!("aubridge-backup-undo-cas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"future":17}"#).unwrap();
    let archive = Archive::decode(&Archive::encode(payload()).unwrap()).unwrap();
    let undo = RestorePlan::preview(&archive, &read_document(&config).unwrap(), &[])
        .unwrap()
        .commit(&config)
        .unwrap();
    let mut changed = read_document(&config).unwrap();
    changed["pending_work_config_restore"]["archive_digest"] = serde_json::json!("external-change");
    let encoded = serde_json::to_vec(&changed).unwrap();
    std::fs::write(&config, &encoded).unwrap();
    assert!(undo.apply(&config).is_err());
    assert_eq!(std::fs::read(&config).unwrap(), encoded);
    std::fs::remove_dir_all(dir).unwrap();
}

fn policy_archive(policy: Option<serde_json::Value>) -> Vec<u8> {
    let mut canonical = serde_json::to_string(&payload()).unwrap();
    if let Some(policy) = policy {
        canonical.pop();
        canonical.push_str(&format!(",\"cost_intent\":{policy}}}"));
    }
    let checksum = credentials::work_config_backup::digest(canonical.as_bytes());
    format!("{{\"format\":\"aubridge-work-config\",\"version\":1,\"payload\":{canonical},\"sha256\":\"{checksum}\"}}").into_bytes()
}
#[test]
fn v1_legacy_checksum_is_unchanged_when_cost_intent_is_absent() {
    let input = policy_archive(None);
    let archive = Archive::decode(&input).unwrap();
    assert_eq!(serde_json::to_vec(&archive).unwrap(), input);
    assert!(serde_json::to_value(archive.payload)
        .unwrap()
        .get("cost_intent")
        .is_none());
}
#[test]
fn explicit_cost_intent_is_versioned_and_invalid_values_are_rejected() {
    for policy in ["confirmed_free_only", "free_then_subscription"] {
        let input = policy_archive(Some(serde_json::json!(policy)));
        let archive = Archive::decode(&input).unwrap();
        assert_eq!(
            serde_json::to_value(archive.payload).unwrap()["cost_intent"],
            policy
        );
    }
    for value in [
        serde_json::json!("unknown"),
        serde_json::json!(17),
        serde_json::json!({}),
    ] {
        assert!(Archive::decode(&policy_archive(Some(value))).is_err());
    }
}

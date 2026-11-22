use super::*;

#[test]
fn update_archive_rejects_aliases_traversal_and_non_bundle_paths() {
    for name in [
        "/aubridge.app/a",
        "aubridge.app/../x",
        "aubridge.app/a\\b",
        "other.app/a",
        "aubridge.app/é",
    ] {
        assert!(safe_name(name).is_err(), "accepted {name}");
    }
    let mut names = BTreeSet::new();
    assert!(unique_name("aubridge.app/Contents/A", &mut names).is_ok());
    assert!(unique_name("aubridge.app/contents/a", &mut names).is_err());
}

#[test]
fn update_links_cannot_escape_or_cross_archive_parents() {
    assert!(safe_link("aubridge.app/Contents/a", "../../escape").is_err());
    assert!(safe_link("aubridge.app/Contents/a", "/etc/passwd").is_err());
    assert!(safe_link("aubridge.app/Contents/a", "Resources/file").is_ok());
}

#[test]
fn update_receipt_cannot_self_authorize_developer_identity() {
    let value = serde_json::json!({"schema":2,"app":"aubridge.app","version":"0.1.1","target":format!("{}-apple-darwin",std::env::consts::ARCH),"git_revision":"a".repeat(40),"git_tree":"b".repeat(40),"mode":"distribution","profile":"release","signing":"developer-id-notarized","team_id":"ATTACKTEAM","files":{"Contents/file":{"mode":420,"sha256":"a".repeat(64)}},"archive":{"name":"a.zip","size":1,"sha256":"a".repeat(64)}});
    let receipt: Receipt = serde_json::from_value(value).unwrap();
    assert_eq!(
        receipt.validate(false, None).unwrap_err(),
        "trusted-developer-team-required"
    );
    assert_eq!(
        receipt.validate(false, Some("TRUSTEDTEAM")).unwrap_err(),
        "trusted-developer-team-required"
    );
}

#[test]
fn update_archive_crc_failure_is_rejected_before_native_extraction() {
    use std::io::{Seek, SeekFrom, Write};
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).unwrap();
    let path = std::env::temp_dir().join(format!(
        "aubridge-update-zip-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    writer
        .start_file(
            "aubridge.app/Contents/test",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(b"original payload").unwrap();
    writer.finish().unwrap();
    let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let offset = archive.by_index(0).unwrap().data_start().unwrap();
    drop(archive);
    let mut output = fs::OpenOptions::new().write(true).open(&path).unwrap();
    output.seek(SeekFrom::Start(offset)).unwrap();
    output.write_all(b"X").unwrap();
    drop(output);
    assert!(preflight_zip(&path).is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn update_local_inputs_reject_directories_and_bound_read_allocation() {
    assert_eq!(
        read_bounded(&std::env::temp_dir(), 10).unwrap_err(),
        "update-input-not-regular-file"
    );
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).unwrap();
    let path = std::env::temp_dir().join(format!(
        "aubridge-update-read-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    fs::write(&path, b"oversized input").unwrap();
    assert_eq!(
        read_bounded(&path, 3).unwrap_err(),
        "update-input-size-limit"
    );
    fs::remove_file(path).unwrap();
}

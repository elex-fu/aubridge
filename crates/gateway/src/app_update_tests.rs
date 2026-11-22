use super::*;

#[test]
fn update_feed_signature_is_checked_before_payload_parse() {
    let envelope =
        serde_json::to_vec(&serde_json::json!({"payload":"not json","signature":"00".repeat(64)}))
            .unwrap();
    assert_eq!(
        verify_envelope(&envelope, &[0; 32], 1).err().unwrap(),
        "feed-signature-invalid"
    );
}

#[test]
fn update_feed_rejects_tampered_payload_and_expired_authentic_offer() {
    use ring::signature::KeyPair;
    let key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
    let payload=serde_json::json!({"channel":"stable","platform":"macos","arch":"aarch64","version":"0.1.1","expires_unix":50,"receipt_path":"receipt.json","receipt_sha256":"a".repeat(64),"archive_path":"a.zip","size":1,"sha256":"b".repeat(64)}).to_string();
    let signature = key
        .sign(payload.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let envelope =
        serde_json::to_vec(&serde_json::json!({"payload":payload,"signature":signature})).unwrap();
    let public: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    assert!(verify_envelope(&envelope, &public, 49).is_ok());
    assert_eq!(
        verify_envelope(&envelope, &public, 50).err().unwrap(),
        "feed-expired"
    );
    let tampered = serde_json::to_vec(
        &serde_json::json!({"payload":payload.replace("0.1.1","0.1.2"),"signature":signature}),
    )
    .unwrap();
    assert_eq!(
        verify_envelope(&tampered, &public, 49).err().unwrap(),
        "feed-signature-invalid"
    );
}

#[test]
fn update_feed_never_follows_another_origin_or_plain_http() {
    assert!(same_origin(
        "https://updates.example.com/stable/feed.json",
        "../receipt.json"
    )
    .is_ok());
    assert!(same_origin(
        "https://updates.example.com/stable/feed.json",
        "https://evil.example.org/a.zip"
    )
    .is_err());
    assert!(same_origin(
        "https://updates.example.com/stable/feed.json",
        "http://updates.example.com/a.zip"
    )
    .is_err());
    assert!(same_origin(
        "https://updates.example.com/stable/feed.json",
        "https://user@updates.example.com/a.zip"
    )
    .is_err());
}

#[test]
fn update_version_requires_strict_semver_increase() {
    assert!(!is_newer("0.1.0", "0.1.0+another-commit").unwrap());
    assert!(!is_newer("0.1.0", "0.1.0-rc.1").unwrap());
    assert!(is_newer("0.1.0", "0.1.1").unwrap());
    assert!(is_newer("0.1.0", "v0.2").is_err());
}

#[test]
fn update_unconfigured_feed_is_not_current() {
    let service = UpdateService::new();
    assert_eq!(service.status().state, "not_configured");
}

#[test]
fn update_confirmation_cannot_accept_unbound_handle() {
    let service = UpdateService::new();
    assert!(service.prepared("invented", false).is_err());
    assert!(service.prepared("invented", true).is_err());
}

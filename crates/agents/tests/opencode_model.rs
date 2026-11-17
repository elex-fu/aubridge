//! Exact JSONC restoration through the public OpenCode wiring API.
use agents::opencode::OpenCode;

#[test]
fn opencode_model_restores_original_escaped_jsonc_literal() {
    let root = std::env::temp_dir().join(format!(
        "aubridge-opencode-original-literal-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("opencode.jsonc");
    let state = root.join("state");
    let literal = r#""foreign\/\u6a21\u578b""#;
    let original = format!(
        "{{\n  // preserve original model spelling\n  \"model\": {literal},\n  \"small_model\": \"foreign/small\",\n  \"theme\": \"dark\",\n}}\n"
    );
    std::fs::write(&config, &original).unwrap();
    let client = OpenCode::new(config.clone(), state.clone(), 3427);
    client.apply().unwrap();
    client.set_model("vendor/new-model").unwrap();
    assert_eq!(client.current_model().as_deref(), Some("vendor/new-model"));
    let updated = OpenCode::new(config.clone(), state, 6543)
        .with_models(vec![("vendor/new-model".into(), "New model".into())]);
    updated.repair().unwrap();
    assert_eq!(updated.current_model().as_deref(), Some("vendor/new-model"));
    let preview = updated.disconnect_preview().unwrap();
    assert!(!preview.blocked);
    updated.disconnect_reviewed(Some(&preview)).unwrap();
    let restored = std::fs::read_to_string(&config).unwrap();
    assert!(restored.contains(&format!("\"model\": {literal}")));
    assert!(restored.contains("// preserve original model spelling"));
    assert!(restored.contains("\"small_model\": \"foreign/small\""));
    assert!(restored.contains("\"theme\": \"dark\""));
    assert!(!restored.contains("aubridge/vendor/new-model"));
    std::fs::remove_dir_all(root).unwrap();
}

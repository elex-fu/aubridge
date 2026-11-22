#![cfg(unix)]

use gateway::{app, state::GatewayState};
use serde_json::{json, Value};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc};

#[tokio::test]
async fn opencode_probe_is_wired_isolated_and_checks_model_output() {
    let root = std::env::temp_dir().join(format!("aubridge-opencode-probe-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("opencode");
    std::fs::write(
        &script,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'opencode v2.2.5\n'; exit 0; fi
test "$1" = "--standalone" || exit 20
test "$2" = "run" || exit 21
test "$3" = "--format" || exit 22
test "$4" = "json" || exit 23
test "$5" = "--model" || exit 24
test "$6" = "aubridge/aubridge" || exit 25
test -f "$OPENCODE_CONFIG" || exit 26
test "$XDG_DATA_HOME" != "$HOME/.local/share" || exit 27
printf '{"type":"text","part":{"text":"收到"}}\n'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let previous = std::env::var_os("AUBRIDGE_OPENCODE_BIN");
    std::env::set_var("AUBRIDGE_OPENCODE_BIN", &script);
    let paths = agents::AgentsPaths {
        claude_settings: root.join("claude/settings.json"),
        codex_config: root.join("codex/config.toml"),
        opencode_config: root.join("config/opencode.json"),
        gemini_settings: root.join("gemini/settings.json"),
        gemini_env: root.join("gemini/.env"),
        state_dir: root.join("agents"),
    };
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "test")
            .with_admin_token("probe-admin".into())
            .with_agents(paths),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let probe = format!("{endpoint}/admin/agents/opencode/test");
    let missing = client
        .post(&probe)
        .bearer_auth("probe-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
    assert!(missing.text().await.unwrap().contains("未接线"));
    let on = client
        .post(format!("{endpoint}/admin/agents/opencode/enable"))
        .bearer_auth("probe-admin")
        .json(&json!({"on":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(on.status(), 200);
    let result = client
        .post(&probe)
        .bearer_auth("probe-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    let body: Value = result.json().await.unwrap();
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["exit_code"], 0);
    std::fs::write(&script,"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '2.2.5'; else printf '{\"type\":\"error\",\"error\":\"收到 an error\"}'; fi\n").unwrap();
    let failed: Value = client
        .post(&probe)
        .bearer_auth("probe-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(failed["ok"], false, "{failed}");
    std::fs::write(
        &script,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf '2.2.5'; exit 0; fi
printf '{"type":"text","part":{"text":"收到"}}\n'
printf '%20000s' ' '
printf '\n{"type":"error","error":"late failure"}\n'
"#,
    )
    .unwrap();
    let late_error: Value = client
        .post(&probe)
        .bearer_auth("probe-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(late_error["ok"], false, "{late_error}");
    let probe_dirs = || -> std::collections::BTreeSet<_> {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("aubridge-agent-test-{}-", std::process::id()))
            })
            .map(|entry| entry.path())
            .collect()
    };
    let before = probe_dirs();
    std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    let version_failure = client
        .post(&probe)
        .bearer_auth("probe-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(version_failure.status(), 400);
    assert_eq!(probe_dirs(), before);
    match previous {
        Some(value) => std::env::set_var("AUBRIDGE_OPENCODE_BIN", value),
        None => std::env::remove_var("AUBRIDGE_OPENCODE_BIN"),
    };
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

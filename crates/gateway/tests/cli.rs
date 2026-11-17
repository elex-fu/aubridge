//! 诊断 CLI 集成测试(变更记录 34):真回环网关 + 回放上游,
//! 验证 `provider test` 的中继链路与会话文件消费。

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::json;
use support::replay::{spawn, Step};

const CHAT_TOOL_CALL: &str = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");

async fn cli(dir: &std::path::Path, args: &[&str]) -> i32 {
    let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    gateway::cli::run_with_dir(&args, dir).await
}

#[tokio::test]
async fn provider_test_relays_to_running_gateway() {
    // 上游:成功回放一条 chat SSE(check_provider 走真实 send 链路)。
    let (_replay, upstream, _task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;
    let manifest: Manifest = serde_json::from_value(json!({
        "id": "prov-a", "version": "1",
        "provider": { "base": upstream, "auth": "bearer" },
        "free_models": ["glm-4.5-flash"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("manifest json");
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("prov-a".to_string(), manifest)]),
            "aubridge",
        )
        .with_admin_token("test-admin".into())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = gateway::app(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // 会话文件:与运行中网关一致 {endpoint, admin_token}。
    let dir = std::env::temp_dir().join(format!("aubridge-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("gateway-session.json"),
        json!({"endpoint": format!("http://{addr}"), "admin_token": "test-admin"}).to_string(),
    )
    .unwrap();

    // 成功路径:上游回放 SSE → check ok → 退出码 0。
    assert_eq!(cli(&dir, &["provider", "test", "prov-a"]).await, 0);
    // 未知 provider:404 → 退出码 1。
    assert_eq!(cli(&dir, &["provider", "test", "nope"]).await, 1);
    // status / providers 也走同一条中继链路。
    assert_eq!(cli(&dir, &["status"]).await, 0);
    assert_eq!(cli(&dir, &["providers"]).await, 0);

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn provider_test_reports_upstream_failure_as_nonzero() {
    // 上游 429:check 非 ok → 退出码 1(诊断语义与 magpie provider
    // test 对齐:只有真通才算 0)。
    let (_replay, upstream, _task) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let manifest: Manifest = serde_json::from_value(json!({
        "id": "prov-b", "version": "1",
        "provider": { "base": upstream, "auth": "bearer" },
        "free_models": ["glm-4.5-flash"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("manifest json");
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("prov-b".to_string(), manifest)]),
            "aubridge",
        )
        .with_admin_token("test-admin".into())
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = gateway::app(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = std::env::temp_dir().join(format!("aubridge-cli-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("gateway-session.json"),
        json!({"endpoint": format!("http://{addr}"), "admin_token": "test-admin"}).to_string(),
    )
    .unwrap();

    assert_eq!(cli(&dir, &["provider", "test", "prov-b"]).await, 1);

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

//! gateway 集成测试:候选循环 failover、429 状态转移、SSE 翻译回环
//! 全链路(经回放服务器)。
//!
//! 语料:复用 translate 回归集的 fixture(include_str 相对路径跨
//! crate 引用——crate-layout §4.5 已裁定砍掉 fixtures crate,不因
//! 第二消费者建 crate;路径断在编译期,不会静默)。

mod support;

use std::collections::BTreeMap;

use gateway::{app, state::GatewayState};
use manifest::schema::Manifest;
use support::replay::{spawn, spawn_with_refresh, Step};

const CHAT_TOOL_CALL: &str = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");

/// 两源:A(先失败 429 quota)→ B(成功回放)。
fn two_provider_state(a: &str, b: &str, token: &str) -> std::sync::Arc<GatewayState> {
    let mk = |id: &str, base: &str| -> (String, Manifest) {
        (
            id.to_string(),
            serde_json::from_value(serde_json::json!({
                "id": id, "version": "1",
                "provider": { "base": base, "auth": "bearer" },
                "free_models": ["glm-4.5-flash"],
                "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("preset json"),
        )
    };
    let manifests = BTreeMap::from([mk("prov-a", a), mk("prov-b", b)]);
    std::sync::Arc::new(GatewayState::from_manifests(manifests, token))
}

#[tokio::test]
async fn failover_on_429_then_success_via_second_provider() {
    // A 回放 429 quota_exceeded;B 回放成功 SSE。
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (_rb, b_url, _b) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;

    let state = two_provider_state(&a_url, &b_url, "aubridge");
    let app = app(state.clone());

    // 两个源都在快照序里(A 在前),候选循环应先试 A、429 后切 B。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":100,
                "messages":[{"role":"user","content":"weather?"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    // 应答者是 B(failover 生效,X-Aubridge-Provider 标明)。
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_start"));
    assert!(body.contains("input_json_delta"));
    assert!(body.contains("message_stop"));

    // 状态转移:A 被 rest + sink(真没量)。
    let a_state = state
        .snapshot()
        .into_iter()
        .find(|s| s.id == "prov-a")
        .expect("prov-a state");
    assert!(a_state.sunk, "quota_exceeded 429 应 sink");
    assert!(a_state.rest_until.is_some(), "应 rest 到重置时刻");

    server.abort();
}

/// 组合模型 group/a+b(变更记录 31 批 2 ④):A 供 ma 失败 → B 供 mb
/// 成功;上游收到的 model 字段按候选源改写成其字面供应的成员。
#[tokio::test]
async fn group_model_fails_over_and_rewrites_model_per_candidate() {
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (rb, b_url, _rb) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;

    // 每源只供一个成员模型(字面匹配)。
    let mk = |id: &str, base: &str, model: &str| -> (String, Manifest) {
        (
            id.to_string(),
            serde_json::from_value(serde_json::json!({
                "id": id, "version": "1",
                "provider": { "base": base, "auth": "bearer" },
                "free_models": [model],
                "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("preset json"),
        )
    };
    let manifests = BTreeMap::from([mk("prov-a", &a_url, "m-a"), mk("prov-b", &b_url, "m-b")]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"group/m-a+m-b","max_tokens":100,
                "messages":[{"role":"user","content":"weather?"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    // 应答者是 B(声明序:A 的候选在前,429 后切 B)。
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_stop"));

    // 上游收到的 model 字段按源改写:A 收 m-a,B 收 m-b(不是
    // group 语法原文——上游只认字面 model)。
    let seen = rb.seen_bodies.lock().await;
    assert_eq!(seen.len(), 1, "B 只收到一次: {seen:?}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&seen[0]).expect("b body json")["model"] == "m-b",
        "B 应收到成员字面模型: {}",
        seen[0]
    );

    server.abort();
}

#[tokio::test]
async fn all_providers_failing_yields_error_termination() {
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (_rb, b_url, _b) = spawn(vec![Step::Fail {
        status: 503,
        body: r#"{"error":"unavailable"}"#.into(),
    }])
    .await;

    let state = two_provider_state(&a_url, &b_url, "aubridge");
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .header("x-api-key", "aubridge")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    let body = resp.text().await.unwrap();
    // F-5 语义:无候选成功时,以 error 终止事件收尾,不裸断流。
    assert!(body.contains("no candidate provider succeeded"));
    assert!(body.contains("message_stop"));

    server.abort();
}

#[tokio::test]
async fn missing_key_rejected() {
    let state = two_provider_state("http://127.0.0.1:1", "http://127.0.0.1:2", "aubridge");
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .body(r#"{"model":"glm-4.5-flash","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 401);

    // agent 前缀 key 合法(`aubridge-<agent>`)。
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .header("authorization", "Bearer aubridge-claude-code")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":1,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    server.abort();
}

#[tokio::test]
async fn dashboard_requires_admin_key_and_reports_savings() {
    use gateway::state::GatewayState;
    use ledger::{Accounting, Ledger, UsageRecord};

    let l = Ledger::open(":memory:").unwrap();
    l.record(&UsageRecord {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        agent: "claude-code".into(),
        provider: "zhipu".into(),
        model: "glm-4.5-flash".into(),
        price_snapshot: ledger::PriceSnapshot::builtin("glm-4.5-flash"),
        input_tokens: Some(1_000_000),
        output_tokens: Some(0),
        accounting: Accounting::Reported,
        ..UsageRecord::default()
    })
    .unwrap();

    let (_ra, a_url, _a) = spawn(vec![Step::Sse {
        body: CANNED_MIN.into(),
    }])
    .await;
    let manifests = BTreeMap::from([(
        "prov-a".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "prov-a", "version": "1",
            "provider": { "base": a_url, "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state =
        std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge").with_ledger(l));
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    // 推理 key 不能进 admin 面(§6:两个 key 空间隔离)。
    let resp = client
        .get(format!("http://{gw}/admin/dashboard"))
        .header("authorization", "Bearer aubridge")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // admin key:省 $X + 功臣榜 + 分享卡片(口径可见)。
    let resp = client
        .get(format!("http://{gw}/admin/dashboard"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!((v["monthly_savings_usd"].as_f64().unwrap() - 0.11).abs() < 1e-9);
    assert_eq!(v["top_providers"][0]["provider"], "zhipu");
    let card = v["share_card"].as_str().unwrap();
    assert!(card.contains("API 等值估算 $0.11"));
    assert!(card.contains("对标口径"));

    server.abort();
}

/// 两源 manifest 构造(决策日志测试复用)。
fn two_provider_manifests(a: &str, b: &str) -> BTreeMap<String, Manifest> {
    let mk = |id: &str, base: &str| -> (String, Manifest) {
        (
            id.to_string(),
            serde_json::from_value(serde_json::json!({
                "id": id, "version": "1",
                "provider": { "base": base, "auth": "bearer" },
                "free_models": ["glm-4.5-flash"],
                "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("preset json"),
        )
    };
    BTreeMap::from([mk("prov-a", a), mk("prov-b", b)])
}

/// 请求决策日志(变更记录 31 批 2 ⑤):429→成功 failover 一条请求
/// 一行,尝试序列/agent 前缀/结局经 /admin/dashboard 可见。
#[tokio::test]
async fn decision_log_records_failover_attempts_and_outcome() {
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (_rb, b_url, _b) = spawn(vec![Step::Sse {
        body: include_str!("../../translate/tests/fixtures/chat_tool_call.sse").into(),
    }])
    .await;

    let l = ledger::Ledger::open(":memory:").unwrap();
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(two_provider_manifests(&a_url, &b_url), "aubridge")
            .with_ledger(l),
    );
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw}/v1/messages"))
        // agent 前缀 key:决策行的 agent 字段取 `aubridge-` 之后。
        .header("authorization", "Bearer aubridge-claude-code")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    assert!(resp.text().await.unwrap().contains("message_stop"));

    let resp = client
        .get(format!("http://{gw}/admin/dashboard"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    let row = &v["recent_decisions"][0];
    assert_eq!(row["model"], "glm-4.5-flash");
    assert_eq!(row["agent"], "claude-code");
    assert_eq!(row["provider"], "prov-b");
    assert_eq!(row["outcome"], "ok");
    // 尝试序列:A 预检失败 429 → B 成功 200。
    let attempts = row["attempts"].as_array().expect("attempts 数组");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["id"], "prov-a");
    assert_eq!(attempts[0]["status"], 429);
    assert_eq!(attempts[1]["id"], "prov-b");
    assert_eq!(attempts[1]["status"], 200);

    server.abort();
}

const CANNED_MIN: &str = "data: [DONE]\n\n";

/// kimi-code(Anthropic 原生上游):同协议字节透传,零翻译。
#[tokio::test]
async fn kimi_code_anthropic_passthrough() {
    // 假 Anthropic 上游:回放真实形状的 Anthropic SSE。
    let anthropic_sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kimi_1\",\"model\":\"kimi-k3\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"kimi passthrough ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (replay, up_url, _u) = spawn(vec![Step::Sse {
        body: anthropic_sse.into(),
    }])
    .await;

    let manifests = BTreeMap::from([(
        "kimi-code".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "kimi-code", "version": "1", "kind": "subscription",
            "provider": { "base": up_url, "auth": "api-key", "protocol": "anthropic" },
            "free_models": ["kimi-k3"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state = std::sync::Arc::new(GatewayState::with_keys(
        manifests,
        "aubridge",
        BTreeMap::from([("kimi-code".to_string(), "fixture-kimi-key".to_string())]),
    ));
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge-claude-code")
        .header("anthropic-beta", "claude-code-20250219")
        .body(
            r#"{"model":"kimi-k3","max_tokens":64,
            "thinking":{"type":"adaptive","display":"summarized"},
            "output_config":{"effort":"high","future_field":true},
            "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-aubridge-provider").unwrap(),
        "kimi-code"
    );
    let body = resp.text().await.unwrap();
    // 透传断言:上游事件原样到达(含 event: 行与原始 JSON 字节)。
    assert!(body.contains("event: message_start"));
    assert!(body.contains("msg_kimi_1"));
    assert!(body.contains("kimi passthrough ok"));
    assert!(body.contains("event: message_stop"));
    // 不经过翻译层:字段形状保持上游原样(model=kimi-k3 而非请求模型重写)。
    assert!(body.contains("\"model\":\"kimi-k3\""));

    // 协议头透传:客户端 anthropic-beta 原样转发;客户端未带
    // anthropic-version 时注入 2023-06-01(model-router 同款缺省)。
    let seen = replay.seen_headers.lock().await;
    let hdrs = seen.first().expect("upstream saw a request");
    assert_eq!(hdrs.get("anthropic-beta").unwrap(), "claude-code-20250219");
    assert_eq!(hdrs.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(hdrs["x-api-key"], "fixture-kimi-key");
    let sent: serde_json::Value =
        serde_json::from_str(&replay.seen_bodies.lock().await[0]).unwrap();
    assert_eq!(
        sent["thinking"],
        serde_json::json!({"type":"adaptive","display":"summarized"})
    );
    assert_eq!(
        sent["output_config"],
        serde_json::json!({"effort":"high","future_field":true})
    );

    server.abort();
}

/// 仪表盘携带源状态快照(§8.3 数据源)。
#[tokio::test]
async fn dashboard_includes_provider_snapshot() {
    let manifests = BTreeMap::from([(
        "smoke-replay".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "smoke-replay", "version": "1",
            "provider": { "base": "http://127.0.0.1:1", "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let resp = reqwest::Client::new()
        .get(format!("http://{gw}/admin/dashboard"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .expect("dashboard");
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    let providers = v["providers"].as_array().expect("providers snapshot");
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "smoke-replay");
    assert_eq!(providers[0]["kind"], "free_tier");
    assert_eq!(providers[0]["healthy"], true);

    server.abort();
}

/// 插件页(§9):扫描 + 默认全关 + 运行时开关 + 校验失败报告。
#[tokio::test]
async fn plugins_admin_scan_toggle_and_invalid() {
    // 插件目录:一个合法插件 + 一个非法文件。
    let dir = std::env::temp_dir().join(format!("aubridge-it-plugins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("kenari.json"),
        r#"{
            "id": "kenari-free", "version": "0.3.1",
            "provider": {"base": "https://api.kenari.dev/v1", "auth": "api-key"},
            "free_models": ["deepseek-v4-flash:free"],
            "permissions": {"net": ["api.kenari.dev"], "store_key": true}
        }"#,
    )
    .unwrap();
    std::fs::write(dir.join("broken.json"), r#"{"id": "BAD"}"#).unwrap();

    let state = std::sync::Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge").with_plugins(dir.clone()),
    );
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let admin = ("authorization", "Bearer aubridge-admin");

    // 扫描:合法插件在列、默认关;非法文件报错误。
    let resp = client
        .get(format!("http://{gw}/admin/plugins"))
        .header(admin.0, admin.1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(v["plugins"][0]["id"], "kenari-free");
    assert_eq!(v["plugins"][0]["enabled"], false, "默认全关(§9)");
    assert!(v["invalid"]["broken.json"].is_array());
    // 默认关:模型列表里没有它。
    assert_eq!(state.models(), vec![gateway::AUTO_MODEL.to_string()]);

    // 非 admin key 被拒。
    let resp = client
        .get(format!("http://{gw}/admin/plugins"))
        .header("authorization", "Bearer aubridge")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 开启:进模型列表;再关闭:移出。
    let resp = client
        .post(format!("http://{gw}/admin/plugins/kenari-free/enable"))
        .header(admin.0, admin.1)
        .json(&serde_json::json!({"on": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        state.models(),
        vec![
            gateway::AUTO_MODEL.to_string(),
            "deepseek-v4-flash:free".to_string()
        ]
    );
    // 持久化:enabled.json 落盘(新格式 {auto, on})。
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("enabled.json")).unwrap()).unwrap();
    assert_eq!(persisted["on"]["kenari-free"], true);
    assert_eq!(persisted["auto"], false);

    // 自动开启(§9 独立设置,默认关):auto on → 全部合法插件进候选。
    let resp = client
        .post(format!("http://{gw}/admin/plugins/auto"))
        .header(admin.0, admin.1)
        .json(&serde_json::json!({"on": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        state.models(),
        vec![
            gateway::AUTO_MODEL.to_string(),
            "deepseek-v4-flash:free".to_string()
        ]
    );
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("enabled.json")).unwrap()).unwrap();
    assert_eq!(persisted["auto"], true);
    // auto 不写逐个记录;早前单独开启的记录保留。
    assert_eq!(persisted["on"]["kenari-free"], true);

    let resp = client
        .post(format!("http://{gw}/admin/plugins/kenari-free/enable"))
        .header(admin.0, admin.1)
        .json(&serde_json::json!({"on": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(state.models(), vec![gateway::AUTO_MODEL.to_string()]);
    // 显式关闭记 false:auto 模式下重启也不复活(§9)。
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("enabled.json")).unwrap()).unwrap();
    assert_eq!(persisted["on"]["kenari-free"], false);

    // 不存在的插件:400。
    let resp = client
        .post(format!("http://{gw}/admin/plugins/nope/enable"))
        .header(admin.0, admin.1)
        .json(&serde_json::json!({"on": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 端口回退(§4):首选端口被占用时绑定随机端口,on_bound 收到实际值。
#[tokio::test]
async fn serve_falls_back_when_port_in_use() {
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = blocker.local_addr().unwrap().port();

    let state = std::sync::Arc::new(GatewayState::from_manifests(BTreeMap::new(), "aubridge"));
    let (tx, rx) = tokio::sync::oneshot::channel::<u16>();
    let task = tokio::spawn(async move {
        gateway::serve(state, taken, move |actual| {
            let _ = tx.send(actual);
        })
        .await
    });

    let actual = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("on_bound fired")
        .expect("port received");
    assert_ne!(actual, taken, "回退后不应再是被占用的端口");
    assert!(actual > 0);
    task.abort();
}

// ---------------------------------------------------------------------------
// chatgpt(Responses 协议)通道:§6.3 头全集 + Responses→Anthropic 流式
// 翻译 + 401 刷新一次重试(§7)。语料复用 F-12 的真实 codex fixture。
// ---------------------------------------------------------------------------

const CODEX_TEXT_SSE: &str = include_str!("../../translate/tests/fixtures/codex_text.sse");

/// Responses 协议单源 manifest(chatgpt 通道形态,与 registry preset 同构)。
fn responses_manifest(base: &str) -> (String, Manifest) {
    (
        "chatgpt".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "chatgpt", "version": "1", "kind": "subscription",
            "provider": { "base": base, "auth": "bearer", "protocol": "responses" },
            "free_models": ["gpt-6.1-sol"],
            "quota": { "source": "usage_api", "rest_hint": "5h-window" },
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("preset json"),
    )
}

fn tmp_codex_auth(tag: &str, json: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aubridge-gw-codex-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("auth.json");
    std::fs::write(&p, json).unwrap();
    p
}

async fn spawn_gateway(
    state: std::sync::Arc<GatewayState>,
) -> (reqwest::Client, String, tokio::task::JoinHandle<()>) {
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // F-0097 harness:system-proxy 特性启用后测试 client 会继承系统代理,
    // host 改写用例被代理劫持返回 502;测试网络必须直连。
    (
        reqwest::Client::builder().no_proxy().build().unwrap(),
        format!("http://{addr}"),
        server,
    )
}

async fn post_message(client: &reqwest::Client, gw: &str) -> reqwest::Response {
    client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"gpt-6.1-sol","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request")
}

#[tokio::test]
async fn codex_login_changes_take_effect_without_gateway_restart() {
    let initial =
        r#"{"tokens":{"access_token":"initial","refresh_token":"r1","account_id":"first"}}"#;
    let switched =
        r#"{"tokens":{"access_token":"initial","refresh_token":"r2","account_id":"second"}}"#;
    let relogin =
        r#"{"tokens":{"access_token":"relogin","refresh_token":"r3","account_id":"third"}}"#;
    let auth = tmp_codex_auth("login-changes", initial);
    let config = auth.with_file_name("config.toml");
    let reply = Step::Sse {
        body: CODEX_TEXT_SSE.into(),
    };
    let (codex, base, codex_task) = spawn(vec![reply.clone(); 3]).await;
    let (fallback, fallback_base, fallback_task) = spawn(vec![reply; 3]).await;
    let (_, mut fallback_manifest) = responses_manifest(&fallback_base);
    fallback_manifest.id = "fallback".into();
    let channel = gateway::chatgpt::ChatGptChannel::new(auth.clone(), config.clone())
        .with_responses_base(base.clone());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([
                responses_manifest(&base),
                (fallback_manifest.id.clone(), fallback_manifest),
            ]),
            "aubridge",
        )
        .with_codex_channel(channel),
    );
    let (client, gw, gateway_task) = spawn_gateway(state).await;

    for (file, keyring, provider) in [
        (Some(initial), false, "chatgpt"),
        (Some(switched), false, "chatgpt"),
        (Some("{\"tokens\":"), false, "fallback"),
        (Some(relogin), true, "fallback"),
        (None, false, "fallback"),
        (Some(relogin), false, "chatgpt"),
    ] {
        if let Some(file) = file {
            std::fs::write(&auth, file).unwrap();
        } else {
            std::fs::remove_file(&auth).unwrap();
        }
        let mode = if keyring { "keyring" } else { "file" };
        std::fs::write(
            &config,
            format!("cli_auth_credentials_store = \"{mode}\"\n"),
        )
        .unwrap();
        let response = post_message(&client, &gw).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-aubridge-provider"], provider);
        assert!(response.text().await.unwrap().contains("message_stop"));
    }

    let headers = codex.seen_headers.lock().await;
    assert_eq!(headers.len(), 3);
    for (sent, (access, account)) in headers.iter().zip([
        ("Bearer initial", "first"),
        ("Bearer initial", "second"),
        ("Bearer relogin", "third"),
    ]) {
        assert_eq!(sent["authorization"], access);
        assert_eq!(sent["chatgpt-account-id"], account);
    }
    assert_eq!(fallback.seen_headers.lock().await.len(), 3);
    for task in [gateway_task, codex_task, fallback_task] {
        task.abort();
    }
    std::fs::remove_dir_all(auth.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn codex_login_change_during_refresh_discards_late_tokens() {
    use std::sync::Arc;
    use std::time::Duration;

    let initial =
        r#"{"tokens":{"access_token":"initial","refresh_token":"r1","account_id":"first"}}"#;
    let switched =
        r#"{"tokens":{"access_token":"initial","refresh_token":"r2","account_id":"second"}}"#;
    for login_change in [None, Some(switched)] {
        let auth = tmp_codex_auth("login-change-during-refresh", initial);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let refresh_route = axum::Router::new().route(
            "/oauth/token",
            axum::routing::post({
                let started = started.clone();
                let release = release.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    assert_eq!(body["refresh_token"], "r1");
                    started.notify_one();
                    release.notified().await;
                    axum::Json(serde_json::json!({
                        "access_token": "late", "refresh_token": "late-refresh"
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refresh_endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let refresh_task = tokio::spawn(async move {
            axum::serve(listener, refresh_route).await.unwrap();
        });
        let reply = Step::Sse {
            body: CODEX_TEXT_SSE.into(),
        };
        let (codex, base, codex_task) = spawn(vec![
            Step::Fail {
                status: 401,
                body: r#"{"error":"token_expired"}"#.into(),
            },
            reply.clone(),
        ])
        .await;
        let (_, fallback_base, fallback_task) = spawn(vec![reply]).await;
        let (_, mut fallback) = responses_manifest(&fallback_base);
        fallback.id = "fallback".into();
        let channel =
            gateway::chatgpt::ChatGptChannel::new(auth.clone(), auth.with_file_name("config.toml"))
                .with_token_endpoint(refresh_endpoint)
                .with_responses_base(base.clone());
        channel.set_share_login(true);
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([responses_manifest(&base), (fallback.id.clone(), fallback)]),
                "aubridge",
            )
            .with_codex_channel(channel),
        );
        let (client, gw, gateway_task) = spawn_gateway(state).await;
        let request = tokio::spawn(async move { post_message(&client, &gw).await });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        if let Some(file) = login_change {
            std::fs::write(&auth, file).unwrap();
        } else {
            std::fs::remove_file(&auth).unwrap();
        }
        release.notify_one();
        let response = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers().contains_key("x-aubridge-provider"));
        let provider = response.headers()["x-aubridge-provider"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(response.text().await.unwrap().contains("message_stop"));
        let headers = codex.seen_headers.lock().await;
        if let Some(file) = login_change {
            assert_eq!(provider, "chatgpt");
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[1]["authorization"], "Bearer initial");
            assert_eq!(headers[1]["chatgpt-account-id"], "second");
            assert_eq!(std::fs::read_to_string(&auth).unwrap(), file);
        } else {
            assert_eq!(provider, "fallback");
            assert_eq!(headers.len(), 1);
            assert!(!auth.exists());
        }
        for task in [gateway_task, refresh_task, codex_task, fallback_task] {
            task.abort();
        }
        std::fs::remove_dir_all(auth.parent().unwrap()).unwrap();
    }
}

#[tokio::test]
async fn codex_native_responses_keep_opaque_items_and_use_only_the_builtin_oauth_channel() {
    let auth = tmp_codex_auth(
        "native-ingress",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-oauth","refresh_token":"fixture-refresh","account_id":"fixture-account"}}"#,
    );
    let upstream = include_str!("../../translate/tests/fixtures/codex_reasoning.sse");
    let (replay, base, upstream_task) = spawn(vec![
        Step::Sse { body: upstream.into() },
        Step::Sse { body: proto::sse::frame_event("response.failed", r#"{"type":"response.failed","response":{"id":"resp_fixture","error":{"code":"context_length_exceeded","message":"sensitive upstream diagnostic"}}}"#) },
    ]).await;
    let channel =
        gateway::chatgpt::ChatGptChannel::new(auth.clone(), auth.with_file_name("config.toml"))
            .with_responses_base(base.clone());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(BTreeMap::from([responses_manifest(&base)]), "aubridge")
            .with_codex_channel(channel)
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (client, gateway, gateway_task) = spawn_gateway(state).await;
    let request = client.post(format!("{gateway}/v1/responses")).bearer_auth("aubridge-codex").json(&serde_json::json!({
        "model":"gpt-6.1-sol", "stream":true, "include":["reasoning.encrypted_content"],
        "reasoning":{"effort":"high","summary":"detailed"},
        "input":[{"type":"reasoning","encrypted_content":"opaque-input"},{"role":"user","content":"fixture"}],
    }));
    let response = request.try_clone().unwrap().send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "chatgpt");
    let output = response.text().await.unwrap();
    assert!(
        output.contains("response.output_item.done")
            && output.contains("encrypted_content")
            && output.contains("response.completed")
    );
    assert!(!output.contains("message_stop"));
    let sent: serde_json::Value =
        serde_json::from_str(&replay.seen_bodies.lock().await[0]).unwrap();
    assert_eq!(sent["store"], false);
    assert_eq!(sent["input"][0]["encrypted_content"], "opaque-input");
    assert_eq!(
        sent["reasoning"],
        serde_json::json!({"effort":"high","summary":"detailed"})
    );
    assert_eq!(
        replay.seen_headers.lock().await[0]["authorization"],
        "Bearer fixture-oauth"
    );
    let rows: serde_json::Value = client
        .get(format!("{gateway}/admin/gateway"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows["requests"][0]["outcome"], "ok");
    assert!(rows["requests"][0]["input_tokens"].is_number());
    let terminal = request.send().await.unwrap();
    assert_eq!(terminal.status(), 400);
    let error: serde_json::Value = terminal.json().await.unwrap();
    assert_eq!(error["error"]["code"], "context_length_exceeded");
    assert!(!error.to_string().contains("sensitive upstream diagnostic"));
    gateway_task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(auth.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn responses_protocol_sends_codex_headers_and_translates() {
    // access 无可读 exp → 不主动刷新(§7:401 兜底),直接用盘上 access。
    let auth = tmp_codex_auth(
        "headers",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#,
    );
    let (replay, base, _upstream) = spawn(vec![Step::Sse {
        body: CODEX_TEXT_SSE.into(),
    }])
    .await;

    let manifests = BTreeMap::from([responses_manifest(&base)]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth,
        std::path::PathBuf::from("nonexistent-config.toml"),
    )
    .with_responses_base(base.clone());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge").with_codex_channel(channel),
    );
    let (client, gw, server) = spawn_gateway(state.clone()).await;

    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-aubridge-provider").unwrap(),
        "chatgpt"
    );
    let body = resp.text().await.unwrap();
    // F-12 真实语料:text 流 → message_start/文本块/message_stop。
    assert!(body.contains("message_start"));
    assert!(body.contains("content_block_delta"));
    assert!(body.contains("message_stop"));
    assert!(!body.contains("error"), "不应有错误事件: {body}");

    // §6.3 头全集(码值断言;token 是测试假值,可读)。
    let headers = replay.seen_headers.lock().await;
    assert_eq!(headers.len(), 1);
    let h = &headers[0];
    assert_eq!(h.get("authorization").unwrap(), "Bearer at-old");
    assert_eq!(h.get("chatgpt-account-id").unwrap(), "acc-42");
    assert_eq!(h.get("originator").unwrap(), "codex_cli_rs");
    assert_eq!(
        h.get("user-agent").unwrap(),
        "codex_cli_rs/0.166.0 (Mac OS 15.6; arm64) xterm-256color"
    );
    assert_eq!(h.get("openai-beta").unwrap(), "responses=experimental");
    drop(headers);

    // 请求体走 Responses 生成側(§6.3:stream/store 强制)。
    let bodies = replay.seen_bodies.lock().await;
    assert!(bodies[0].contains("\"stream\":true"));
    assert!(bodies[0].contains("\"store\":false"));
    // 生成側钳到 Responses 下限 128(sub2api 实证;请求里是 100)。
    assert!(bodies[0].contains("\"max_output_tokens\":128"));

    server.abort();
}

#[tokio::test]
async fn responses_401_refreshes_once_retries_and_writes_back() {
    let auth = tmp_codex_auth(
        "refresh",
        r#"{"auth_mode":"chatgpt","last_refresh":"2026-11-01T00:00:00Z",
            "tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#,
    );
    // 步序:先 401(token 过期)→ 刷新 → 重试成功(F-12 语料)。
    let steps = vec![
        Step::Fail {
            status: 401,
            body: r#"{"error":"token_expired"}"#.into(),
        },
        Step::Sse {
            body: CODEX_TEXT_SSE.into(),
        },
    ];
    let (replay, base, _upstream) = spawn_with_refresh(
        steps,
        r#"{"access_token":"at-new","refresh_token":"rt-new"}"#.into(),
    )
    .await;

    let manifests = BTreeMap::from([responses_manifest(&base)]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth.clone(),
        auth.parent().unwrap().join("config.toml"),
    )
    .with_token_endpoint(format!("{base}/oauth/token"))
    .with_responses_base(base.clone());
    let dir = auth.parent().unwrap();
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_codex_channel(channel)
            .with_settings(dir.join("config.json"), dir.join("stash")),
    );
    // §7 opt-in:开启"与 CLI 共享登录"才写回(开启即先 stash 备份)。
    state.set_share_login(true).expect("enable share_login");
    let (client, gw, server) = spawn_gateway(state).await;

    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_stop"));

    // 两次上游请求:旧 token 401,刷新后新 token 成功。
    let headers = replay.seen_headers.lock().await;
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].get("authorization").unwrap(), "Bearer at-old");
    assert_eq!(headers[1].get("authorization").unwrap(), "Bearer at-new");
    drop(headers);

    // 写回(§7 红线):字段级合并——access/refresh 更新、auth_mode
    // 保留、last_refresh 落地。
    let disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(disk["auth_mode"], "chatgpt");
    assert_eq!(disk["tokens"]["access_token"], "at-new");
    assert_eq!(disk["tokens"]["refresh_token"], "rt-new");
    assert!(disk["last_refresh"].as_str().is_some_and(|s| !s.is_empty()));

    server.abort();
}

/// §7 opt-in 默认只读:未开启"与 CLI 共享登录"时,401 兜底刷新照常
/// (内存凭据换新、重试成功),但 auth.json 分毫不动——刷新产物只
/// 留 aubridge 内存缓存。
#[tokio::test]
async fn share_login_off_by_default_leaves_auth_json_untouched() {
    let original = r#"{"auth_mode":"chatgpt","last_refresh":"2026-11-01T00:00:00Z",
        "tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#;
    let auth = tmp_codex_auth("readonly", original);
    let steps = vec![
        Step::Fail {
            status: 401,
            body: r#"{"error":"token_expired"}"#.into(),
        },
        Step::Sse {
            body: CODEX_TEXT_SSE.into(),
        },
        Step::Sse {
            body: CODEX_TEXT_SSE.into(),
        },
    ];
    let (replay, base, _upstream) = spawn_with_refresh(
        steps,
        r#"{"access_token":"at-new","refresh_token":"rt-new"}"#.into(),
    )
    .await;

    let manifests = BTreeMap::from([responses_manifest(&base)]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth.clone(),
        auth.parent().unwrap().join("config.toml"),
    )
    .with_token_endpoint(format!("{base}/oauth/token"))
    .with_responses_base(base.clone());
    let dir = auth.parent().unwrap();
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_codex_channel(channel)
            // settings 已接线但开关未开:验证的是 opt-in 语义,不是
            // "未接线所以碰不到文件"。
            .with_settings(dir.join("config.json"), dir.join("stash")),
    );
    let (client, gw, server) = spawn_gateway(state).await;

    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("message_stop"));

    // An in-place CLI update can expose a partial document briefly. It
    // cannot authorize a request, and rereading the same login keeps the
    // already rotated token instead of trying the superseded disk token.
    std::fs::write(&auth, "{").unwrap();
    let unavailable = post_message(&client, &gw).await;
    assert_eq!(unavailable.headers()["x-aubridge-provider"], "none");
    assert!(unavailable.text().await.unwrap().contains("event: error"));
    std::fs::write(&auth, original).unwrap();

    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("message_stop"));
    // 上游侧确实走了"旧 token 401 → 刷新 → 新 token 成功"。
    let headers = replay.seen_headers.lock().await;
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[1].get("authorization").unwrap(), "Bearer at-new");
    assert_eq!(headers[2].get("authorization").unwrap(), "Bearer at-new");
    drop(headers);

    // 盘上原样(无 config.json / stash 副作用)。
    assert_eq!(std::fs::read_to_string(&auth).unwrap(), original);
    assert!(!dir.join("config.json").exists());

    server.abort();
}

/// 设置路由(§7):GET 状态 + 风险披露;POST 开启 = 先备份后放行;
/// 一键还原把 stash 原样写回;admin key 空间独立校验。
#[tokio::test]
async fn settings_routes_toggle_backup_and_restore() {
    let original = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-old"}}"#;
    let auth = tmp_codex_auth("settings", original);
    let dir = auth.parent().unwrap();

    let manifests = BTreeMap::from([responses_manifest("http://127.0.0.1:1")]);
    // 显式接测试通道:不接的话 state 会默认指向真实 ~/.codex/auth.json,
    // 测试会碰用户文件。
    let channel = gateway::chatgpt::ChatGptChannel::new(auth.clone(), dir.join("config.toml"));
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_codex_channel(channel)
            .with_settings(dir.join("config.json"), dir.join("stash")),
    );
    let (client, gw, server) = spawn_gateway(state).await;

    // 非 admin key 拒之门外(§6 空间隔离)。
    let r = client
        .get(format!("{gw}/admin/settings"))
        .header("authorization", "Bearer aubridge")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 初始态:只读、无备份、有风险披露。
    let v: serde_json::Value = client
        .get(format!("{gw}/admin/settings"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["share_login"], false);
    assert_eq!(v["wired"], true);
    assert!(v["stash"].is_null());
    assert!(v["risk"].as_str().is_some_and(|s| s.contains("异常检测")));

    // 开启:开启即备份(stash 元信息可见),config.json 持久化。
    let r: serde_json::Value = client
        .post(format!("{gw}/admin/settings"))
        .header("authorization", "Bearer aubridge-admin")
        .header("content-type", "application/json")
        .body(r#"{"share_login":true}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["share_login"], true);
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg["share_login"], true);
    let v: serde_json::Value = client
        .get(format!("{gw}/admin/settings"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["share_login"], true);
    assert!(v["stash"]["sha256"].as_str().is_some_and(|s| s.len() == 64));

    // 模拟写回改动后的 auth.json → 一键还原回开启前的原样。
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-new"}}"#,
    )
    .unwrap();
    let repeated = client
        .post(format!("{gw}/admin/settings"))
        .bearer_auth("aubridge-admin")
        .json(&serde_json::json!({"share_login":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(repeated.status(), 200);
    let r = client
        .post(format!("{gw}/admin/settings/restore"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(std::fs::read_to_string(&auth).unwrap(), original);

    // 开关状态不被还原动到(退出共享另走开关)。
    let v: serde_json::Value = client
        .get(format!("{gw}/admin/settings"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["share_login"], true);

    server.abort();
}

/// 仪表盘快照(采样断言数据源;admin key 空间,§6)。
async fn provider_snapshot(client: &reqwest::Client, gw: &str, id: &str) -> serde_json::Value {
    let v: serde_json::Value = client
        .get(format!("{gw}/admin/dashboard"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .expect("dashboard")
        .json()
        .await
        .expect("dashboard json");
    v["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .expect("provider in snapshot")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn codex_429_usage_limit_body_rests_account_until_reset() {
    let auth = tmp_codex_auth(
        "usage-limit-body",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-access","refresh_token":"fixture-refresh","account_id":"fixture-account"}}"#,
    );
    let db = auth.with_file_name("ledger.sqlite");
    let reset = now_secs() + 3600;
    let (replay, base, upstream) = spawn(vec![Step::Fail {
        status: 429,
        body: serde_json::json!({
            "error":{
                "type":"usage_limit_reached",
                "message":"The usage limit has been reached",
                "resets_at":reset
            }
        })
        .to_string(),
    }])
    .await;
    let state = || {
        let (id, mut manifest) = responses_manifest(&base);
        manifest.free_models.push("gpt-6.1-other".into());
        let channel =
            gateway::chatgpt::ChatGptChannel::new(auth.clone(), auth.with_file_name("config.toml"))
                .with_responses_base(base.clone());
        std::sync::Arc::new(
            GatewayState::from_manifests(BTreeMap::from([(id, manifest)]), "aubridge")
                .with_codex_channel(channel)
                .with_ledger(ledger::Ledger::open(&db).unwrap()),
        )
    };
    let assert_cooldown = |response: &reqwest::Response| {
        assert_eq!(response.status(), 429);
        let retry: u64 = response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let remaining = reset.saturating_sub(now_secs());
        assert!(
            (remaining..=remaining + 2).contains(&retry),
            "Retry-After: {retry}, body reset is in {remaining}s"
        );
    };
    let active = state();
    let (client, gw, server) = spawn_gateway(active.clone()).await;
    let response = post_message(&client, &gw).await;
    assert_cooldown(&response);
    let failure: serde_json::Value = response.json().await.unwrap();
    assert_eq!(failure["error"]["type"], "rate_limit_error");
    assert!(!failure
        .to_string()
        .contains("The usage limit has been reached"));
    let provider = provider_snapshot(&client, &gw, "chatgpt").await;
    assert_eq!(provider["resting"], true);
    assert_eq!(provider["sunk"], true);
    assert_eq!(provider["resets_at"], reset);
    let other = |client: &reqwest::Client, gw: &str| {
        client
            .post(format!("{gw}/v1/messages"))
            .bearer_auth("aubridge")
            .json(&serde_json::json!({
                "model":"gpt-6.1-other", "max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]
            }))
            .send()
    };
    assert_cooldown(&other(&client, &gw).await.unwrap());
    let rows = active.recent_decisions().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].model, "gpt-6.1-other");
    assert_eq!(rows[0].attempts, "[]");
    let skipped: serde_json::Value =
        serde_json::from_str(rows[0].skipped.as_deref().unwrap()).unwrap();
    assert_eq!(skipped[0]["reason"], "account-quota-exhausted");
    active.flush_router_state();
    server.abort();
    let _ = server.await;
    drop(active);

    let (client, gw, server) = spawn_gateway(state()).await;
    assert_cooldown(&other(&client, &gw).await.unwrap());
    assert_eq!(replay.seen_bodies.lock().await.len(), 1);
    server.abort();
    let _ = server.await;
    upstream.abort();
    std::fs::remove_dir_all(auth.parent().unwrap()).unwrap();
}

/// 429 + `x-codex-*-used-percent` ≥100(§8.2 被动采样):无 quota
/// 词表命中也按窗口耗尽处理——rest 到受限窗口重置时刻、沉底。
#[tokio::test]
async fn codex_429_sampled_window_exhausted_rests_until_reset() {
    let auth = tmp_codex_auth(
        "sample-429",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#,
    );
    // secondary(5h)耗尽 100%、600s 后重置;primary(7d)只有 42%
    // 未耗尽——reset 必须取受限窗口(600s),不是 primary 的 50 万秒。
    let (replay, base, _upstream) = spawn(vec![Step::FailWithHeaders {
        status: 429,
        body: r#"{"error":"secondary window exhausted"}"#.into(),
        headers: vec![
            ("x-codex-primary-used-percent".into(), "42".into()),
            (
                "x-codex-primary-reset-after-seconds".into(),
                "500000".into(),
            ),
            ("x-codex-secondary-used-percent".into(), "100".into()),
            ("x-codex-secondary-reset-after-seconds".into(), "600".into()),
        ],
    }])
    .await;

    let manifests = BTreeMap::from([responses_manifest(&base)]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth,
        std::path::PathBuf::from("nonexistent-config.toml"),
    )
    .with_responses_base(base.clone());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge").with_codex_channel(channel),
    );
    let (client, gw, server) = spawn_gateway(state).await;

    // 首段尚未提交，客户端收到耗尽窗口的 HTTP 429 和退避秒数。
    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 429);
    let retry: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((598..=601).contains(&retry), "Retry-After: {retry}");
    let failure: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(failure["error"]["type"], "rate_limit_error");
    drop(replay);

    let p = provider_snapshot(&client, &gw, "chatgpt").await;
    assert_eq!(p["resting"], true, "窗口耗尽应休息: {p}");
    assert_eq!(p["sunk"], true, "真没量应沉底: {p}");
    let resets_at = p["resets_at"].as_u64().expect("resets_at 采样落地");
    let now = now_secs();
    assert!(
        (now + 300..=now + 700).contains(&resets_at),
        "reset 应来自受限 secondary 窗口(~600s): {resets_at}"
    );

    server.abort();
}

/// 成功响应的 `x-codex-*` 被动采样(§8.2):未耗尽 → 只记录窗口
/// 重置时刻(双窗口取较早),不休息、不沉底。
#[tokio::test]
async fn codex_success_samples_window_reset_without_resting() {
    let auth = tmp_codex_auth(
        "sample-200",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#,
    );
    let (_replay, base, _upstream) = spawn(vec![Step::SseWithHeaders {
        body: CODEX_TEXT_SSE.into(),
        headers: vec![
            ("x-codex-primary-used-percent".into(), "42".into()),
            ("x-codex-primary-reset-after-seconds".into(), "3600".into()),
            ("x-codex-secondary-used-percent".into(), "7.5".into()),
            ("x-codex-secondary-reset-after-seconds".into(), "600".into()),
        ],
    }])
    .await;

    let manifests = BTreeMap::from([responses_manifest(&base)]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth,
        std::path::PathBuf::from("nonexistent-config.toml"),
    )
    .with_responses_base(base.clone());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge").with_codex_channel(channel),
    );
    let (client, gw, server) = spawn_gateway(state).await;

    let resp = post_message(&client, &gw).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("message_stop"));

    let p = provider_snapshot(&client, &gw, "chatgpt").await;
    assert_eq!(p["resting"], false, "未耗尽不应休息: {p}");
    assert_eq!(p["sunk"], false, "未耗尽不应沉底: {p}");
    // 利用率被动采样(§8.2):primary=7d、secondary=5h,浮点 percent
    // 也入(仪表盘口径)。
    assert_eq!(p["util_7d"].as_f64().unwrap(), 42.0, "util_7h: {p}");
    assert_eq!(p["util_5h"].as_f64().unwrap(), 7.5, "util_5h: {p}");
    let resets_at = p["resets_at"].as_u64().expect("resets_at 采样落地");
    let now = now_secs();
    // 双窗口取较早(secondary 600s,不是 primary 3600s)。
    assert!(
        (now + 300..=now + 700).contains(&resets_at),
        "reset 应取较早的 secondary 窗口(~600s): {resets_at}"
    );

    server.abort();
}

/// 仪表盘触发的 codex wham 主动查询(§8.2 第二数据源):Responses 源
/// + 凭据可用 → 查一次(Bearer = 盘上 access token),利用率与重置
/// 时刻并入快照;缓存 3min,反复刷新不打上游。v1 口径:主动查询
/// 只补利用率/重置,rest 决策仍由被动采样/失败路径驱动。
#[tokio::test]
async fn dashboard_queries_wham_usage_for_responses_sources() {
    let auth = tmp_codex_auth(
        "wham",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-old","refresh_token":"rt-old","account_id":"acc-42"}}"#,
    );
    let hits: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let hits2 = hits.clone();
    let wham_app = axum::Router::new().route(
        "/backend-api/wham/usage",
        axum::routing::get(move |h: axum::http::HeaderMap| {
            let hits = hits2.clone();
            async move {
                hits.lock().unwrap().push(
                    h.get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string(),
                );
                axum::Json(serde_json::json!({
                    "rate_limit": {
                        "primary_window": {"used_percent": 20.0, "reset_at": 1762300000},
                        "secondary_window": {"used_percent": 60.0, "reset_at": 1762205400}
                    }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wham_url = format!(
        "http://{}/backend-api/wham/usage",
        listener.local_addr().unwrap()
    );
    let wham_server = tokio::spawn(async move { axum::serve(listener, wham_app).await.unwrap() });

    // 仪表盘查询不依赖消息转发:base 指向无人监听的端口即可。
    let manifests = BTreeMap::from([responses_manifest("http://127.0.0.1:1")]);
    let channel = gateway::chatgpt::ChatGptChannel::new(
        auth,
        std::path::PathBuf::from("nonexistent-config.toml"),
    )
    .with_responses_base("http://127.0.0.1:1");
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_codex_channel(channel)
            .with_wham_endpoint(wham_url),
    );
    let (client, gw, server) = spawn_gateway(state).await;

    let p = provider_snapshot(&client, &gw, "chatgpt").await;
    assert_eq!(p["util_5h"].as_f64().unwrap(), 60.0, "secondary=5h: {p}");
    assert_eq!(p["util_7d"].as_f64().unwrap(), 20.0, "primary=7d: {p}");
    assert_eq!(
        p["resets_at"].as_u64().unwrap(),
        1762205400,
        "5h 窗口 reset: {p}"
    );

    // 缓存命中:第二次仪表盘刷新不再打 wham 服务器。
    let _ = provider_snapshot(&client, &gw, "chatgpt").await;
    let seen = hits.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["Bearer at-old".to_string()],
        "只查一次且带 Bearer"
    );

    server.abort();
    wham_server.abort();
}

/// router 运行态 write-behind 持久化(M2,engineering-constraints
/// B-8):429 rest/sink + 亲和粘性 flush 落盘后,用同一 ledger 文件
/// 重建网关 → 状态与粘性都不丢;kind/models 仍以当前 manifest 为准。
#[tokio::test]
async fn router_state_survives_gateway_restart() {
    let dir = std::env::temp_dir().join(format!("aubridge-gw-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ledger.sqlite");
    let manifests_of = |a: &str, b: &str| {
        let mk = |id: &str, base: &str| -> (String, Manifest) {
            (
                id.to_string(),
                serde_json::from_value(serde_json::json!({
                    "id": id, "version": "1",
                    "provider": { "base": base, "auth": "bearer" },
                    "free_models": ["glm-4.5-flash"],
                    "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                    "permissions": { "net": ["127.0.0.1"], "store_key": false }
                }))
                .expect("preset json"),
            )
        };
        BTreeMap::from([mk("prov-a", a), mk("prov-b", b)])
    };

    // 第一代:A 429(→ rest/sink),B 答话;粘性记到 B;flush 落盘。
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (_rb, b_url, _b) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests_of(&a_url, &b_url), "aubridge")
            .with_ledger(ledger::Ledger::open(db.to_str().unwrap()).unwrap()),
    );
    let (client, gw, server) = spawn_gateway(state.clone()).await;
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":100,
                "messages":[{"role":"user","content":"weather?"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    state.record_affinity("sess-persist", "prov-b", 4096);
    state.flush_router_state();
    server.abort();

    // 第二代:同一 ledger 文件重建网关。
    let l2 = ledger::Ledger::open(db.to_str().unwrap()).unwrap();
    let state2 =
        GatewayState::from_manifests(manifests_of(&a_url, &b_url), "aubridge").with_ledger(l2);
    let a = state2
        .snapshot()
        .into_iter()
        .find(|p| p.id == "prov-a")
        .expect("prov-a state");
    assert!(a.sunk, "重启后 sink 不丢: {a:?}");
    assert!(a.rest_until.is_some(), "重启后 rest 不丢: {a:?}");
    assert_eq!(
        a.models,
        vec!["glm-4.5-flash".to_string()],
        "models 以当前 manifest 为准"
    );
    let stick = state2.stick("sess-persist").expect("粘性重启不丢");
    assert_eq!(stick.main, "prov-b");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recreated_custom_provider_starts_without_a_retired_snapshot() {
    let dir =
        std::env::temp_dir().join(format!("aubridge-recreated-source-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("ledger.sqlite");
    let id = "recreated-free";
    let mut retired = manifest::state::ProviderState::new(
        id,
        manifest::schema::ProviderKind::FreeTier,
        vec!["free-model".into()],
    );
    retired.enabled = false;
    let ledger = ledger::Ledger::open(&database).unwrap();
    ledger.save_provider_state(id, &retired).unwrap();
    ledger
        .record_requests(&[ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                model: "free-model".into(),
                provider: id.into(),
                outcome: "ok".into(),
                ..Default::default()
            },
            usage: Vec::new(),
        }])
        .unwrap();
    let make_state = |ledger| {
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_settings(dir.join("config.json"), dir.join("stash"))
            .with_ledger(ledger)
    };
    let state = make_state(ledger);
    let provider = serde_json::from_value(serde_json::json!({
        "id":id,"base":"http://127.0.0.1:10001/v1","models":["free-model"]
    }))
    .unwrap();
    state.add_custom_provider(&provider).unwrap();
    assert!(state.snapshot()[0].enabled);
    // Restart without a write-behind flush of the new generation.
    drop(state);
    let restarted = make_state(ledger::Ledger::open(&database).unwrap());
    assert!(restarted.snapshot()[0].enabled);
    assert_eq!(restarted.recent_decisions().unwrap()[0].provider, id);
    drop(restarted);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Anthropic 协议成功响应的 unified 限流头被动采样(§8.2):0–1 小数
/// 换算百分比入源状态(5h/7d 双窗口),reset 时刻落地;头缺失的源
/// 不写(未知不覆盖)。
#[tokio::test]
async fn anthropic_success_samples_unified_utilization_headers() {
    let reset_at = 1762300000u64;
    let (_replay, base, _upstream) = spawn(vec![Step::SseWithHeaders {
        body: anthropic_sse_with_cache(0, 0),
        headers: vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.42".into(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization".into(),
                "0.15".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".into(),
                reset_at.to_string(),
            ),
        ],
    }])
    .await;

    let manifests = BTreeMap::from([(
        "prov-a".to_string(),
        serde_json::from_value::<Manifest>(serde_json::json!({
            "id": "prov-a", "version": "1", "kind": "subscription",
            "provider": { "base": base, "auth": "api-key", "protocol": "anthropic" },
            "free_models": ["kimi-k3"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let (client, gw, server) = spawn_gateway(state).await;

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"kimi-k3","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let provider = resp
        .headers()
        .get("x-aubridge-provider")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.bytes().await;
    assert_eq!(provider, "prov-a", "透传路径成功(采样发生在成功分支)");

    let p = provider_snapshot(&client, &gw, "prov-a").await;
    assert_eq!(p["util_5h"].as_f64().unwrap(), 42.0, "0.42 → 42%: {p}");
    assert_eq!(p["util_7d"].as_f64().unwrap(), 15.0, "0.15 → 15%: {p}");
    assert_eq!(
        p["resets_at"].as_u64().unwrap(),
        reset_at,
        "reset 采样: {p}"
    );
    assert_eq!(p["resting"], false, "利用率未到耗尽线不休息: {p}");

    server.abort();
}

/// 熔断器(§8.1):单源连续 5 次 5xx → open,后续请求直接排除该源
/// (不再打上游),仪表盘 breaker_open=true。半开语义(open_until 过期
/// 放行、2 成恢复)由 router 纯单测覆盖——集成测不可注入时钟。
#[tokio::test]
async fn breaker_opens_source_after_five_5xx() {
    let (replay, base, _upstream) = spawn(vec![
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Fail {
            status: 502,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Fail {
            status: 503,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Fail {
            status: 504,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        // 若熔断未生效,第 6 次请求会打到这里(不应消费)。
        Step::Sse {
            body: CHAT_TOOL_CALL.into(),
        },
    ])
    .await;

    let manifests = BTreeMap::from([(
        "prov-x".to_string(),
        serde_json::from_value::<Manifest>(serde_json::json!({
            "id": "prov-x", "version": "1",
            "provider": { "base": base, "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "quota": { "source": "inferred", "rest_hint": "daily-utc" },
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("preset json"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let (client, gw, server) = spawn_gateway(state).await;

    for i in 1..=5 {
        // post_message 助手固定 gpt-6.1-sol;本源供 glm-4.5-flash。
        let resp = client
            .post(format!("{gw}/v1/messages"))
            .header("authorization", "Bearer aubridge")
            .body(
                r#"{"model":"glm-4.5-flash","max_tokens":100,
                    "messages":[{"role":"user","content":"hi"}]}"#,
            )
            .send()
            .await
            .expect("gateway request");
        assert_eq!(resp.status(), 200);
        assert!(
            resp.text().await.unwrap().contains("\"error\""),
            "第 {i} 次失败请求应有 error 终止"
        );
    }
    let p = provider_snapshot(&client, &gw, "prov-x").await;
    assert_eq!(p["breaker_open"], true, "5 连败后应熔断: {p}");
    assert_eq!(p["resting"], false, "熔断不走 rest: {p}");

    // 第 6 次:候选排除熔断源 → 无候选 → error 终止;上游不再被打。
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("no candidate"));
    let remaining = replay.steps.lock().await.len();
    assert_eq!(remaining, 1, "SSE 步不应被消费(源被熔断排除)");

    server.abort();
}

/// 循环护栏(§8.5):同一 tool call 连续 8 轮 → 本地作答,end_turn,
/// 不打上游(上游回放步不被消费)。
#[tokio::test]
async fn tool_loop_guard_answers_locally_without_hitting_upstream() {
    let (replay, base, _upstream) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;

    let manifests = BTreeMap::from([(
        "prov-a".to_string(),
        serde_json::from_value::<Manifest>(serde_json::json!({
            "id": "prov-a", "version": "1",
            "provider": { "base": base, "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "quota": { "source": "inferred", "rest_hint": "daily-utc" },
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("preset json"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let (client, gw, server) = spawn_gateway(state).await;

    // 8 轮:assistant(同一 Read 调用)→ user(tool_result)。
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for _ in 0..8 {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "tu-1", "name": "Read",
                          "input": {"file_path": "/tmp/a.txt"}}]
        }));
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu-1",
                          "content": "same content"}]
        }));
    }
    let body = serde_json::json!({
        "model": "glm-4.5-flash", "max_tokens": 100, "messages": messages
    })
    .to_string();

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(body)
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-aubridge-provider").unwrap(),
        "loop-guard"
    );
    let text = resp.text().await.unwrap();
    assert!(text.contains("同一工具调用"), "本地作答文本: {text}");
    assert!(text.contains("end_turn"));
    assert!(text.contains("message_stop"));
    // 上游一次都没被打(止损线)。
    let remaining = replay.steps.lock().await.len();
    assert_eq!(remaining, 1, "SSE 步不应被消费(护栏本地作答)");

    server.abort();
}

/// 订阅源并发槽位(§8.5:每账号上限 2):3 并发请求打同一订阅源,
/// 第 3 个必须等前两个之一结束才拿到槽位(延迟 ≈ 2×上游延迟);
/// manifest kind 缺省(免费源)不受限(对照组)。
#[tokio::test]
async fn subscription_slot_caps_concurrent_streams() {
    async fn run(kind: Option<&str>) -> Vec<std::time::Duration> {
        // 上游每响应延迟 800ms(占住流 → 占住槽位)。
        let steps = vec![
            Step::SseAfterDelay {
                body: CANNED_MIN.into(),
                delay_ms: 800,
            },
            Step::SseAfterDelay {
                body: CANNED_MIN.into(),
                delay_ms: 800,
            },
            Step::SseAfterDelay {
                body: CANNED_MIN.into(),
                delay_ms: 800,
            },
        ];
        let (_replay, up_url, _u) = spawn(steps).await;
        let mut m = serde_json::json!({
            "id": "prov-sub", "version": "1",
            "provider": { "base": up_url, "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        });
        if let Some(k) = kind {
            m["kind"] = serde_json::json!(k);
        }
        let manifests = BTreeMap::from([(
            "prov-sub".to_string(),
            serde_json::from_value::<Manifest>(m).expect("manifest"),
        )]);
        let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
        let app = app(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let one = |client: reqwest::Client, gw: String| async move {
            let t0 = std::time::Instant::now();
            let resp = client
                .post(format!("http://{gw}/v1/messages"))
                .header("authorization", "Bearer aubridge")
                .body(
                    r#"{"model":"glm-4.5-flash","max_tokens":1,
                        "messages":[{"role":"user","content":"hi"}]}"#,
                )
                .send()
                .await
                .expect("gateway request");
            assert_eq!(resp.status(), 200);
            let _ = resp.bytes().await.expect("drain body");
            t0.elapsed()
        };
        let (a, b, c) = tokio::join!(
            one(client.clone(), gw.to_string()),
            one(client.clone(), gw.to_string()),
            one(client, gw.to_string())
        );
        server.abort();
        vec![a, b, c]
    }

    // 订阅源:两条快(≈800ms,立即拿到槽位),一条慢(≈1600ms,
    // 等槽位)。
    let mut sub = run(Some("subscription")).await;
    sub.sort();
    assert!(
        sub[1] < std::time::Duration::from_millis(1400),
        "前两个请求应立即拿到槽位: {sub:?}"
    );
    assert!(
        sub[2] >= std::time::Duration::from_millis(1200),
        "第 3 个请求应等槽位(≈2×上游延迟): {sub:?}"
    );

    // 免费源(对照组):三个都 ≈800ms,不受槽位限制。
    let free = run(None).await;
    let max = free.into_iter().max().unwrap();
    assert!(
        max < std::time::Duration::from_millis(1400),
        "免费源不受并发槽位限制: {max:?}"
    );
}

/// 文本重复循环切断(§8.5 第二护栏):透传路径。上游 Anthropic SSE
/// 退化成逐字重复 → 流被提前切断(error 终止,F-5),后续帧不到达。
#[tokio::test]
async fn repetition_loop_cut_on_passthrough_stream() {
    let phrase = "同一段内容不断重复输出,";
    let mut anthropic_sse = String::from(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_loop_1\",\"model\":\"kimi-k3\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    ));
    // 退化循环:同一短语 40 遍(远超检测阈值)。
    for _ in 0..40 {
        anthropic_sse.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\
             \"delta\":{{\"type\":\"text_delta\",\"text\":\"{phrase}\"}}}}\n\n"
        ));
    }
    // 切断点之后不应到达的标记帧。
    anthropic_sse.push_str(concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));

    let (_replay, up_url, _u) = spawn(vec![Step::Sse {
        body: anthropic_sse,
    }])
    .await;
    let manifests = BTreeMap::from([(
        "kimi-code".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "kimi-code", "version": "1", "kind": "subscription",
            "provider": { "base": up_url, "auth": "api-key", "protocol": "anthropic" },
            "free_models": ["kimi-k3"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"kimi-k3","max_tokens":4096,
            "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // F-5 口径:完整 error 终止事件 + message_stop,不裸断流。
    assert!(body.contains("重复循环"), "error 终止事件: {body}");
    assert!(body.contains("message_stop"));
    // 上游原生的 end_turn 收尾不应到达(提前切断)。
    assert!(
        !body.contains("\"stop_reason\":\"end_turn\""),
        "上游收尾帧应被截断: {body}"
    );

    server.abort();
}

/// 文本重复循环切断:翻译路径(Chat 上游)。重复 content delta →
/// 翻译流被切断,后续 delta 不到达。
#[tokio::test]
async fn repetition_loop_cut_on_translate_stream() {
    let phrase = "the same degenerate output ";
    let mut chat_sse = String::new();
    for _ in 0..40 {
        chat_sse.push_str(&format!(
            "data: {{\"id\":\"1\",\"object\":\"chat.completion.chunk\",\
             \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{phrase}\"}}}}]}}\n\n"
        ));
    }
    // 切断点之后不应到达的标记 delta。
    chat_sse.push_str(concat!(
        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"TAIL_MARKER_UNIQUE\"}}]}\n\n",
        "data: [DONE]\n\n",
    ));

    let (_replay, up_url, _u) = spawn(vec![Step::Sse { body: chat_sse }]).await;
    let manifests = BTreeMap::from([(
        "zhipu".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "zhipu", "version": "1",
            "provider": { "base": up_url, "auth": "bearer" },
            "free_models": ["glm-4.5-flash"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .expect("manifest"),
    )]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"glm-4.5-flash","max_tokens":4096,
            "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("重复循环"), "error 终止事件: {body}");
    assert!(body.contains("message_stop"));
    assert!(
        !body.contains("TAIL_MARKER_UNIQUE"),
        "切断点之后的翻译帧不应到达: {body}"
    );

    server.abort();
}

// ---------------------------------------------------------------------------
// 会话亲和(§8.4):仅订阅源、按回复 cache 量粘、冷窗内有效
// ---------------------------------------------------------------------------

/// 两个订阅源(快照序 prov-a 在前 = 正常序首选)。
fn two_subscription_state(
    a_url: &str,
    b_url: &str,
    b_protocol: &str,
) -> std::sync::Arc<GatewayState> {
    let mk = |id: &str, base: &str, protocol: &str| {
        (
            id.to_string(),
            serde_json::from_value::<Manifest>(serde_json::json!({
                "id": id, "version": "1", "kind": "subscription",
                "provider": { "base": base, "auth": "api-key", "protocol": protocol },
                "free_models": ["kimi-k3"],
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("manifest"),
        )
    };
    std::sync::Arc::new(GatewayState::from_manifests(
        BTreeMap::from([
            mk("prov-a", a_url, "anthropic"),
            mk("prov-b", b_url, b_protocol),
        ]),
        "aubridge",
    ))
}

/// 带指定 cache 量的最小 Anthropic SSE(亲和粘性依据 = 回复 usage 的
/// cache_read + cache_creation 实际量,§8.4)。
fn anthropic_sse_with_cache(read: u64, creation: u64) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_aff\",\"model\":\"kimi-k3\",",
            "\"role\":\"assistant\",\"usage\":{{\"input_tokens\":50,",
            "\"cache_read_input_tokens\":{read},\"cache_creation_input_tokens\":{creation}}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},",
            "\"usage\":{{\"output_tokens\":3,",
            "\"cache_read_input_tokens\":{read},\"cache_creation_input_tokens\":{creation}}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        read = read,
        creation = creation,
    )
}

/// 发一发请求,返回应答源(x-aubridge-provider)。可选会话头与
/// Claude Code 子 agent 头(§8.4 亲和 key 来源)。
async fn affinity_probe(gw: &str, session: Option<&str>, agent: Option<&str>) -> String {
    let mut req = reqwest::Client::new()
        .post(format!("http://{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge");
    if let Some(s) = session {
        req = req.header("x-claude-code-session-id", s);
    }
    if let Some(a) = agent {
        req = req.header("x-claude-code-agent-id", a);
    }
    let resp = req
        .body(
            r#"{"model":"kimi-k3","max_tokens":100,
                "messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    let provider = resp
        .headers()
        .get("x-aubridge-provider")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.bytes().await.expect("drain body");
    provider
}

/// 粘性建立与重排:首回 prov-b 的 cache ≥1024 → 同会话后续越过
/// 正常序直达 prov-b;无会话头的请求(另一会话)与子 agent
/// (独立 key)不受牵连。
#[tokio::test]
async fn session_affinity_sticks_by_cache_and_scopes_to_session() {
    let sse_stick = anthropic_sse_with_cache(2048, 512);
    let sse_plain = anthropic_sse_with_cache(0, 0);
    // A:首回 500(把首答让给 B,粘到 B),后续成功。
    let (_ra, a_url, _a) = spawn(vec![
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Sse {
            body: sse_plain.clone(),
        },
        Step::Sse {
            body: sse_plain.clone(),
        },
    ])
    .await;
    let (_rb, b_url, _b) = spawn(vec![
        Step::Sse {
            body: sse_stick.clone(),
        },
        Step::Sse {
            body: sse_stick.clone(),
        },
    ])
    .await;

    let state = two_subscription_state(&a_url, &b_url, "anthropic");
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // 首答:prov-a 500 → failover prov-b(带 cache 量)→ 粘住。
    let p1 = affinity_probe(&gw.to_string(), Some("sess-1"), None).await;
    // 同会话:亲和重排,prov-b 越过正常序的 prov-a 提前。
    let p2 = affinity_probe(&gw.to_string(), Some("sess-1"), None).await;
    // 无会话头:另一会话(首条 user 消息 hash key),无粘性,正常序。
    let p3 = affinity_probe(&gw.to_string(), None, None).await;
    // 子 agent:同会话标识 + 独立后缀,不覆盖主会话粘性。
    let p4 = affinity_probe(&gw.to_string(), Some("sess-1"), Some("agent-9")).await;

    assert_eq!(
        (p1.as_str(), p2.as_str(), p3.as_str(), p4.as_str()),
        ("prov-b", "prov-b", "prov-a", "prov-a"),
        "粘性只作用于同 key 会话"
    );
    let stick = state.stick("sess-1").expect("粘性应已建立");
    assert_eq!(stick.main, "prov-b");
    assert!(state.stick("sess-1|agent:agent-9").is_none());

    server.abort();
}

async fn assert_cache_write_affinity(protocol: &str, cached_reply: String) {
    let (a, a_url, a_task) = spawn(vec![
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Sse {
            body: anthropic_sse_with_cache(0, 0),
        },
        Step::Sse {
            body: anthropic_sse_with_cache(0, 0),
        },
    ])
    .await;
    let (b, b_url, b_task) = spawn(vec![
        Step::Sse {
            body: cached_reply.clone(),
        },
        Step::Sse { body: cached_reply },
    ])
    .await;
    let state = two_subscription_state(&a_url, &b_url, protocol);
    let (_, gateway_url, server) = spawn_gateway(state.clone()).await;
    let gateway = gateway_url.strip_prefix("http://").unwrap();

    assert_eq!(
        affinity_probe(gateway, Some("cached"), None).await,
        "prov-b"
    );
    let stick = state
        .stick("cached")
        .expect("nested cache write establishes affinity");
    assert_eq!(stick.main, "prov-b");
    assert_eq!(
        stick.cache, 1536,
        "aliases and snapshots share a single count"
    );
    assert_eq!(
        affinity_probe(gateway, Some("cached"), None).await,
        "prov-b"
    );
    assert_eq!(affinity_probe(gateway, Some("fresh"), None).await, "prov-a");
    assert_eq!(a.seen_bodies.lock().await.len(), 2);
    assert_eq!(b.seen_bodies.lock().await.len(), 2);

    server.abort();
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn chat_cache_write_retains_subscription_affinity() {
    let usage = r#"data: {"id":"chatcmpl-affinity","choices":[],"usage":{"prompt_tokens":1800,"completion_tokens":3,"cache_creation_input_tokens":512,"prompt_tokens_details":{"cache_write_tokens":1536}}}"#;
    let reply = format!(
        concat!(
            "data: {{\"id\":\"chatcmpl-affinity\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"ok\"}},\"finish_reason\":\"stop\"}}]}}\n\n",
            "{usage}\n\n{usage}\n\ndata: [DONE]\n\n"
        ),
        usage = usage
    );
    assert_cache_write_affinity("chat", reply).await;
}

#[tokio::test]
async fn responses_cache_write_retains_subscription_affinity() {
    let reply = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_affinity\"}}\n\n",
        "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_affinity\",\"usage\":{\"input_tokens\":1800,\"input_tokens_details\":{\"cache_write_tokens\":1536}}}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_affinity\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1800,\"output_tokens\":3,\"cache_creation_input_tokens\":512,\"input_tokens_details\":{\"cache_write_tokens\":1500}}}}\n\n"
    );
    assert_cache_write_affinity("responses", reply.into()).await;
}

/// cache 量不足 1024 不粘(§8.4:小 cache 粘住只剩延迟劣势)。
#[tokio::test]
async fn session_affinity_requires_cache_worth() {
    let sse_thin = anthropic_sse_with_cache(100, 0);
    let sse_plain = anthropic_sse_with_cache(0, 0);
    let (_ra, a_url, _a) = spawn(vec![
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Sse {
            body: sse_plain.clone(),
        },
    ])
    .await;
    let (_rb, b_url, _b) = spawn(vec![Step::Sse {
        body: sse_thin.clone(),
    }])
    .await;

    let state = two_subscription_state(&a_url, &b_url, "anthropic");
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // 首答 prov-b 但 cache 100:不值一粘。
    let p1 = affinity_probe(&gw.to_string(), Some("sess-2"), None).await;
    // 同会话第二发:无粘性,回正常序 prov-a。
    let p2 = affinity_probe(&gw.to_string(), Some("sess-2"), None).await;
    assert_eq!((p1.as_str(), p2.as_str()), ("prov-b", "prov-a"));
    assert!(state.stick("sess-2").is_none());

    server.abort();
}

/// 亲和源 rest 时立即放行迁移(§8.4):亲和源被限速 → failover 替补
/// 答话;粘性记录保留 main(backToMain,主力恢复后交回);亲和源
/// rest 中不再被亲和回打。
#[tokio::test]
async fn session_affinity_yields_while_sticky_source_rests() {
    let sse_stick = anthropic_sse_with_cache(2048, 512);
    let sse_plain = anthropic_sse_with_cache(0, 0);
    let (_ra, a_url, _a) = spawn(vec![
        Step::Fail {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        },
        Step::Sse {
            body: sse_plain.clone(),
        },
        Step::Sse {
            body: sse_plain.clone(),
        },
    ])
    .await;
    // B:先成功(粘),再 429 + retry-after 60s(RateFor,不 sink)。
    let (_rb, b_url, _b) = spawn(vec![
        Step::Sse {
            body: sse_stick.clone(),
        },
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":"temporary"}"#.into(),
            headers: vec![("retry-after".into(), "60".into())],
        },
    ])
    .await;

    let state = two_subscription_state(&a_url, &b_url, "anthropic");
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // 首答 prov-b(带 cache)→ 粘住。
    let p1 = affinity_probe(&gw.to_string(), Some("sess-3"), None).await;
    // 同会话:亲和把 prov-b 提前 → prov-b 429 rest → failover prov-a
    //(亲和不阻断迁移;替补答话无 cache 量,不改写粘性记录)。
    let p2 = affinity_probe(&gw.to_string(), Some("sess-3"), None).await;
    // 亲和源 rest 中:亲和不再回打 prov-b,正常序直接 prov-a。
    let p3 = affinity_probe(&gw.to_string(), Some("sess-3"), None).await;
    assert_eq!(
        (p1.as_str(), p2.as_str(), p3.as_str()),
        ("prov-b", "prov-a", "prov-a")
    );
    // backToMain 数据面:main 未被替补答话改写。
    let stick = state.stick("sess-3").expect("粘性应保留");
    assert_eq!(
        (stick.main.as_str(), stick.last.as_str()),
        ("prov-b", "prov-b")
    );

    server.abort();
}

/// CORS(§6 cors.go 语义):桌面壳/浏览器页跨源 fetch 要能读响应——
/// 通过守卫的响应回显 Origin,预检 OPTIONS 按声明放行;拒绝响应
/// (403)不回显,不帮恶意站点探测。
#[tokio::test]
async fn cors_only_admits_the_desktop_origin_with_the_right_key() {
    // Messages 非流式响应由网关收齐上游 SSE 后组装。
    let (upstream, up_url, upstream_task) = spawn(vec![Step::Sse {
        body: anthropic_sse_with_cache(0, 0),
    }])
    .await;
    let manifests = BTreeMap::from([(
        "test-source".to_string(),
        serde_json::from_value(serde_json::json!({
            "id": "test-source", "version": "1",
            "provider": { "base": up_url, "auth": "api-key", "protocol": "anthropic" },
            "free_models": ["test-model"],
            "permissions": { "net": ["127.0.0.1"], "store_key": false }
        }))
        .unwrap(),
    )]);
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_admin_token("instance-secret".to_string())
            .with_desktop_access(),
    );
    let (client, gw, server) = spawn_gateway(state).await;
    let origin = "tauri://localhost";
    let pre = client
        .request(reqwest::Method::OPTIONS, format!("{gw}/v1/messages"))
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization, content-type",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status(), 204);
    assert_eq!(pre.headers()["access-control-allow-origin"], origin);
    assert_eq!(
        pre.headers()["access-control-allow-methods"],
        "GET, POST, DELETE"
    );
    assert_eq!(
        pre.headers()["access-control-allow-headers"],
        "authorization, content-type, x-api-key, x-goog-api-key"
    );
    assert_eq!(
        pre.headers()["access-control-expose-headers"],
        "x-aubridge-provider, x-aubridge-request-id"
    );
    let accepted = client
        .get(format!("{gw}/admin/dashboard"))
        .header("origin", origin)
        .bearer_auth("instance-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 200);
    assert_eq!(accepted.headers()["access-control-allow-origin"], origin);
    let legacy = client
        .get(format!("{gw}/admin/dashboard"))
        .header("origin", origin)
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), 401);
    let inference_key_on_admin = client
        .get(format!("{gw}/admin/dashboard"))
        .header("origin", origin)
        .bearer_auth("aubridge-ui-test")
        .send()
        .await
        .unwrap();
    assert_eq!(inference_key_on_admin.status(), 401);
    let admin_key_on_inference = client
        .get(format!("{gw}/v1/models"))
        .header("origin", origin)
        .bearer_auth("instance-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(admin_key_on_inference.status(), 401);
    let models = client
        .get(format!("{gw}/v1/models"))
        .header("origin", origin)
        .bearer_auth("aubridge-ui-test")
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    assert_eq!(models.headers()["access-control-allow-origin"], origin);
    assert!(models.json::<serde_json::Value>().await.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "test-model"));
    let inferred = client
        .post(format!("{gw}/v1/messages"))
        .header("origin", origin)
        .bearer_auth("aubridge-ui-test")
        .json(&serde_json::json!({
            "model": "test-model", "max_tokens": 512, "stream": false,
            "messages": [{"role": "user", "content": "只回复 OK。"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(inferred.status(), 200);
    assert_eq!(inferred.headers()["access-control-allow-origin"], origin);
    assert_eq!(inferred.headers()["x-aubridge-provider"], "test-source");
    assert_eq!(
        inferred.headers()["access-control-expose-headers"],
        "x-aubridge-provider, x-aubridge-request-id"
    );
    assert_eq!(
        inferred.json::<serde_json::Value>().await.unwrap()["content"][0]["text"],
        "ok"
    );
    let requests = upstream.seen_bodies.lock().await;
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["model"], "test-model");
    drop(requests);
    for origin in ["https://untrusted.example", "null"] {
        for method in [reqwest::Method::POST, reqwest::Method::OPTIONS] {
            let denied = client
                .request(method, format!("{gw}/admin/gateway"))
                .header("origin", origin)
                .bearer_auth("instance-secret")
                .json(&serde_json::json!({"serving":false}))
                .send()
                .await
                .unwrap();
            assert_eq!(denied.status(), 403);
            assert!(denied
                .headers()
                .get("access-control-allow-origin")
                .is_none());
            assert!(denied
                .headers()
                .get("access-control-expose-headers")
                .is_none());
        }
    }
    let rebound = client
        .get(format!("{gw}/admin/dashboard"))
        .header("origin", origin)
        .header("host", "rebound.example")
        .bearer_auth("instance-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), 421);
    assert!(rebound
        .headers()
        .get("access-control-expose-headers")
        .is_none());
    server.abort();
    upstream_task.abort();
}

// ---- 变更记录 32-①:Provider/keys admin 路由 + keyring 接线 ----

use credentials::KeyStore;

/// 测试临时配置目录(with_settings 的 config.json 落点)。
fn tmp_conf(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aubridge-gw-32a-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// store_key 源 manifest(key 路由测试用)。
fn key_store_manifest(id: &str, store_key: bool) -> Manifest {
    serde_json::from_value(serde_json::json!({
        "id": id, "version": "1",
        "provider": { "base": "https://api.example.dev/v1", "auth": "bearer" },
        "free_models": ["glm-4.5-flash"],
        "permissions": { "net": ["api.example.dev"], "store_key": store_key }
    }))
    .expect("manifest")
}

/// 起 app 服务器(测试内复用):返回 (地址, 服务器句柄)。
async fn serve_app(
    state: std::sync::Arc<GatewayState>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (gw, server)
}

#[tokio::test]
async fn provider_key_source_tracks_saved_environment_and_missing_keys() {
    const CHILD: &str = "AUBRIDGE_PROVIDER_KEY_SOURCE_TEST_CHILD";
    const ENV_KEY: &str = "round59-fixture-environment";
    const SAVED_KEY: &str = "round59-fixture-saved";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "provider_key_source_tracks_saved_environment_and_missing_keys",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("KEY_SOURCE_REMOTE", ENV_KEY)
            .env("KEY_SOURCE_BLANK", " \t")
            .env("KEY_CHATGPT", ENV_KEY)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "isolated test failed:\n{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        return;
    }

    let local_base = "http://127.0.0.1:1/v1";
    let mut local = key_store_manifest("source-local", false);
    local.provider.base = local_base.into();
    let manifests = BTreeMap::from([
        (
            "source-remote".into(),
            key_store_manifest("source-remote", true),
        ),
        (
            "source-blank".into(),
            key_store_manifest("source-blank", true),
        ),
        (
            "source-missing".into(),
            key_store_manifest("source-missing", true),
        ),
        ("source-local".into(), local),
        responses_manifest(local_base),
    ]);
    let dir = tmp_conf(&format!("key-source-{}", std::process::id()));
    let auth = dir.join("auth.json");
    let channel = gateway::chatgpt::ChatGptChannel::new(auth.clone(), dir.join("config.toml"))
        .with_responses_base(local_base);
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_key_store(std::sync::Arc::new(credentials::MemoryStore::default()))
            .with_codex_channel(channel),
    );
    let (gw, server) = serve_app(state.clone()).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{gw}/admin/providers");
    let unauthorized = client
        .get(&url)
        .bearer_auth("aubridge")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert!(!unauthorized.text().await.unwrap().contains("key_source"));
    let overview = || async {
        let body = client
            .get(&url)
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!body.contains(ENV_KEY));
        assert!(!body.contains(SAVED_KEY));
        serde_json::from_str::<serde_json::Value>(&body).unwrap()
    };
    let check = |overview: &serde_json::Value, id, source, key_set, ready, key: Option<&str>| {
        let row = overview["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(row["key_source"], source, "{id}");
        assert_eq!(row["key_set"], key_set, "{id}");
        assert_eq!(row["ready"], ready, "{id}");
        assert_eq!(state.api_key_opt(id).as_deref(), key, "{id}");
    };

    let initial = overview().await;
    check(
        &initial,
        "source-remote",
        "environment",
        true,
        true,
        Some(ENV_KEY),
    );
    check(
        &initial,
        "source-blank",
        "environment",
        true,
        false,
        Some(" \t"),
    );
    check(&initial, "source-missing", "none", false, false, None);
    check(&initial, "source-local", "none", false, true, None);
    check(
        &initial,
        "chatgpt",
        "environment",
        true,
        false,
        Some(ENV_KEY),
    );

    state.set_provider_key("source-remote", SAVED_KEY).unwrap();
    std::fs::write(&auth, r#"{"tokens":{"access_token":"fake-oauth","refresh_token":"fake-refresh","account_id":"fixture"}}"#).unwrap();
    let saved = overview().await;
    check(
        &saved,
        "source-remote",
        "saved",
        true,
        true,
        Some(SAVED_KEY),
    );
    check(&saved, "chatgpt", "environment", true, true, Some(ENV_KEY));

    state.clear_provider_key("source-remote").unwrap();
    check(
        &overview().await,
        "source-remote",
        "environment",
        true,
        true,
        Some(ENV_KEY),
    );
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

/// keys admin 路由(变更记录 32-①):keyring 预载 → POST 保存 →
/// 缓存即时生效(key_set 布尔翻转)→ DELETE 清除回未设置。
/// store_key=false 的源(chatgpt 通道)拒绝写 key。
#[tokio::test]
async fn keys_admin_routes_set_clear_and_reject_oauth_channel() {
    let manifests = BTreeMap::from([
        ("prov-key".to_string(), key_store_manifest("prov-key", true)),
        ("prov-pre".to_string(), key_store_manifest("prov-pre", true)),
        ("chatgpt".to_string(), key_store_manifest("chatgpt", false)),
    ]);
    // 预载语义:构造前 store 里已有的条目,启动即"已设置"。
    let store = std::sync::Arc::new(credentials::MemoryStore::default());
    store.set("prov-pre", "preloaded-key").unwrap();
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge").with_key_store(store.clone()),
    );
    let (gw, server) = serve_app(state.clone()).await;

    let client = reqwest::Client::new();
    let admin = |method: &str, path: &str, body: Option<&str>| {
        let url = format!("http://{gw}{path}");
        match (method, body) {
            ("GET", _) => client
                .get(&url)
                .header("authorization", "Bearer aubridge-admin"),
            ("POST", Some(b)) => client
                .post(&url)
                .header("authorization", "Bearer aubridge-admin")
                .body(b.to_string()),
            ("DELETE", Some(b)) => client
                .delete(&url)
                .header("authorization", "Bearer aubridge-admin")
                .body(b.to_string()),
            _ => panic!("不支持的请求形态"),
        }
    };

    // 推理 key 不能进 admin 面(§6 隔离,与 dashboard 同口径)。
    let resp = client
        .get(format!("http://{gw}/admin/providers"))
        .header("authorization", "Bearer aubridge")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 初始态:prov-pre 预载已设置,prov-key 未设置。
    let v: serde_json::Value = admin("GET", "/admin/providers", None)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = |id: &str| {
        v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(row("prov-pre")["key_set"], true);
    assert_eq!(row("prov-key")["key_set"], false);

    // POST /admin/keys:保存即生效(缓存翻转,无需重启)。
    let resp = admin(
        "POST",
        "/admin/keys",
        Some(r#"{"id":"prov-key","key":"sk-live"}"#),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    // 回执只有布尔,绝不回显 key 值(红线)。
    assert_eq!(v["set"], true);
    assert!(v.get("key").is_none());
    assert_eq!(store.get("prov-key").as_deref(), Some("sk-live"));
    assert_eq!(state.api_key_opt("prov-key").as_deref(), Some("sk-live"));

    // OAuth 通道源(store_key=false)拒绝写 key。
    let resp = admin("POST", "/admin/keys", Some(r#"{"id":"chatgpt","key":"x"}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // DELETE /admin/keys:keyring + 缓存双清。
    let resp = admin("DELETE", "/admin/keys", Some(r#"{"id":"prov-key"}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(state.api_key_opt("prov-key"), None);
    assert_eq!(
        store.get("prov-pre").as_deref(),
        Some("preloaded-key"),
        "清除不误伤别的条目"
    );

    // 未知源 400。
    let resp = admin("POST", "/admin/keys", Some(r#"{"id":"nope","key":"x"}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    server.abort();
}

/// 自定义源(变更记录 32-①):添加(校验 + 持久化 + key 同步保存)→
/// 并入候选 → 重启(with_settings 重载)还原 → 删除(preset 不可删)。
#[tokio::test]
async fn custom_provider_add_reload_and_remove() {
    let conf = tmp_conf("custom");
    let config = conf.join("config.json");
    let manifests = BTreeMap::from([("prov-a".to_string(), key_store_manifest("prov-a", true))]);
    let store = std::sync::Arc::new(credentials::MemoryStore::default());
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_settings(config.clone(), conf.join("stash"))
            .with_key_store(store.clone()),
    );
    let (gw, server) = serve_app(state.clone()).await;

    let client = reqwest::Client::new();
    let admin = client.clone();
    let _ = admin; // 与 keys 测试不同,这里直接串行调用更贴现有风格。

    // 添加:base + models + key 同一步(ui-design §6.2 卡片流)。
    let resp = client
        .post(format!("http://{gw}/admin/providers"))
        .header("authorization", "Bearer aubridge-admin")
        .body(
            r#"{"id":"my-glm","base":"https://api.my-glm.dev/v1",
                "models":["glm-4.5-flash"],"key":"sk-custom"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // key 与源同步保存(自定义源 store_key 恒 true)。
    assert_eq!(store.get("my-glm").as_deref(), Some("sk-custom"));
    // 并入候选:manifests/models 即时可见。
    assert!(state.manifest("my-glm").is_some());
    assert!(state.models().contains(&"glm-4.5-flash".to_string()));
    assert!(state.key_set("my-glm"));

    // 持久化:config.json 字段落盘(重启还原依据)。
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["providers"][0]["id"], "my-glm");

    // 重复 id 拒绝(与 preset 撞名同样拒绝)。
    let resp = client
        .post(format!("http://{gw}/admin/providers"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"id":"my-glm","base":"https://api.my-glm.dev/v1","models":["m"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("http://{gw}/admin/providers"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"id":"prov-a","base":"https://api.x.dev","models":["m"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 坏输入 400 且错误可读(registry::load 校验口径)。
    let resp = client
        .post(format!("http://{gw}/admin/providers"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"id":"BAD_ID","base":"https://api.x.dev","models":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: serde_json::Value = resp.json().await.unwrap();
    // registry::load 校验口径:逐条可读(kebab-case + models 非空)。
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("kebab-case"), "err = {err}");
    assert!(err.contains("free_models"), "err = {err}");

    // 重启还原:同一 config.json 经 with_settings 重载,自定义源
    // 回到候选(变更记录 32-① 装载路径)。
    let reloaded = GatewayState::from_manifests(BTreeMap::new(), "aubridge")
        .with_settings(config.clone(), conf.join("stash"));
    assert!(reloaded.manifest("my-glm").is_some());
    let overview = reloaded.providers_overview();
    let row = overview
        .iter()
        .find(|p| p["id"] == "my-glm")
        .cloned()
        .unwrap();
    assert_eq!(row["source"], "custom");

    // preset 不可删(只有 custom 可删)。
    let resp = client
        .delete(format!("http://{gw}/admin/providers/prov-a"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 删除 custom:运行态 + 持久化 + keyring 三清。
    let resp = client
        .delete(format!("http://{gw}/admin/providers/my-glm"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(state.manifest("my-glm").is_none());
    assert_eq!(store.get("my-glm"), None, "keyring 残留清理");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert!(saved["providers"].as_array().unwrap().is_empty());

    server.abort();
}

/// 自动虚拟模型 "aubridge"(变更记录 32-②):请求模型 = 内置别名
/// 时不按模型过滤候选(全部健康启用源),A 失败 429 → B 应答,上游
/// 收到的 model 字段改写成 B 的首个声明模型(不是别名原文)。
#[tokio::test]
async fn auto_model_fails_over_and_rewrites_to_first_declared_model() {
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (rb, b_url, _rb) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;

    // 两源供完全不同的模型:普通请求在两源都无候选,只有自动别名
    // 才能把两者拉进候选。
    let mk = |id: &str, base: &str, model: &str| -> (String, Manifest) {
        (
            id.to_string(),
            serde_json::from_value(serde_json::json!({
                "id": id, "version": "1",
                "provider": { "base": base, "auth": "bearer" },
                "free_models": [model],
                "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("preset json"),
        )
    };
    let manifests = BTreeMap::from([mk("prov-a", &a_url, "m-a"), mk("prov-b", &b_url, "m-b")]);
    let state = std::sync::Arc::new(GatewayState::from_manifests(manifests, "aubridge"));
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{gw_addr}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"aubridge","max_tokens":100,
                "messages":[{"role":"user","content":"weather?"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    let body = resp.text().await.unwrap();
    assert!(body.contains("message_stop"));

    // 上游收到的是 B 的首个声明模型(别名改写,非 "aubridge" 原文)。
    let seen = rb.seen_bodies.lock().await;
    assert_eq!(seen.len(), 1, "B 只收到一次: {seen:?}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&seen[0]).expect("b body json")["model"] == "m-b",
        "B 应收到其首个声明模型: {}",
        seen[0]
    );

    server.abort();
}

/// admin GET 便捷封装(model-groups 测试用)。
async fn get_admin(client: &reqwest::Client, gw: &str, path: &str) -> serde_json::Value {
    client
        .get(format!("{gw}{path}"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// 命名分组 store(变更记录 32-③):POST 落盘 config.json 字段级
/// 合并 + 运行态生效;GET 列表;保留字/未知成员/未知规则 400;
/// DELETE 移除;models() 并入分组名。
#[tokio::test]
async fn model_groups_admin_crud_and_persistence() {
    let conf = tmp_conf("groups");
    let config = conf.join("config.json");

    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "id": "prov", "version": "1",
        "provider": { "base": "http://127.0.0.1:1", "auth": "bearer" },
        "free_models": ["m-a", "m-b"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("preset json");
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(BTreeMap::from([("prov".to_string(), manifest)]), "aubridge")
            .with_settings(config.clone(), conf.join("stash")),
    );
    let (gw_addr, server) = serve_app(state.clone()).await;
    let gw = format!("http://{gw_addr}");
    let client = reqwest::Client::new();

    // 未写前:空表。
    let resp = client
        .get(format!("{gw}/admin/model-groups"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["groups"].as_array().unwrap().len(), 0);

    // 无 admin key → 401。
    let resp = client
        .get(format!("{gw}/admin/model-groups"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // POST:缺省 rule 默认 free_first(serde default),成员校验过。
    let resp = client
        .post(format!("{gw}/admin/model-groups"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"name":"my-fallback","members":["m-a","m-b"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["name"], "my-fallback");

    // 落盘:config.json model_groups 数组,字段级合并保留其他键。
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        saved["model_groups"][0]["name"], "my-fallback",
        "saved = {saved}"
    );
    assert_eq!(saved["model_groups"][0]["rule"], "free_first");

    // GET 列表回显;models() 并入分组名(下拉/v1/models 共用)。
    let v: serde_json::Value = get_admin(&client, &gw, "/admin/model-groups").await;
    assert_eq!(v["groups"].as_array().unwrap().len(), 1);
    let models = state.models();
    assert!(
        models.iter().any(|m| m == "my-fallback"),
        "models = {models:?}"
    );

    // upsert:同名覆盖(改规则 + 成员)。
    let resp = client
        .post(format!("{gw}/admin/model-groups"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"name":"my-fallback","members":["m-b"],"rule":"sequential"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = get_admin(&client, &gw, "/admin/model-groups").await;
    assert_eq!(v["groups"].as_array().unwrap().len(), 1);
    assert_eq!(v["groups"][0]["rule"], "sequential");

    // 校验 400:保留字名(group/ 前缀、AUTO_MODEL)、未知成员、未知规则。
    for body in [
        r#"{"name":"group/x","members":["m-a"]}"#,
        r#"{"name":"aubridge","members":["m-a"]}"#,
        r#"{"name":"g1","members":["no-such-model"]}"#,
        r#"{"name":"g2","members":["m-a"],"rule":"intent"}"#,
        r#"{"name":"","members":["m-a"]}"#,
        r#"{"name":"g3","members":[]}"#,
    ] {
        let resp = client
            .post(format!("{gw}/admin/model-groups"))
            .header("authorization", "Bearer aubridge-admin")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body = {body}");
    }

    // DELETE:200 + 落盘清空 + 运行态移除。
    let resp = client
        .delete(format!("{gw}/admin/model-groups/my-fallback"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = get_admin(&client, &gw, "/admin/model-groups").await;
    assert_eq!(v["groups"].as_array().unwrap().len(), 0);
    assert!(!state.models().iter().any(|m| m == "my-fallback"));

    // DELETE 不存在 → 400。
    let resp = client
        .delete(format!("{gw}/admin/model-groups/my-fallback"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 未接线 settings(model_groups_overview None)→ 400。
    let bare = std::sync::Arc::new(GatewayState::from_manifests(BTreeMap::new(), "aubridge"));
    let (gw_addr2, server2) = serve_app(bare).await;
    let resp = client
        .get(format!("http://{gw_addr2}/admin/model-groups"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    server.abort();
    server2.abort();
}

/// 命名分组转发(变更记录 32-③):请求分组名 → 内联语义解析失败
/// (非 group/ 前缀)→ 查表命中 → 按成员改写上游 model 为首个声明
/// 成员;重启(with_settings 重载)分组还原继续可路由。
#[tokio::test]
async fn named_group_routing_rewrites_member_model_and_survives_reload() {
    let (_ra, a_url, _a) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"type":"quota_exceeded","remaining":0}}"#.into(),
    }])
    .await;
    let (rb, b_url, _rb) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;

    let mk = |id: &str, base: &str, model: &str| -> (String, Manifest) {
        (
            id.to_string(),
            serde_json::from_value(serde_json::json!({
                "id": id, "version": "1",
                "provider": { "base": base, "auth": "bearer" },
                "free_models": [model],
                "quota": { "source": "inferred", "rest_hint": "daily-utc" },
                "permissions": { "net": ["127.0.0.1"], "store_key": false }
            }))
            .expect("preset json"),
        )
    };
    let manifests = BTreeMap::from([mk("prov-a", &a_url, "m-a"), mk("prov-b", &b_url, "m-b")]);

    let conf = tmp_conf("groups-route");
    let config = conf.join("config.json");
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_settings(config.clone(), conf.join("stash")),
    );
    let (gw_addr, server) = serve_app(state.clone()).await;
    let gw = format!("http://{gw_addr}");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gw}/admin/model-groups"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"name":"dual","members":["m-a","m-b"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 请求分组名:A 429 → B 应答,B 收到成员 m-b 改写后的 model。
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(
            r#"{"model":"dual","max_tokens":100,
                "messages":[{"role":"user","content":"weather?"}]}"#,
        )
        .send()
        .await
        .expect("gateway request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-aubridge-provider").unwrap(), "prov-b");
    let seen = rb.seen_bodies.lock().await;
    assert_eq!(seen.len(), 1, "B 只收到一次: {seen:?}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&seen[0]).expect("b body json")["model"] == "m-b",
        "B 应收到成员字面模型: {}",
        seen[0]
    );

    // 重启还原:同一 config.json 经 with_settings 重载,分组回
    // 运行态(group_of 命中,变更记录 32-③ 装载路径)。
    let reloaded = GatewayState::from_manifests(BTreeMap::new(), "aubridge")
        .with_settings(config.clone(), conf.join("stash"));
    assert!(reloaded.group_of("dual").is_some());

    server.abort();
}

/// 代理页后端(变更记录 32-④):软开关 off → 推理路径 503(进程
/// 不退,admin 面照常);再开恢复转发;GET /admin/gateway 回接入
/// 方式 + 历史请求;无 admin key 401。
#[tokio::test]
async fn gateway_serving_switch_and_access_info() {
    let (rb, b_url, _rb) = spawn(vec![Step::Sse {
        body: CHAT_TOOL_CALL.into(),
    }])
    .await;
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "id": "prov-b", "version": "1",
        "provider": { "base": b_url, "auth": "bearer" },
        "free_models": ["m-b"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("preset json");
    let state = std::sync::Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("prov-b".to_string(), manifest)]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (gw_addr, server) = serve_app(state.clone()).await;
    // serve_app 直绑不走 serve();模拟 serve() 的端口回写(§4)。
    state.set_port(gw_addr.port());
    let gw = format!("http://{gw_addr}");
    let client = reqwest::Client::new();
    let body = r#"{"model":"m-b","max_tokens":100,
        "messages":[{"role":"user","content":"weather?"}]}"#;

    // 基线:开着,正常转发。
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // GET /admin/gateway:接入方式 + 开关默认开 + 历史请求含刚转发的行。
    let resp = client
        .get(format!("{gw}/admin/gateway"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["serving"], true);
    assert_eq!(
        v["endpoint"],
        format!("http://127.0.0.1:{}", gw_addr.port())
    );
    assert_eq!(v["admin_token"], "aubridge-admin");
    assert!(
        !v["requests"].as_array().unwrap().is_empty(),
        "历史请求行: {v}"
    );

    // POST off → 200;推理路径 503(不裸断连,admin 面照常)。
    let resp = client
        .post(format!("{gw}/admin/gateway"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"serving":false}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["serving"], false);

    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("暂停"));

    let resp = client
        .get(format!("{gw}/admin/gateway"))
        .header("authorization", "Bearer aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "off 时 admin 面照常");

    // 无 admin key → 401(GET/POST)。
    let resp = client
        .get(format!("{gw}/admin/gateway"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .post(format!("{gw}/admin/gateway"))
        .body(r#"{"serving":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 再开 → 恢复转发(上游又收到一发)。
    let resp = client
        .post(format!("{gw}/admin/gateway"))
        .header("authorization", "Bearer aubridge-admin")
        .body(r#"{"serving":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .post(format!("{gw}/v1/messages"))
        .header("authorization", "Bearer aubridge")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = rb.seen_bodies.lock().await;
    assert_eq!(seen.len(), 2, "off 期间零外发: {seen:?}");

    server.abort();
}

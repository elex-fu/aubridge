//! agent 接线 admin 面验收(§8.6,变更记录 30 ⑥):临时路径注入,
//! wire → 状态 → unwire 全往返、用户字段保留、未知 agent 400、
//! 无 admin key 401、未注入空表。

use std::collections::BTreeMap;

use agents::AgentsPaths;
use gateway::{app, state::GatewayState};
use manifest::schema::Manifest;

const ADMIN: &str = "Bearer aubridge-admin";

fn tmp_paths(tag: &str) -> AgentsPaths {
    let dir = std::env::temp_dir().join(format!("aubridge-gw-agents-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    AgentsPaths {
        claude_settings: dir.join("claude").join("settings.json"),
        codex_config: dir.join("codex").join("config.toml"),
        opencode_config: dir.join("opencode").join("opencode.json"),
        gemini_settings: dir.join("gemini").join("settings.json"),
        gemini_env: dir.join("gemini").join(".env"),
        state_dir: dir.join("state"),
    }
}

fn state_with_agents(paths: &AgentsPaths) -> std::sync::Arc<GatewayState> {
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "id": "prov", "version": "1",
        "provider": { "base": "http://127.0.0.1:1", "auth": "bearer" },
        "free_models": ["glm-4.5-flash"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("preset json");
    std::sync::Arc::new(
        GatewayState::from_manifests(BTreeMap::from([("prov".to_string(), manifest)]), "aubridge")
            .with_agents(AgentsPaths {
                claude_settings: paths.claude_settings.clone(),
                codex_config: paths.codex_config.clone(),
                opencode_config: paths.opencode_config.clone(),
                gemini_settings: paths.gemini_settings.clone(),
                gemini_env: paths.gemini_env.clone(),
                state_dir: paths.state_dir.clone(),
            }),
    )
}

async fn spawn(state: std::sync::Arc<GatewayState>) -> String {
    let gw_app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, gw_app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn get_json(client: &reqwest::Client, gw: &str, path: &str) -> reqwest::Response {
    client
        .get(format!("{gw}{path}"))
        .header("authorization", ADMIN)
        .send()
        .await
        .unwrap()
}

async fn toggle(client: &reqwest::Client, gw: &str, id: &str, on: bool) -> reqwest::Response {
    client
        .post(format!("{gw}/admin/agents/{id}/enable"))
        .header("authorization", ADMIN)
        .body(format!("{{\"on\":{on}}}"))
        .send()
        .await
        .unwrap()
}

async fn wired_flags(
    client: &reqwest::Client,
    gw: &str,
) -> Vec<(String, bool, bool, Option<String>)> {
    let v: serde_json::Value = get_json(client, gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["id"].as_str().unwrap().to_string(),
                a["wired"].as_bool().unwrap(),
                a["detected"].as_bool().unwrap(),
                a["drift"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// 全往返:四 agent wire → 状态在位 → unwire → 还原 + 用户字段保留。
#[tokio::test]
async fn wire_unwire_roundtrip_all_agents() {
    let paths = tmp_paths("roundtrip");
    // 用户已有内容:接线不得覆盖。
    std::fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(&paths.claude_settings, r#"{"model":"sonnet"}"#).unwrap();
    std::fs::create_dir_all(paths.codex_config.parent().unwrap()).unwrap();
    std::fs::write(&paths.codex_config, "model = \"gpt-5.1\"\n").unwrap();
    std::fs::create_dir_all(paths.opencode_config.parent().unwrap()).unwrap();
    std::fs::write(&paths.opencode_config, r#"{"theme":"dark"}"#).unwrap();
    std::fs::create_dir_all(paths.gemini_settings.parent().unwrap()).unwrap();
    std::fs::write(&paths.gemini_settings, r#"{"theme":"dark"}"#).unwrap();

    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    let before = wired_flags(&client, &gw).await;
    assert_eq!(before.len(), 4);
    assert!(
        before.iter().all(|(_, w, d, _)| !*w && *d),
        "种子配置在,未接线"
    );
    let initial: serde_json::Value = get_json(&client, &gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    assert!(
        initial["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| { agent["discovery"]["status"] == "not_checked" }),
        "已有配置不代表已检查CLI可执行文件或版本"
    );

    for (id, _, _, _) in &before {
        let resp = toggle(&client, &gw, id, true).await;
        assert_eq!(resp.status(), 200, "{id} wire");
    }
    let wired = wired_flags(&client, &gw).await;
    assert!(wired.iter().all(|(_, w, _, _)| *w), "接线后全部在位");
    // codex 的残余动作提示要透出(变更记录 30 ① hint)。
    let agents_json: serde_json::Value = get_json(&client, &gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    let codex = agents_json["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "codex")
        .unwrap();
    assert_eq!(codex["hint"].as_str().unwrap(), "codex --profile aubridge");

    // claude 接线值落地检查(base_url 指网关默认端口 3427)。
    let cc = std::fs::read_to_string(&paths.claude_settings).unwrap();
    assert!(cc.contains("http://127.0.0.1:3427"));
    assert!(cc.contains("aubridge-claude-code"));
    assert!(cc.contains("sonnet"), "用户字段保留");

    for (id, _, _, _) in &wired {
        let resp = toggle(&client, &gw, id, false).await;
        assert_eq!(resp.status(), 200, "{id} unwire");
    }
    let after = wired_flags(&client, &gw).await;
    assert!(after.iter().all(|(_, w, _, _)| !*w), "断开后全部还原");

    // 用户文件还原(接线字段消失,原有内容保留)。
    let cc = std::fs::read_to_string(&paths.claude_settings).unwrap();
    assert!(cc.contains("sonnet"));
    assert!(!cc.contains("ANTHROPIC_BASE_URL"));
    let codex = std::fs::read_to_string(&paths.codex_config).unwrap();
    assert!(codex.contains("gpt-5.1"));
    assert!(!codex.contains("[model_providers.aubridge]"));
    assert!(!paths
        .codex_config
        .with_file_name("aubridge.config.toml")
        .exists());
    let oc = std::fs::read_to_string(&paths.opencode_config).unwrap();
    assert!(oc.contains("dark"));
    let gem = std::fs::read_to_string(&paths.gemini_settings).unwrap();
    assert!(gem.contains("dark"));
}

/// drift:第三方改写接线字段 → 状态回 replaced。
#[tokio::test]
async fn drift_reports_replaced_after_third_party_edit() {
    let paths = tmp_paths("drift");
    std::fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(&paths.claude_settings, "{}").unwrap();

    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();
    let resp = toggle(&client, &gw, "claude-code", true).await;
    assert_eq!(resp.status(), 200);

    let flags = wired_flags(&client, &gw).await;
    let (_, _, _, drift) = flags
        .iter()
        .find(|(id, _, _, _)| id == "claude-code")
        .unwrap();
    assert_eq!(drift.as_deref(), Some("clean"));

    // 第三方改写 base_url。
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.claude_settings).unwrap()).unwrap();
    v["env"]["ANTHROPIC_BASE_URL"] = serde_json::json!("http://elsewhere:9999");
    std::fs::write(&paths.claude_settings, serde_json::to_string(&v).unwrap()).unwrap();

    let flags = wired_flags(&client, &gw).await;
    let (wired, drift) = flags
        .iter()
        .find(|(id, _, _, _)| id == "claude-code")
        .map(|(_, w, _, d)| (*w, d.clone()))
        .unwrap();
    assert!(!wired, "接线值被改 → 不在位");
    assert_eq!(drift.as_deref(), Some("replaced"));
}

#[tokio::test]
async fn disconnect_preview_is_private_and_blocks_stale_confirmation() {
    let paths = tmp_paths("preview");
    std::fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(
        &paths.claude_settings,
        r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"saved-token-for-test","CUSTOM":"keep"}}"#,
    )
    .unwrap();
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();
    assert_eq!(
        toggle(&client, &gw, "claude-code", true).await.status(),
        200
    );
    let path = "/admin/agents/claude-code/disconnect-preview";
    assert_eq!(
        client
            .get(format!("{gw}{path}"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let preview: serde_json::Value = get_json(&client, &gw, path).await.json().await.unwrap();
    assert_eq!(preview["changes"][0]["action"], "remove");
    assert_eq!(preview["changes"][1]["action"], "restore");
    assert!(!preview.to_string().contains("saved-token-for-test"));
    let mut current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&paths.claude_settings).unwrap()).unwrap();
    current["env"]["ANTHROPIC_AUTH_TOKEN"] = serde_json::json!("new-foreign-token");
    std::fs::write(&paths.claude_settings, current.to_string()).unwrap();
    let response = client
        .post(format!("{gw}/admin/agents/claude-code/enable"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({"on": false, "preview": preview}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        std::fs::read_to_string(&paths.claude_settings).unwrap(),
        current.to_string()
    );
}

/// 未知 agent → 400;无 admin key → 401。
#[tokio::test]
async fn unknown_agent_400_and_missing_admin_key_401() {
    let paths = tmp_paths("auth");
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    let resp = toggle(&client, &gw, "no-such-agent", true).await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("未知 agent"));

    // 无 admin key:GET/POST 都 401。
    let resp = client
        .get(format!("{gw}/admin/agents"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .post(format!("{gw}/admin/agents/codex/enable"))
        .body(r#"{"on":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // 未触发任何写(401 先于接线)。
    assert!(!paths
        .codex_config
        .with_file_name("aubridge.config.toml")
        .exists());
    assert!(
        !paths.codex_config.exists()
            || !std::fs::read_to_string(&paths.codex_config)
                .unwrap()
                .contains("aubridge")
    );
}

/// 未注入路径 → configured:false + 空表(与设置页"未配置"同口径)。
#[tokio::test]
async fn no_injection_yields_unconfigured_empty_list() {
    let state = std::sync::Arc::new(GatewayState::from_manifests(BTreeMap::new(), "aubridge"));
    let gw = spawn(state).await;
    let client = reqwest::Client::new();
    let v: serde_json::Value = get_json(&client, &gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["configured"], false);
    assert_eq!(v["agents"].as_array().unwrap().len(), 0);

    // 未注入时开关 → 400 未配置。
    let resp = toggle(&client, &gw, "codex", true).await;
    assert_eq!(resp.status(), 400);
}

/// serve() 端口回退:实际端口回写 state(接线 base_url 用实际值,
/// 变更记录 30 ②)。spawn 这里用 app() 直绑,不走 serve();另以
/// serve() 全路径验证。
#[tokio::test]
async fn serve_writes_back_actual_port() {
    let paths = tmp_paths("port");
    let state = state_with_agents(&paths);
    // 占住 3427(若空闲)逼 serve 走回退;若已被占也照样回退。
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:3427").await;
    if let Ok(b) = blocker {
        let _b = b; // 持有到测试结束
        assert_eq!(state.port(), 3427, "serve 前是默认值");
        let actual = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
        let seen = actual.clone();
        let handle = tokio::spawn(gateway::serve(state.clone(), 3427, move |p| {
            seen.store(p, std::sync::atomic::Ordering::SeqCst)
        }));
        // 等绑定回调跑完(轮询上限 2s)。
        for _ in 0..200 {
            if actual.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let bound = actual.load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(bound, 0, "on_bound 应已回调");
        assert_ne!(bound, 3427, "回退端口不等于被占端口");
        assert_eq!(state.port(), bound, "state 应回写实际端口");
        handle.abort();
    } else {
        // 3427 被外部进程占用:serve 同样回退,只验证回写 ≠ 3427。
        assert_eq!(state.port(), 3427);
        let moved = state.clone();
        let handle = tokio::spawn(gateway::serve(state.clone(), 3427, move |p| {
            moved.set_port(p)
        }));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_ne!(state.port(), 3427, "回退端口不等于默认端口");
        handle.abort();
    }
}

/// 模型路由(变更记录 32-②):POST 设置写 agent 配置文件;自动
/// 别名置首 models 列表;overview 每行带 model/token(编辑弹层
/// 数据源);未知模型/agent 400。
#[tokio::test]
async fn agent_model_route_sets_and_overview_carries_fields() {
    let paths = tmp_paths("model");
    std::fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(&paths.claude_settings, r#"{"env":{"CUSTOM":"keep"}}"#).unwrap();
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    // 设置自动模型 → 200,落盘到 settings.json 顶层 model。
    let resp = client
        .post(format!("{gw}/admin/agents/claude-code/model"))
        .header("authorization", ADMIN)
        .body(r#"{"model":"aubridge"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["model"], "aubridge");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.claude_settings).unwrap()).unwrap();
    assert_eq!(v["model"], "aubridge", "写顶层 model 字段");
    assert_eq!(v["env"]["CUSTOM"], "keep", "用户字段保留");

    // overview:model/token 透出(token = aubridge-<id>,可复制)。
    let v: serde_json::Value = get_json(&client, &gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    let claude = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "claude-code")
        .unwrap();
    assert_eq!(claude["model"], "aubridge");
    assert_eq!(claude["token"], "aubridge-claude-code");
    // 模型下拉:自动别名置首,聚合模型在后。
    let models = v["models"].as_array().unwrap();
    assert_eq!(models[0], "aubridge");
    assert!(models.iter().any(|m| m == "glm-4.5-flash"));

    // 未知模型 → 400;未知 agent → 400。
    let resp = client
        .post(format!("{gw}/admin/agents/claude-code/model"))
        .header("authorization", ADMIN)
        .body(r#"{"model":"no-such-model"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("未知模型"));
    let resp = client
        .post(format!("{gw}/admin/agents/no-such-agent/model"))
        .header("authorization", ADMIN)
        .body(r#"{"model":"aubridge"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// OpenCode 默认模型必须先接线，再通过显式选择写入。
#[tokio::test]
async fn agent_model_for_opencode_requires_wiring_and_writes_default() {
    let paths = tmp_paths("model-opencode");
    std::fs::create_dir_all(paths.opencode_config.parent().unwrap()).unwrap();
    std::fs::write(&paths.opencode_config, r#"{"theme":"dark"}"#).unwrap();
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{gw}/admin/agents/opencode/model"))
        .header("authorization", ADMIN)
        .body(r#"{"model":"glm-4.5-flash"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let oc = std::fs::read_to_string(&paths.opencode_config).unwrap();
    assert_eq!(oc, r#"{"theme":"dark"}"#);
    assert!(!paths.state_dir.join("opencode.stash.json").exists());

    assert_eq!(toggle(&client, &gw, "opencode", true).await.status(), 200);
    let resp = client
        .post(format!("{gw}/admin/agents/opencode/model"))
        .header("authorization", ADMIN)
        .body(r#"{"model":"glm-4.5-flash"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let oc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&paths.opencode_config).unwrap()).unwrap();
    assert_eq!(oc["model"], "aubridge/glm-4.5-flash");
    assert_eq!(oc["theme"], "dark");
    let overview: serde_json::Value = get_json(&client, &gw, "/admin/agents")
        .await
        .json()
        .await
        .unwrap();
    let opencode = overview["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == "opencode")
        .unwrap();
    assert_eq!(opencode["model"], "glm-4.5-flash");
}

/// Claude Code 高级配置:密钥打码、角色映射、revision 乐观锁。
#[tokio::test]
async fn claude_advanced_config_masks_secrets_and_guards_revision() {
    let paths = tmp_paths("advanced-claude");
    std::fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
    std::fs::write(
        &paths.claude_settings,
        serde_json::json!({
            "model": "glm-5.3[1M]",
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "secret-token",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.3[1M]",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "glm-5.3",
            }
        })
        .to_string(),
    )
    .unwrap();
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{gw}/admin/agents/claude-code/config"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    let view: serde_json::Value = get_json(&client, &gw, "/admin/agents/claude-code/config")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(view["secrets_masked"], true);
    assert_eq!(view["config"]["env"]["ANTHROPIC_AUTH_TOKEN"], "••••••••");
    assert_eq!(view["roles"]["sonnet"]["one_million"], true);
    assert_eq!(view["roles"]["sonnet"]["name"], "glm-5.3");
    assert!(view["test"]["command"]
        .as_str()
        .unwrap()
        .contains("--no-session-persistence"));
    let revision = view["revision"].as_str().unwrap().to_string();

    let mut config = view["config"].clone();
    config["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"] = serde_json::json!("next-model");
    config["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"] = serde_json::json!("next-model");
    let resp = client
        .post(format!("{gw}/admin/agents/claude-code/config"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({"revision": revision, "config": config}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&paths.claude_settings).unwrap()).unwrap();
    assert_eq!(saved["env"]["ANTHROPIC_AUTH_TOKEN"], "secret-token");
    assert_eq!(saved["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "next-model");

    let stale = client
        .post(format!("{gw}/admin/agents/claude-code/config"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({"revision": revision, "config": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 400);
    assert!(stale.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("修改"));

    let fresh: serde_json::Value = get_json(&client, &gw, "/admin/agents/claude-code/config")
        .await
        .json()
        .await
        .unwrap();
    let invalid = client
        .post(format!("{gw}/admin/agents/claude-code/config"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({
            "revision": fresh["revision"],
            "config": {"env": {"NUMBER": 1}},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(invalid.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("字符串"));
}

/// Codex 高级配置:只改受管 profile 顶层字段,provider 表保持不变。
#[tokio::test]
async fn codex_advanced_config_round_trips_profile_fields() {
    let paths = tmp_paths("advanced-codex");
    std::fs::create_dir_all(paths.codex_config.parent().unwrap()).unwrap();
    std::fs::write(&paths.codex_config, "model = \"gpt-5.1\"\n").unwrap();
    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();
    assert_eq!(toggle(&client, &gw, "codex", true).await.status(), 200);

    let view: serde_json::Value = get_json(&client, &gw, "/admin/agents/codex/config")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(view["wired"], true);
    assert_eq!(view["config"]["model"], "aubridge");
    let revision = view["revision"].as_str().unwrap().to_string();
    let mut config = view["config"].clone();
    config["model"] = serde_json::json!("gpt-6.1-sol");
    config["model_reasoning_effort"] = serde_json::json!("high");
    config["model_context_window"] = serde_json::json!(400000);

    let resp = client
        .post(format!("{gw}/admin/agents/codex/config"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({"revision": revision, "config": config}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let profile =
        std::fs::read_to_string(paths.codex_config.with_file_name("aubridge.config.toml")).unwrap();
    assert!(profile.contains("model = \"gpt-6.1-sol\""));
    assert!(profile.contains("model_reasoning_effort = \"high\""));
    assert!(profile.contains("model_context_window = 400000"));
    assert!(profile.contains("experimental_bearer_token = \"aubridge-codex\""));

    let after: serde_json::Value = get_json(&client, &gw, "/admin/agents/codex/config")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after["config"]["model"], "gpt-6.1-sol");
    assert_eq!(after["config"]["model_reasoning_effort"], "high");

    let stale = client
        .post(format!("{gw}/admin/agents/codex/config"))
        .header("authorization", ADMIN)
        .json(&serde_json::json!({"revision": revision, "config": {"model": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 400);
    assert!(stale.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("修改"));
}

/// 测试按钮执行固定命令并返回输出;二进制可用环境变量覆盖(便于
/// 非标准安装路径,也让验收不必依赖真实 CLI)。
#[tokio::test]
async fn agent_test_route_runs_the_fixed_command() {
    use std::os::unix::fs::PermissionsExt;

    let paths = tmp_paths("agent-test");
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let script = paths.state_dir.join("fake-claude.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf '收到\\n'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let previous = std::env::var_os("AUBRIDGE_CLAUDE_BIN");
    std::env::set_var("AUBRIDGE_CLAUDE_BIN", &script);

    let gw = spawn(state_with_agents(&paths)).await;
    let client = reqwest::Client::new();

    let unwired = client
        .post(format!("{gw}/admin/agents/claude-code/test"))
        .header("authorization", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(unwired.status(), 400);
    assert!(unwired.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("未接线"));

    assert_eq!(
        toggle(&client, &gw, "claude-code", true).await.status(),
        200
    );

    let result = client
        .post(format!("{gw}/admin/agents/claude-code/test"))
        .header("authorization", ADMIN)
        .send()
        .await
        .unwrap();

    match previous {
        Some(value) => std::env::set_var("AUBRIDGE_CLAUDE_BIN", value),
        None => std::env::remove_var("AUBRIDGE_CLAUDE_BIN"),
    }

    assert_eq!(result.status(), 200);
    let body: serde_json::Value = result.json().await.unwrap();
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout"], "收到\n");
    assert!(body["message"].as_str().unwrap().contains("通过"));

    let missing = client
        .post(format!("{gw}/admin/agents/no-such-agent/test"))
        .header("authorization", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
}

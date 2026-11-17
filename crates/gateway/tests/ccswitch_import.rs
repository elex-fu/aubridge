//! cc-switch 导入集成测试(cc-switch-import-design §6):
//! 临时 SQLite + 真回环 admin 路由,验证 GET 不回显 key、POST 落
//! config.json(字段级合并不抹既有字段)与 keyring、逐行 added/failed。

use std::collections::BTreeMap;
use std::sync::Arc;

use credentials::KeyStore;
use gateway::state::GatewayState;
use serde_json::{json, Value};

const CLAUDE_LONGCAT: &str = r#"{
    "env": {
        "ANTHROPIC_AUTH_TOKEN": "fixture-claude-token",
        "ANTHROPIC_BASE_URL": "https://api.longcat.chat/anthropic",
        "ANTHROPIC_MODEL": "LongCat-2.0",
        "ANTHROPIC_SMALL_FAST_MODEL": "LongCat-2.0"
    }
}"#;

const CODEX_LONGCAT: &str = r#"{
    "auth": {"OPENAI_API_KEY": "fixture-codex-key"},
    "config": "model_provider = \"custom\"\nmodel = \"LongCat-2.0\"\n\n[model_providers.custom]\nbase_url = \"https://api.longcat.chat/openai/v1\"\nwire_api = \"responses\"\n"
}"#;

fn db(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aubridge-ccswitch-gw-{tag}-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "CREATE TABLE providers (
                id TEXT NOT NULL, app_type TEXT NOT NULL, name TEXT NOT NULL,
                settings_config TEXT NOT NULL,
                created_at INTEGER, sort_index INTEGER,
                PRIMARY KEY (id, app_type))",
            [],
        )
        .unwrap();
    for (i, (id, app_type, name, settings)) in [
        ("0a77", "claude", "Longcat", CLAUDE_LONGCAT),
        ("7ee9", "codex", "Longcat", CODEX_LONGCAT),
        (
            "fe2d",
            "claude",
            "Kimi For Coding",
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"k","ANTHROPIC_BASE_URL":"http://127.0.0.1:62760"}}"#,
        ),
        ("official", "claude", "Claude Official", r#"{"env":{}}"#),
    ]
    .into_iter()
    .enumerate()
    {
        connection
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config, created_at, sort_index)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, app_type, name, settings, i as i64, i as i64],
            )
            .unwrap();
    }
    path
}

async fn serve(state: Arc<GatewayState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = gateway::app(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, task)
}

#[tokio::test]
async fn get_lists_candidates_without_key_and_post_imports_selected_refs() {
    let path = db("roundtrip");
    let dir = std::env::temp_dir().join(format!("aubridge-ccswitch-gw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    // 既有共享字段:导入写回不得抹掉(字段级合并红线)。
    std::fs::write(&config, r#"{"port":3427}"#).unwrap();
    let store = Arc::new(credentials::MemoryStore::default());
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_admin_token("test-admin".into())
            .with_cc_switch_db(path.clone())
            .with_settings(config.clone(), dir.join("stash"))
            .with_key_store(store.clone())
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, server) = serve(state.clone()).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // GET:候选列表,key 永不回显。
    let body: Value = client
        .get(format!("{base}/admin/import/cc-switch"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 4);
    let text = serde_json::to_string(&body).unwrap();
    assert!(!text.contains("fixture-claude-token"), "key 不得回显");
    assert!(!text.contains("fixture-codex-key"), "key 不得回显");
    let find = |reference: &str| {
        items
            .iter()
            .find(|item| item["reference"] == reference)
            .unwrap()
            .clone()
    };
    // claude 行在前(sort):Longcat(claude)拿裸 slug,codex 行带后缀。
    let longcat = find("claude/0a77");
    assert_eq!(longcat["provider"]["id"], "longcat");
    assert_eq!(longcat["provider"]["protocol"], "anthropic");
    let codex = find("codex/7ee9");
    assert_eq!(codex["provider"]["id"], "longcat-codex");
    assert_eq!(codex["provider"]["protocol"], "responses");
    // 回环行与官方行:skip 带原因。
    assert!(find("claude/fe2d")["skip"]
        .as_str()
        .unwrap()
        .contains("自环"));
    assert!(find("claude/official")["skip"]
        .as_str()
        .unwrap()
        .contains("官方登录"));

    // POST:勾选即新增(两条 Longcat)。
    let result: Value = client
        .post(format!("{base}/admin/import/cc-switch"))
        .bearer_auth("test-admin")
        .json(&json!({"refs": ["claude/0a77", "codex/7ee9"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["added"], json!(["longcat", "longcat-codex"]));
    assert_eq!(result["failed"], json!([]));

    // config.json:字段级合并(共享字段保留)+ 新源落盘。
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(document["port"], 3427, "共享字段保留");
    let providers = document["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    let codex_row = providers
        .iter()
        .find(|p| p["id"] == "longcat-codex")
        .unwrap();
    assert_eq!(codex_row["protocol"], "responses");
    assert_eq!(codex_row["kind"], "subscription");

    // keyring:两个 key 落库;路由快照热更新(源可解析)。
    assert_eq!(
        store.get("longcat").as_deref(),
        Some("fixture-claude-token")
    );
    assert_eq!(
        store.get("longcat-codex").as_deref(),
        Some("fixture-codex-key")
    );
    assert!(state.manifest("longcat").is_some());
    assert!(state.manifest("longcat-codex").is_some());

    // 重复导入同 refs:勾选即新增语义下 id 冲突逐行报错。
    let again: Value = client
        .post(format!("{base}/admin/import/cc-switch"))
        .bearer_auth("test-admin")
        .json(&json!({"refs": ["claude/0a77"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["added"], json!([]));
    let failed = again["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert!(failed[0]["error"].as_str().unwrap().contains("已存在"));

    // 未知 ref 与 skip 行:failed 带原因。
    let mixed: Value = client
        .post(format!("{base}/admin/import/cc-switch"))
        .bearer_auth("test-admin")
        .json(&json!({"refs": ["claude/nope", "claude/fe2d"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mixed["added"], json!([]));
    let failed = mixed["failed"].as_array().unwrap();
    assert!(failed[0]["error"].as_str().unwrap().contains("未找到"));
    assert!(failed[1]["error"].as_str().unwrap().contains("自环"));

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn missing_db_and_unwired_state_yield_empty_items() {
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_admin_token("test-admin".into()),
    );
    let (base, server) = serve(state).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let body: Value = client
        .get(format!("{base}/admin/import/cc-switch"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["items"], json!([]));
    // 鉴权口径:无 admin key → 401。
    let status = client
        .get(format!("{base}/admin/import/cc-switch"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 401);
    server.abort();
}

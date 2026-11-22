//! Release-path regressions: replies, routing and credentials through the HTTP entry point.

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{body::Body, http::HeaderMap, http::StatusCode, routing::post, Router};
use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::{json, Value};
use support::replay::{spawn, Step};

const MODEL: &str = "glm-4.5-flash";
const CHAT_TOOL: &str = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");
const CODEX_REPLY: &str = include_str!("../../translate/tests/fixtures/codex_text.sse");

fn source(id: &str, base: &str, model: &str, kind: &str, protocol: &str) -> (String, Manifest) {
    let manifest = serde_json::from_value(json!({
        "id": id, "version": "1", "kind": kind,
        "provider": {"base":base, "auth":"bearer", "protocol":protocol},
        "free_models": [model], "permissions": {"net": [], "store_key": true}
    }))
    .unwrap();
    (id.to_string(), manifest)
}

async fn http_app(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, server)
}

async fn gateway(state: GatewayState) -> (reqwest::Client, String, tokio::task::JoinHandle<()>) {
    let (url, task) = http_app(gateway::app(Arc::new(state))).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    (client, url, task)
}

fn messages(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    stream: bool,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("aubridge-claude-code")
        .json(&json!({
            "model":model, "max_tokens":80, "stream":stream,
            "messages":[{"role":"user","content":"check the weather"}],
            "tools":[{"name":"get_weather","input_schema":{"type":"object"}}],
        }))
}

fn stalled_chat(after: String) -> Router {
    stalled_sse("/chat/completions", after)
}

fn stalled_sse(path: &str, after: String) -> Router {
    Router::new().route(
        path,
        post(move || {
            let after = after.clone();
            async move {
                use futures_util::StreamExt;
                let body = futures_util::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(after))
                })
                .chain(futures_util::stream::pending());
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(body),
                )
            }
        }),
    )
}

async fn latest(client: &reqwest::Client, base: &str) -> Value {
    client
        .get(format!("{base}/admin/dashboard"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn serving_watch_keeps_latest_value_without_subscribers_and_resets_on_new_state() {
    let state = GatewayState::from_manifests(BTreeMap::new(), "aubridge");
    assert!(state.is_serving());
    state.set_serving(false);
    assert!(!state.is_serving());
    let mut first = state.subscribe_serving();
    let mut second = state.subscribe_serving();
    assert!(!*first.borrow_and_update());
    assert!(!*second.borrow_and_update());
    state.set_serving(true);
    first.changed().await.unwrap();
    second.changed().await.unwrap();
    assert!(*first.borrow_and_update());
    assert!(*second.borrow_and_update());
    state.set_serving(false);
    state.set_serving(true);
    state.set_serving(false);
    first.changed().await.unwrap();
    second.changed().await.unwrap();
    assert!(!*first.borrow_and_update());
    assert!(!*second.borrow_and_update());
    drop(first);
    drop(second);
    state.set_serving(true);
    assert!(state.is_serving());
    state.set_serving(false);
    assert!(!*state.subscribe_serving().borrow());
    assert!(GatewayState::from_manifests(BTreeMap::new(), "aubridge").is_serving());
}

#[tokio::test]
async fn serving_pause_rejects_new_protocol_requests_but_finishes_committed_stream() {
    use futures_util::{stream, StreamExt};
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (upstream, upstream_task) = http_app(Router::new().route("/chat/completions", post({
        let release = release.clone();
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let release = release.clone();
            async move {
                let first = stream::once(async {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"before pause\"}}]}\n\n"
                    ))
                });
                let tail = stream::once(async move {
                    release.notified().await;
                    Ok(axum::body::Bytes::from_static(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" after pause\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n"
                    ))
                });
                ([("content-type","text/event-stream")], Body::from_stream(first.chain(tail)))
            }
        }
    }))).await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source("free", &upstream, MODEL, "free_tier", "chat")]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let mut changes = state.subscribe_serving();
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let mut response = messages(&client, &base, MODEL, true).send().await.unwrap();
    let mut prefix = Vec::new();
    while !String::from_utf8_lossy(&prefix).contains("before pause") {
        prefix.extend_from_slice(&response.chunk().await.unwrap().unwrap());
    }
    client
        .post(format!("{base}/admin/gateway"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"serving":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    changes.changed().await.unwrap();
    assert!(!*changes.borrow_and_update());
    assert!(!state.is_serving());
    for request in [
        messages(&client, &base, MODEL, true),
        client.post(format!("{base}/v1/chat/completions")).bearer_auth("aubridge-opencode")
            .json(&json!({"model":MODEL,"stream":true,"messages":[{"role":"user","content":"hello"}]})),
        client.post(format!("{base}/v1/responses")).bearer_auth("aubridge-codex")
            .json(&json!({"model":MODEL,"stream":true,"input":"hello"})),
    ] {
        assert_eq!(request.send().await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(latest(&client, &base).await["recent_decisions"], json!([]));
    release.notify_one();
    let tail = response.text().await.unwrap();
    assert!(tail.contains(" after pause") && tail.contains("message_stop"));
    assert!(!tail.contains("event: error"));
    let history = latest(&client, &base).await;
    assert_eq!(history["recent_decisions"].as_array().unwrap().len(), 1);
    assert_eq!(history["recent_decisions"][0]["outcome"], "ok");
    assert_eq!(history["recent_decisions"][0]["input_tokens"], 17);
    assert_eq!(history["recent_decisions"][0]["output_tokens"], 4);
    state.set_serving(true);
    changes.changed().await.unwrap();
    assert!(*changes.borrow_and_update());
    let overview: Value = client
        .get(format!("{base}/admin/gateway"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["serving"], true);
    upstream_task.abort();
    task.abort();
}

#[tokio::test]
async fn opencode_default_model_selection_enforces_managed_supply_guards() {
    let dir =
        std::env::temp_dir().join(format!("aubridge-opencode-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let opencode = dir.join("opencode.jsonc");
    std::fs::write(
        &opencode,
        "{\n// user preference\n\"model\":\"foreign/original\",\"theme\":\"dark\"\n}\n",
    )
    .unwrap();
    let state = Arc::new(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_settings(config.clone(), dir.join("stash"))
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_agents(agents::AgentsPaths {
                claude_settings: dir.join("claude.json"),
                codex_config: dir.join("codex.toml"),
                opencode_config: opencode.clone(),
                gemini_settings: dir.join("gemini.json"),
                gemini_env: dir.join("gemini.env"),
                state_dir: dir.join("agents"),
            }),
    );
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .json(&body)
            .send()
    };
    let delete = |path: &str| {
        client
            .delete(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .send()
    };
    post("providers", json!({"id":"local","base":"http://127.0.0.1:1","models":["local-model"],"kind":"free_tier"}))
        .await.unwrap().error_for_status().unwrap();
    post(
        "model-groups",
        json!({"name":"daily/group","members":["local-model"],"rule":"free_only"}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    post("agents/opencode/enable", json!({"on":true}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(std::fs::read_to_string(&opencode)
        .unwrap()
        .contains("foreign/original"));
    post("providers", json!({"id":"unused","base":"http://127.0.0.1:1","models":["unused-model"],"kind":"free_tier"}))
        .await.unwrap().error_for_status().unwrap();
    delete("providers/unused")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    post("agents/opencode/model", json!({"model":"daily/group"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(
        std::fs::read_to_string(&opencode)
            .unwrap()
            .contains("aubridge/daily/group"),
        "successful Admin selection must write the actual OpenCode default"
    );
    let snapshot = || {
        [
            std::fs::read(&config).unwrap(),
            std::fs::read(&opencode).unwrap(),
        ]
    };
    let before = snapshot();
    for response in [
        delete("model-groups/daily%2Fgroup").await.unwrap(),
        delete("providers/local").await.unwrap(),
        post("providers/local/models", json!({"models":["other"]}))
            .await
            .unwrap(),
        post(
            "providers/local/models",
            json!({"models":["local-model"],"kind":"subscription"}),
        )
        .await
        .unwrap(),
    ] {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.unwrap();
        let error = body["error"]
            .as_str()
            .or_else(|| body["error"]["message"].as_str())
            .unwrap();
        assert!(error.contains("OpenCode"), "{error}");
        assert_eq!(snapshot(), before);
    }
    // A group may remain stored without supply while it is unselected.
    post("agents/opencode/model", json!({"model":"aubridge"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    post(
        "providers/local/models",
        json!({"models":["local-model"],"kind":"subscription"}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let before = snapshot();
    let rejected = post("agents/opencode/model", json!({"model":"daily/group"}))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(snapshot(), before);
    post(
        "model-groups",
        json!({"name":"daily/group","members":["local-model"],"rule":"sequential"}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    post("agents/opencode/model", json!({"model":"daily/group"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let before = snapshot();
    let rejected = post(
        "model-groups",
        json!({"name":"daily/group","members":["local-model"],"rule":"free_only"}),
    )
    .await
    .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(rejected.text().await.unwrap().contains("OpenCode"));
    assert_eq!(snapshot(), before);

    post("agents/opencode/model", json!({"model":"local-model"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("model-groups/daily%2Fgroup")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let rejected = delete("providers/local").await.unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(rejected.text().await.unwrap().contains("OpenCode"));
    post("agents/opencode/model", json!({"model":"aubridge"}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("providers/local")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(state.manifest("local").is_none());
    let preview: Value = client
        .get(format!("{base}/admin/agents/opencode/disconnect-preview"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    post(
        "agents/opencode/enable",
        json!({"on":false,"preview":preview}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert!(std::fs::read_to_string(&opencode)
        .unwrap()
        .contains("foreign/original"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn model_group_edit_and_selection_preserve_active_declared_supply() {
    let dir =
        std::env::temp_dir().join(format!("aubridge-edit-active-group-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("config.json");
    let claude = dir.join("claude.json");
    let profile = dir.join("aubridge.config.toml");
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([
                source(
                    "paid",
                    "http://127.0.0.1:1",
                    "paid-model",
                    "subscription",
                    "chat",
                ),
                source("free", "http://127.0.0.1:1", MODEL, "free_tier", "chat"),
            ]),
            "aubridge",
        )
        .with_settings(settings.clone(), dir.join("stash"))
        .with_agents(agents::AgentsPaths {
            claude_settings: claude.clone(),
            codex_config: dir.join("codex.toml"),
            opencode_config: dir.join("opencode.jsonc"),
            gemini_settings: dir.join("gemini.json"),
            gemini_env: dir.join("gemini.env"),
            state_dir: dir.join("agents"),
        }),
    );
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let post_admin = |path: &str, body: Value| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .json(&body)
            .send()
    };
    let group = |name: &str, rule: &str, members: &[&str]| {
        post_admin(
            "model-groups",
            json!({"name":name,"rule":rule,"members":members}),
        )
    };
    let select =
        |id: &str, model: &str| post_admin(&format!("agents/{id}/model"), json!({"model":model}));
    let paid = &["paid-model"];
    let mixed = &["paid-model", MODEL];
    let accept = |response: Result<reqwest::Response, reqwest::Error>| {
        response.unwrap().error_for_status().unwrap()
    };
    accept(group("prepared", "free_only", paid).await);
    accept(select("claude-code", "prepared").await);
    for id in ["claude-code", "codex"] {
        accept(post_admin(&format!("agents/{id}/enable"), json!({"on":true})).await);
    }
    // An old bad selection may be maintained and then repaired.
    accept(group("prepared", "free_only", paid).await);
    accept(group("prepared", "free_only", mixed).await);
    accept(group("daily", "round_robin", paid).await);
    for id in ["claude-code", "codex"] {
        accept(select(id, "daily").await);
    }
    assert_eq!(state.next_group_rotation("daily"), 0);
    let snapshot = || [&settings, &claude, &profile].map(|path| std::fs::read(path).unwrap());
    let before = snapshot();
    let original = state.group_of("daily");
    let reject = |response: Result<reqwest::Response, reqwest::Error>| async move {
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply: Value = response.json().await.unwrap();
        reply["error"].as_str().unwrap().to_owned()
    };
    let error = reject(group("daily", "free_only", paid).await).await;
    assert!(error.contains("daily") && error.contains("Claude Code") && error.contains("Codex"));
    assert_eq!(snapshot(), before);
    assert_eq!(state.group_of("daily"), original);
    assert_eq!(state.next_group_rotation("daily"), 1);

    accept(group("daily", "free_only", mixed).await);
    let original = state.group_of("daily");
    for (other, expected, absent) in [
        ("claude-code", "Codex", "Claude Code"),
        ("codex", "Claude Code", "Codex"),
    ] {
        accept(select(other, "aubridge").await);
        let before = snapshot();
        let error = reject(group("daily", "free_only", paid).await).await;
        assert!(
            error.contains(expected) && !error.contains(absent),
            "{error}"
        );
        assert_eq!(snapshot(), before);
        assert_eq!(state.group_of("daily"), original);
        accept(select(other, "daily").await);
    }

    // Unselected edits are allowed, but a wired client cannot select the result.
    accept(group("prepared", "free_only", paid).await);
    for id in ["claude-code", "codex"] {
        let before = snapshot();
        let error = reject(select(id, "prepared").await).await;
        assert!(error.contains("prepared"), "{error}");
        assert_eq!(snapshot(), before);
    }
    accept(group("prepared", "free_only", mixed).await);
    accept(select("codex", "prepared").await);

    accept(group("racing", "round_robin", paid).await);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let selection = tokio::task::spawn_blocking({
        let state = state.clone();
        let barrier = barrier.clone();
        move || {
            barrier.wait();
            state.set_agent_model("claude-code", "racing")
        }
    });
    let edit = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            let group = credentials::settings::ModelGroup {
                name: "racing".into(),
                rule: "free_only".into(),
                members: vec!["paid-model".into()],
            };
            barrier.wait();
            state.upsert_model_group(&group)
        }
    });
    let (selection, edit) = tokio::join!(selection, edit);
    let selection = selection.unwrap();
    let edit = edit.unwrap();
    assert_ne!(selection.is_ok(), edit.is_ok());
    let config: Value = serde_json::from_slice(&std::fs::read(&claude).unwrap()).unwrap();
    let (_, rule) = state.group_of("racing").unwrap();
    if let Err(error) = edit {
        assert!(error.contains("Claude Code") && error.contains("racing"));
        assert_eq!(config["model"], "racing");
        assert_eq!(rule, router::GroupRule::RoundRobin);
    } else {
        assert!(selection.unwrap_err().contains("racing"));
        assert_eq!(config["model"], "daily");
        assert_eq!(rule, router::GroupRule::FreeOnly);
    }
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn model_group_delete_protects_wired_claude_and_codex_references() {
    let dir = std::env::temp_dir().join(format!("aubridge-active-group-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("config.json");
    let claude = dir.join("claude.json");
    let profile = dir.join("aubridge.config.toml");
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "fixture",
                "http://127.0.0.1:1",
                MODEL,
                "free_tier",
                "chat",
            )]),
            "aubridge",
        )
        .with_settings(settings.clone(), dir.join("stash"))
        .with_agents(agents::AgentsPaths {
            claude_settings: claude.clone(),
            codex_config: dir.join("codex.toml"),
            opencode_config: dir.join("opencode.jsonc"),
            gemini_settings: dir.join("gemini.json"),
            gemini_env: dir.join("gemini.env"),
            state_dir: dir.join("agents"),
        }),
    );
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let post_admin = |path: &str, body: Value| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .json(&body)
            .send()
    };
    let create = |name| {
        post_admin(
            "model-groups",
            json!({"name":name,"members":[MODEL],"rule":"round_robin"}),
        )
    };
    let select =
        |id: &str, model: &str| post_admin(&format!("agents/{id}/model"), json!({"model":model}));
    let delete = |name: &str| {
        client
            .delete(format!("{base}/admin/model-groups/{name}"))
            .bearer_auth("aubridge-admin")
            .send()
    };
    create("daily").await.unwrap().error_for_status().unwrap();
    for id in ["claude-code", "codex"] {
        post_admin(&format!("agents/{id}/enable"), json!({"on":true}))
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        select(id, "daily")
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    assert_eq!(state.next_group_rotation("daily"), 0);
    let snapshot = || [&settings, &claude, &profile].map(|path| std::fs::read(path).unwrap());
    for (rotation, (claude_active, codex_active)) in
        (1..).zip([(true, true), (true, false), (false, true)])
    {
        for (id, active) in [("claude-code", claude_active), ("codex", codex_active)] {
            select(id, if active { "daily" } else { MODEL })
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        let before = snapshot();
        let response = delete("daily").await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply: Value = response.json().await.unwrap();
        let message = reply["error"].as_str().unwrap();
        assert!(message.contains("daily"));
        assert_eq!(message.contains("Claude Code"), claude_active, "{message}");
        assert_eq!(message.contains("Codex"), codex_active, "{message}");
        assert!(!message.contains("aubridge-claude-code") && !message.contains("aubridge-codex"));
        assert_eq!(snapshot(), before);
        assert!(state.group_of("daily").is_some());
        assert_eq!(state.next_group_rotation("daily"), rotation);
    }
    select("codex", MODEL)
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("daily").await.unwrap().error_for_status().unwrap();
    assert!(state.group_of("daily").is_none());
    let saved: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(saved["model_groups"], json!([]));
    for id in ["claude-code", "codex"] {
        let before = snapshot();
        let response = select(id, "daily").await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply: Value = response.json().await.unwrap();
        assert!(reply["error"].as_str().unwrap().contains("未知模型"));
        assert_eq!(snapshot(), before);
    }

    create("racing").await.unwrap().error_for_status().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let set_model = tokio::task::spawn_blocking({
        let state = state.clone();
        let barrier = barrier.clone();
        move || {
            barrier.wait();
            state.set_agent_model("claude-code", "racing")
        }
    });
    let remove = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            barrier.wait();
            state.remove_model_group("racing")
        }
    });
    let (set_model, remove) = tokio::join!(set_model, remove);
    let set_model = set_model.unwrap();
    let remove = remove.unwrap();
    assert_ne!(set_model.is_ok(), remove.is_ok());
    let config: Value = serde_json::from_slice(&std::fs::read(&claude).unwrap()).unwrap();
    if let Err(error) = remove {
        assert!(error.contains("Claude Code"));
        assert_eq!(config["model"], "racing");
        assert!(state.group_of("racing").is_some());
    } else {
        assert!(set_model.unwrap_err().contains("未知模型"));
        assert_eq!(config["model"], MODEL);
        assert!(state.group_of("racing").is_none());
    }

    create("stale").await.unwrap().error_for_status().unwrap();
    for id in ["claude-code", "codex"] {
        select(id, "stale")
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let mut foreign: Value = serde_json::from_slice(&std::fs::read(&claude).unwrap()).unwrap();
    foreign["env"]["ANTHROPIC_BASE_URL"] = json!("https://third-party.invalid");
    std::fs::write(&claude, foreign.to_string()).unwrap();
    let edited_profile = format!(
        "{}# user edit\n",
        std::fs::read_to_string(&profile).unwrap()
    );
    std::fs::write(&profile, &edited_profile).unwrap();
    delete("stale").await.unwrap().error_for_status().unwrap();
    assert_eq!(
        std::fs::read_to_string(&claude).unwrap(),
        foreign.to_string()
    );
    assert_eq!(std::fs::read_to_string(&profile).unwrap(), edited_profile);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn request_history_distinguishes_empty_from_unavailable() {
    for available in [false, true] {
        let mut state = GatewayState::from_manifests(
            BTreeMap::from([source(
                "fixture",
                "http://127.0.0.1:1",
                MODEL,
                "free_tier",
                "chat",
            )]),
            "aubridge",
        );
        if available {
            state = state.with_ledger(ledger::Ledger::open(":memory:").unwrap());
        }
        let endpoint = format!("http://127.0.0.1:{}", state.port());
        let serving = state.is_serving();
        let (client, base, task) = gateway(state).await;
        let history: Value = client
            .get(format!("{base}/admin/gateway"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let dashboard = latest(&client, &base).await;
        task.abort();
        let (status, rows) = if available {
            ("available", json!([]))
        } else {
            ("unavailable", Value::Null)
        };
        assert_eq!(history["history_status"], status);
        assert_eq!(history.get("requests"), Some(&rows));
        assert_eq!(history.get("next"), Some(&Value::Null));
        assert_eq!(history["serving"], serving);
        assert_eq!(history["endpoint"], endpoint);
        assert_eq!(dashboard.get("recent_decisions"), Some(&rows));
        assert_eq!(dashboard["providers"][0]["id"], "fixture");
        if available {
            assert_eq!(dashboard["monthly_savings_status"], "available");
            assert_eq!(dashboard["monthly_savings_usd"], 0.0);
        }
    }
}

#[tokio::test]
async fn dashboard_monthly_savings_distinguishes_empty_from_unavailable() {
    let state = || {
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "fixture",
                "http://127.0.0.1:1",
                MODEL,
                "free_tier",
                "chat",
            )]),
            "aubridge",
        )
    };
    let (client, base, task) = gateway(state()).await;
    let unavailable = latest(&client, &base).await;
    task.abort();

    let (client, base, task) =
        gateway(state().with_ledger(ledger::Ledger::open(":memory:").unwrap())).await;
    let empty = latest(&client, &base).await;
    task.abort();

    for field in ["monthly_savings_usd", "top_providers", "share_card"] {
        assert_eq!(unavailable.get(field), Some(&Value::Null), "{field}");
    }
    assert_eq!(unavailable["monthly_savings_status"], "unavailable");
    assert_eq!(unavailable["providers"][0]["id"], "fixture");
    assert_eq!(unavailable["providers"], empty["providers"]);
    assert_eq!(unavailable.get("recent_decisions"), Some(&Value::Null));

    assert_eq!(empty["monthly_savings_status"], "available");
    assert_eq!(empty["monthly_savings_usd"], 0.0);
    assert_eq!(empty["top_providers"], json!([]));
    assert!(empty["share_card"].as_str().unwrap().contains("$0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_provider_is_excluded_until_explicitly_resumed_after_restart() {
    let (free, free_base, free_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let (plan, plan_base, plan_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let manifests = BTreeMap::from([
        source("free", &free_base, "free-model", "free_tier", "chat"),
        source("plan", &plan_base, "plan-model", "subscription", "chat"),
    ]);
    let dir = std::env::temp_dir().join(format!("aubridge-provider-pause-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"port":3427,"extra":"preserve"}"#).unwrap();
    let state = || {
        GatewayState::from_manifests(manifests.clone(), "aubridge")
            .with_settings(config.clone(), dir.join("stash"))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap())
    };
    let (client, base, first_task) = gateway(state()).await;
    let pause = |base: &str| client.post(format!("{base}/admin/providers/plan/pause"));
    let update = |base: &str, paused: bool| {
        pause(base)
            .bearer_auth("aubridge-admin")
            .json(&json!({"paused":paused}))
    };
    assert_eq!(
        pause(&base)
            .bearer_auth("aubridge-claude-code")
            .json(&json!({"paused":true}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        pause(&base)
            .bearer_auth("aubridge-admin")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let (paused, group) = tokio::join!(
        update(&base, true).send(),
        client
            .post(format!("{base}/admin/model-groups"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"name":"pause-test","members":["plan-model","free-model"]}))
            .send(),
    );
    paused.unwrap().error_for_status().unwrap();
    group.unwrap().error_for_status().unwrap();
    let persisted: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(persisted["paused_providers"], json!(["plan"]));
    assert_eq!(persisted["model_groups"][0]["name"], "pause-test");
    assert_eq!(persisted["port"], 3427);
    assert_eq!(persisted["extra"], "preserve");
    first_task.abort();

    let (_, base, restarted) = gateway(state()).await;
    let info: Value = client
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "plan" && p["paused"] == true && p["ready"] == true));
    let response = messages(&client, &base, "plan-model", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let history = latest(&client, &base).await;
    assert_eq!(
        history["recent_decisions"][0]["skipped"][0]["reason"],
        "provider-paused"
    );
    let response = messages(&client, &base, "pause-test", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    assert_eq!(free.seen_bodies.lock().await.len(), 1);
    assert!(plan.seen_bodies.lock().await.is_empty());

    let blocked_write = config.with_extension("json.tmp");
    std::fs::create_dir(&blocked_write).unwrap();
    assert_eq!(update(&base, false).send().await.unwrap().status(), 400);
    let response = messages(&client, &base, "plan-model", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(plan.seen_bodies.lock().await.is_empty());
    std::fs::remove_dir(blocked_write).unwrap();

    update(&base, false)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let response = messages(&client, &base, "plan-model", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    assert_eq!(plan.seen_bodies.lock().await.len(), 1);
    for task in [free_task, plan_task, restarted] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn paused_provider_is_rechecked_after_waiting_for_a_subscription_slot() {
    let (plan, plan_base, plan_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let dir = std::env::temp_dir().join(format!(
        "aubridge-provider-pause-slot-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "plan",
                &plan_base,
                "plan-model",
                "subscription",
                "chat",
            )]),
            "aubridge",
        )
        .with_settings(dir.join("config.json"), dir.join("stash"))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let held = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let (base, gateway_task) = http_app(gateway::app(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let queued = messages(&client, &base, "plan-model", false);
    let request = tokio::spawn(async move { queued.send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!request.is_finished());
    client
        .post(format!("{base}/admin/providers/plan/pause"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"paused":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    drop(held);
    assert_eq!(request.await.unwrap().status(), 502);
    assert!(plan.seen_bodies.lock().await.is_empty());
    let history = latest(&client, &base).await;
    assert_eq!(
        history["recent_decisions"][0]["skipped"][0]["reason"],
        "provider-paused"
    );
    assert_eq!(history["recent_decisions"][0]["attempts"], json!([]));
    plan_task.abort();
    gateway_task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn explicit_routing_resume_persists_and_keeps_pause_and_cooldown() {
    let (replay, upstream, upstream_task) = spawn(vec![]).await;
    let dir = std::env::temp_dir().join(format!("aubridge-routing-resume-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let build_state = || {
        GatewayState::from_manifests(
            BTreeMap::from([source("free", &upstream, MODEL, "free_tier", "chat")]),
            "aubridge",
        )
        .with_settings(dir.join("config.json"), dir.join("stash"))
        .with_ledger(ledger::Ledger::open(dir.join("ledger.sqlite")).unwrap())
    };
    let state = Arc::new(build_state());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    state.apply_failure(
        "free",
        &router::UpstreamSignal {
            status: 429,
            retry_after: Some(180),
            ..Default::default()
        },
        now,
        0,
    );
    state.apply_failure(
        "free",
        &router::UpstreamSignal {
            status: 403,
            account_restricted: true,
            ..Default::default()
        },
        now,
        0,
    );
    state.flush_router_state();
    let initial = &state.snapshot()[0];
    assert!(!initial.enabled);
    assert_eq!(initial.rest_until, Some(now + 180));
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    client
        .post(format!("{base}/admin/providers/free/pause"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"paused":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let resume = |id: &str, token: &str| {
        client
            .post(format!("{base}/admin/providers/{id}/resume-routing"))
            .bearer_auth(token)
    };
    assert_eq!(
        resume("free", "aubridge-opencode")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        resume("missing", "aubridge-admin")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(state.providers_overview()[0]["routing_disabled"], true);
    for _ in 0..2 {
        let reply = resume("free", "aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            reply.json::<Value>().await.unwrap()["routing_disabled"],
            false
        );
    }
    assert_eq!(state.providers_overview()[0]["routing_disabled"], false);
    let restarted = build_state();
    assert!(restarted.snapshot()[0].enabled);
    assert_eq!(restarted.snapshot()[0].rest_until, initial.rest_until);
    assert_eq!(restarted.providers_overview()[0]["paused"], true);
    let (client, restarted_url, restarted_task) = gateway(restarted).await;
    let paused = messages(&client, &restarted_url, MODEL, false)
        .send()
        .await
        .unwrap();
    assert_eq!(paused.status(), 502);
    assert_eq!(
        latest(&client, &restarted_url).await["recent_decisions"][0]["skipped"][0]["reason"],
        "provider-paused"
    );
    client
        .post(format!("{restarted_url}/admin/providers/free/pause"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"paused":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let cooled = messages(&client, &restarted_url, MODEL, false)
        .send()
        .await
        .unwrap();
    assert_eq!(cooled.status(), 429);
    assert!(cooled.headers().contains_key("retry-after"));
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    restarted_task.abort();
    assert!(restarted_task.await.unwrap_err().is_cancelled());
    upstream_task.abort();
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn queued_subscription_rechecks_new_account_cooldown() {
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "plan",
                &upstream,
                "plan-model",
                "subscription",
                "chat",
            )]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let held = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let queued = messages(&client, &base, "plan-model", false);
    let request = tokio::spawn(async move { queued.send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    state.apply_failure(
        "plan",
        &router::UpstreamSignal {
            status: 429,
            retry_after: Some(45),
            ..Default::default()
        },
        now,
        0,
    );
    drop(held);
    let response = request.await.unwrap();
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    assert!(replay.seen_bodies.lock().await.is_empty());
    let history = latest(&client, &base).await;
    let row = &history["recent_decisions"][0];
    assert_eq!(row["skipped"][0]["reason"], "account-rate-limit");
    assert_eq!(row["attempts"], json!([]));
    assert!(row["duration_ms"].as_u64().unwrap() >= 50);
    upstream_task.abort();
    task.abort();
}

#[tokio::test]
async fn queued_subscription_uses_latest_key() {
    let reply = Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (replay, upstream, upstream_task) = spawn(vec![reply.clone(), reply]).await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "plan",
                &upstream,
                "plan-model",
                "subscription",
                "chat",
            )]),
            "aubridge",
        )
        .with_key_store(Arc::new(credentials::MemoryStore::default()))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    state.set_provider_key("plan", "fixture-before").unwrap();
    let slot = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap();
    let (base, task) = http_app(gateway::app(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let keys = format!("{base}/admin/keys");
    for (index, (change, expected)) in [
        (
            client
                .post(&keys)
                .json(&json!({"id":"plan","key":"fixture-after"})),
            Some("Bearer fixture-after"),
        ),
        (client.delete(&keys).json(&json!({"id":"plan"})), None),
    ]
    .into_iter()
    .enumerate()
    {
        let held = slot.clone().acquire_many_owned(2).await.unwrap();
        let queued = messages(&client, &base, "plan-model", false);
        let request = tokio::spawn(async move { queued.send().await.unwrap() });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!request.is_finished());
        change
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        drop(held);
        assert_eq!(request.await.unwrap().status(), 200);
        let headers = replay.seen_headers.lock().await;
        assert_eq!(
            headers[index]
                .get("authorization")
                .and_then(|h| h.to_str().ok()),
            expected
        );
    }
    upstream_task.abort();
    task.abort();
}

#[tokio::test]
async fn late_forbidden_reply_cannot_disable_a_rotated_key() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new().route(
        "/chat/completions",
        post({
            let started = started.clone();
            let release = release.clone();
            move |headers: HeaderMap| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    let key = headers["authorization"].to_str().unwrap();
                    let (status, content_type, body) = if key == "Bearer fixture-old" {
                        started.notify_one();
                        release.notified().await;
                        (
                            403,
                            "application/json",
                            r#"{"error":{"message":"account has been suspended"}}"#,
                        )
                    } else {
                        assert_eq!(key, "Bearer fixture-new");
                        (200, "text/event-stream", CHAT_TOOL)
                    };
                    axum::http::Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(Body::from(body))
                        .unwrap()
                }
            }
        }),
    );
    let (upstream, upstream_task) = http_app(upstream).await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source("free", &upstream, MODEL, "free_tier", "chat")]),
            "aubridge",
        )
        .with_key_store(Arc::new(credentials::MemoryStore::default()))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    state.set_provider_key("free", "fixture-old").unwrap();
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let pending = messages(&client, &base, MODEL, false);
    let request = tokio::spawn(async move { pending.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    client
        .post(format!("{base}/admin/keys"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"id":"free","key":"fixture-new"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    release.notify_one();
    assert_eq!(request.await.unwrap().status(), 502);
    assert!(state.snapshot()[0].enabled);
    assert_eq!(
        latest(&client, &base).await["recent_decisions"][0]["attempts"][0]["reason"],
        "access-denied"
    );
    let reply = messages(&client, &base, MODEL, false).send().await.unwrap();
    assert_eq!(reply.headers()["x-aubridge-provider"], "free");
    assert_eq!(reply.status(), 200);
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn free_only_group_keeps_same_model_subscription_out_of_fallback() {
    let (free, free_base, free_task) = spawn(vec![Step::FailWithHeaders {
        status: 429,
        body: r#"{"error":{"code":"model_rate_limit_exceeded"}}"#.into(),
        headers: vec![("retry-after".into(), "90".into())],
    }])
    .await;
    let (plan, plan_base, plan_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let manifests = BTreeMap::from([
        source("free", &free_base, MODEL, "free_tier", "chat"),
        source("plan", &plan_base, MODEL, "subscription", "chat"),
    ]);
    let dir = std::env::temp_dir().join(format!("aubridge-free-only-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = || {
        GatewayState::from_manifests(manifests.clone(), "aubridge")
            .with_settings(dir.join("config.json"), dir.join("stash"))
            .with_ledger(ledger::Ledger::open(dir.join("ledger.sqlite")).unwrap())
    };
    let (client, base, task) = gateway(state()).await;
    let group = "free-session";
    client
        .post(format!("{base}/admin/model-groups"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"name":group,"members":[MODEL],"rule":"free_only"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(response.status(), 429);
    assert!(response.headers().contains_key("retry-after"));
    let history = latest(&client, &base).await;
    let row = &history["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["id"], "free");
    assert_eq!(row["skipped"][0]["reason"], "subscription-excluded");
    assert!(plan.seen_bodies.lock().await.is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let (_, base, restarted) = gateway(state()).await;
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == group));
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(response.status(), 429);
    assert!(plan.seen_bodies.lock().await.is_empty());
    assert_eq!(free.seen_bodies.lock().await.len(), 1);

    let normal = format!("group/{MODEL}");
    let response = messages(&client, &base, &normal, false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    assert_eq!(plan.seen_bodies.lock().await.len(), 1);
    for task in [free_task, plan_task, restarted] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn clear_request_history_discards_decisions_but_keeps_pending_usage() {
    let (_, upstream, upstream_task) = spawn(vec![
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let dir = std::env::temp_dir().join(format!("aubridge-clear-history-{}", std::process::id()));
    let db = dir.join("ledger.sqlite");
    let ledger = ledger::Ledger::open(&db).unwrap();
    ledger
        .record_requests(&[ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                ts: 1,
                model: MODEL.into(),
                outcome: "ok".into(),
                ..Default::default()
            },
            usage: vec![ledger::UsageRecord {
                ts: 1,
                agent: "claude-code".into(),
                provider: "previous".into(),
                model: MODEL.into(),
                input_tokens: Some(10),
                output_tokens: Some(5),
                accounting: ledger::Accounting::Reported,
                price_snapshot: ledger::PriceSnapshot::builtin(MODEL),
                ..Default::default()
            }],
        }])
        .unwrap();
    let state = GatewayState::from_manifests(
        BTreeMap::from([source("free", &upstream, MODEL, "free_tier", "chat")]),
        "aubridge",
    )
    .with_ledger(ledger);
    let (client, base, task) = gateway(state).await;
    let response = messages(&client, &base, MODEL, false).send().await.unwrap();
    let reply: Value = response.error_for_status().unwrap().json().await.unwrap();
    let new_tokens = reply["usage"]["input_tokens"].as_u64().unwrap()
        + reply["usage"]["output_tokens"].as_u64().unwrap();
    assert!(new_tokens > 0);
    let clear = |token: &str| {
        client
            .delete(format!("{base}/admin/gateway/history"))
            .bearer_auth(token)
    };
    assert_eq!(
        clear("aubridge-opencode").send().await.unwrap().status(),
        401
    );
    let first: Value = clear("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["deleted"], 2);
    let second: Value = clear("aubridge-admin")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["deleted"], 0);
    let history = latest(&client, &base).await;
    assert_eq!(history["recent_decisions"], json!([]));
    let reader = ledger::Ledger::open(&db).unwrap();
    assert_eq!(reader.value_summary(0, i64::MAX as u64).unwrap().records, 2);
    let (total, by_provider) = reader.monthly_savings(0, i64::MAX as u64).unwrap();
    assert_eq!(by_provider.len(), 2);
    assert!((total - (15 + new_tokens) as f64 * 0.11 / 1e6).abs() < 1e-12);
    drop(reader);

    let response = messages(&client, &base, MODEL, false).send().await.unwrap();
    response.error_for_status().unwrap().bytes().await.unwrap();
    assert_eq!(
        latest(&client, &base).await["recent_decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    upstream_task.abort();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn custom_provider_protects_last_declared_supply_for_wired_agents() {
    use credentials::KeyStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Store {
        keys: credentials::MemoryStore,
        deletes: AtomicUsize,
    }
    impl KeyStore for Store {
        fn set(&self, provider: &str, key: &str) -> Result<(), String> {
            self.keys.set(provider, key)
        }
        fn get(&self, provider: &str) -> Option<String> {
            self.keys.get(provider)
        }
        fn delete(&self, provider: &str) -> Result<(), String> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            self.keys.delete(provider)
        }
    }

    let dir = std::env::temp_dir().join(format!("aubridge-last-supply-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("config.json");
    let claude = dir.join("claude.json");
    let profile = dir.join("aubridge.config.toml");
    let store = Arc::new(Store::default());
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "paid",
                "http://127.0.0.1:1",
                "paid-model",
                "subscription",
                "chat",
            )]),
            "aubridge",
        )
        .with_settings(settings.clone(), dir.join("stash"))
        .with_key_store(store.clone())
        .with_agents(agents::AgentsPaths {
            claude_settings: claude.clone(),
            codex_config: dir.join("codex.toml"),
            opencode_config: dir.join("opencode.jsonc"),
            gemini_settings: dir.join("gemini.json"),
            gemini_env: dir.join("gemini.env"),
            state_dir: dir.join("agents"),
        }),
    );
    let (base, task) = http_app(gateway::app(state.clone())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let post_admin = |path: &str, body: Value| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
            .json(&body)
            .send()
    };
    let add = |id: &str, model: &str| {
        post_admin(
            "providers",
            json!({"id":id,"base":"http://127.0.0.1:1","models":[model],"key":"fixture-key"}),
        )
    };
    let edit = |body| post_admin("providers/primary-source/models", body);
    let select =
        |id: &str, model: &str| post_admin(&format!("agents/{id}/model"), json!({"model":model}));
    let delete = |id: &str| {
        client
            .delete(format!("{base}/admin/providers/{id}"))
            .bearer_auth("aubridge-admin")
            .send()
    };
    let group = |rule: &str| {
        post_admin(
            "model-groups",
            json!({"name":"daily","members":[MODEL,"paid-model"],"rule":rule}),
        )
    };
    add("primary-source", MODEL)
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    group("free_only")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for id in ["claude-code", "codex"] {
        post_admin(&format!("agents/{id}/enable"), json!({"on":true}))
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        select(id, MODEL).await.unwrap().error_for_status().unwrap();
    }
    let snapshot = || [&settings, &claude, &profile].map(|path| std::fs::read(path).unwrap());
    let blocked = |response: reqwest::Response, selection: &str, claude_active: bool| {
        let selection = selection.to_owned();
        async move {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error_path = if response.url().path().ends_with("/models") {
                "/error/message"
            } else {
                "/error"
            };
            let reply: Value = response.json().await.unwrap();
            let error = reply.pointer(error_path).and_then(Value::as_str).unwrap();
            assert!(
                error.contains(&selection) && error.contains("Codex"),
                "{error}"
            );
            assert_eq!(error.contains("Claude Code"), claude_active, "{error}");
            assert!(!error.contains("fixture-key"));
        }
    };
    let before = snapshot();
    let providers = state.providers_overview();
    let models = state.models();
    blocked(delete("primary-source").await.unwrap(), MODEL, true).await;
    blocked(
        edit(json!({"models":["other"]})).await.unwrap(),
        MODEL,
        true,
    )
    .await;
    assert_eq!(snapshot(), before);
    assert_eq!(state.providers_overview(), providers);
    assert_eq!(state.models(), models);
    assert_eq!(store.deletes.load(Ordering::Relaxed), 0);
    assert_eq!(store.get("primary-source").as_deref(), Some("fixture-key"));

    add("alternate-source", MODEL)
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("alternate-source")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(store.get("alternate-source").is_none());
    assert_eq!(store.deletes.load(Ordering::Relaxed), 1);
    select("claude-code", "aubridge")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    select("codex", "daily")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let before = snapshot();
    let providers = state.providers_overview();
    blocked(delete("primary-source").await.unwrap(), "daily", false).await;
    blocked(
        edit(json!({"models":["other"]})).await.unwrap(),
        "daily",
        false,
    )
    .await;
    blocked(
        edit(json!({"models":[MODEL],"kind":"subscription"}))
            .await
            .unwrap(),
        "daily",
        false,
    )
    .await;
    assert_eq!(snapshot(), before);
    assert_eq!(state.providers_overview(), providers);
    assert_eq!(store.deletes.load(Ordering::Relaxed), 1);
    assert_eq!(store.get("primary-source").as_deref(), Some("fixture-key"));

    group("sequential")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    edit(json!({"models":["other"]}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("primary-source")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(store.get("primary-source").is_none());
    assert!(state.manifest("primary-source").is_none());
    assert!(!state.models().iter().any(|model| model == MODEL));
    assert!(state.group_of("daily").is_some());

    add("stale-source", "stale-model")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    select("claude-code", "stale-model")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut foreign: Value = serde_json::from_slice(&std::fs::read(&claude).unwrap()).unwrap();
    foreign["env"]["ANTHROPIC_BASE_URL"] = json!("https://third-party.invalid");
    let foreign = serde_json::to_vec(&foreign).unwrap();
    std::fs::write(&claude, &foreign).unwrap();
    delete("stale-source")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(std::fs::read(&claude).unwrap(), foreign);

    // The original managed model is now absent. That preexisting breakage
    // cannot block unrelated maintenance when the wiring becomes current again.
    let mut broken: Value = serde_json::from_slice(&foreign).unwrap();
    broken["env"]["ANTHROPIC_BASE_URL"] = json!(format!("http://127.0.0.1:{}", state.port()));
    std::fs::write(&claude, serde_json::to_vec(&broken).unwrap()).unwrap();
    add("unrelated-source", "other")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    delete("unrelated-source")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(store.deletes.load(Ordering::Relaxed), 4);

    add("race-source", "race-model")
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let selection = tokio::task::spawn_blocking({
        let barrier = barrier.clone();
        let state = state.clone();
        move || {
            barrier.wait();
            state.set_agent_model("claude-code", "race-model")
        }
    });
    let removal = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            barrier.wait();
            state.remove_custom_provider("race-source")
        }
    });
    let (selection, removal) = tokio::join!(selection, removal);
    let selection = selection.unwrap();
    let removal = removal.unwrap();
    assert_ne!(selection.is_ok(), removal.is_ok());
    if let Err(error) = removal {
        assert!(error.contains("Claude Code") && error.contains("race-model"));
        assert_eq!(store.get("race-source").as_deref(), Some("fixture-key"));
        assert!(state.manifest("race-source").is_some());
    } else {
        assert!(selection.unwrap_err().contains("未知模型"));
        assert!(store.get("race-source").is_none());
        assert!(state.manifest("race-source").is_none());
        let claude: Value = serde_json::from_slice(&std::fs::read(&claude).unwrap()).unwrap();
        assert_eq!(claude["model"], "stale-model");
    }
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn custom_provider_delete_requires_credential_cleanup() {
    use credentials::KeyStore;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Store {
        keys: credentials::MemoryStore,
        reject_delete: AtomicBool,
    }
    impl KeyStore for Store {
        fn set(&self, provider: &str, key: &str) -> Result<(), String> {
            self.keys.set(provider, key)
        }
        fn get(&self, provider: &str) -> Option<String> {
            self.keys.get(provider)
        }
        fn delete(&self, provider: &str) -> Result<(), String> {
            if self.reject_delete.load(Ordering::SeqCst) {
                return Err("keyring fixture failure with fixture-key".into());
            }
            self.keys.delete(provider)
        }
    }
    let dir = std::env::temp_dir().join(format!("aubridge-delete-source-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let store = Arc::new(Store::default());
    let (client, base, server) = gateway(
        GatewayState::from_manifests(BTreeMap::new(), "aubridge")
            .with_settings(config.clone(), dir.join("stash"))
            .with_key_store(store.clone()),
    )
    .await;
    let endpoint = format!("{base}/admin/providers");
    let id = "fixture-delete";
    let add = || client.post(&endpoint).bearer_auth("aubridge-admin");
    let delete = || {
        client
            .delete(format!("{endpoint}/{id}"))
            .bearer_auth("aubridge-admin")
            .send()
    };
    let overview = || async {
        client
            .get(&endpoint)
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let definition = json!({"id":id,"base":"http://127.0.0.1:10001","models":["delete-model"]});
    let mut with_key = definition.clone();
    with_key["key"] = json!("fixture-key");
    add()
        .json(&with_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let original = std::fs::read(&config).unwrap();
    store.reject_delete.store(true, Ordering::SeqCst);
    let failure = delete().await.unwrap();
    assert_eq!(failure.status(), 400);
    let body = failure.text().await.unwrap();
    assert!(body.contains("钥匙串"));
    assert!(!body.contains("fixture-key"));
    assert_eq!(std::fs::read(&config).unwrap(), original);
    assert_eq!(overview().await["providers"][0]["key_set"], true);
    assert_eq!(store.get(id).as_deref(), Some("fixture-key"));

    store.reject_delete.store(false, Ordering::SeqCst);
    let blocked = config.with_extension("json.tmp");
    std::fs::create_dir(&blocked).unwrap();
    let failure = delete().await.unwrap();
    assert_eq!(failure.status(), 400);
    assert!(failure.text().await.unwrap().contains("钥匙串已清理"));
    assert_eq!(std::fs::read(&config).unwrap(), original);
    assert_eq!(overview().await["providers"][0]["key_set"], false);
    assert!(store.get(id).is_none());
    std::fs::remove_dir(&blocked).unwrap();

    delete().await.unwrap().error_for_status().unwrap();
    assert_eq!(overview().await["providers"], json!([]));
    add()
        .json(&definition)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(overview().await["providers"][0]["key_set"], false);
    assert!(store.get(id).is_none());
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn custom_model_edit_preserves_credentials_and_replaces_discovery() {
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let original_base = format!("{upstream}/old/v1");
    let dir = std::env::temp_dir().join(format!("aubridge-edit-models-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let store = Arc::new(credentials::MemoryStore::default());
    let state = || {
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "preset",
                &upstream,
                "preset-model",
                "free_tier",
                "chat",
            )]),
            "aubridge",
        )
        .with_settings(config.clone(), dir.join("stash"))
        .with_key_store(store.clone())
    };
    let (client, base, first) = gateway(state()).await;
    let admin = |base: &str, path: &str| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
    };
    admin(&base, "providers")
        .json(&json!({"id":"local","base":original_base,"models":["before"],"key":"fixture-key"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let edit = || admin(&base, "providers/local/models");
    let providers = |base: &str| {
        client
            .get(format!("{base}/admin/providers"))
            .bearer_auth("aubridge-admin")
    };
    assert_eq!(
        client
            .post(format!("{base}/admin/providers/local/models"))
            .bearer_auth("aubridge-opencode")
            .json(&json!({"models":["after"],"base":upstream}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        edit()
            .json(&json!({"models":[" "]}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        admin(&base, "providers/preset/models")
            .json(&json!({"models":["after"]}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        edit()
            .json(&json!({"models":["after"],"kind":"paid"}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let original_config = std::fs::read(&config).unwrap();
    let different_host = upstream.replace("127.0.0.1", "localhost");
    for invalid_base in [
        different_host.clone(),
        "http://127.0.0.1:0/v1".into(),
        upstream.replacen("http://", "https://", 1),
        upstream.replacen("http://", "http://fixture@", 1),
        format!("{upstream}/?query=1"),
        format!("{upstream}/#fragment"),
    ] {
        assert_eq!(
            edit()
                .json(&json!({"models":["after"],"base":invalid_base}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
            "accepted {invalid_base}"
        );
        assert_eq!(std::fs::read(&config).unwrap(), original_config);
    }
    // An external file edit cannot authorize a host that the live source never used.
    let mut drifted: Value = serde_json::from_slice(&original_config).unwrap();
    drifted["providers"][0]["base"] = json!(different_host);
    let drifted = serde_json::to_vec(&drifted).unwrap();
    std::fs::write(&config, &drifted).unwrap();
    assert_eq!(
        edit()
            .json(&json!({"models":["after"],"base":different_host}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(std::fs::read(&config).unwrap(), drifted);
    std::fs::write(&config, &original_config).unwrap();
    let temp = config.with_extension("json.tmp");
    std::fs::create_dir(&temp).unwrap();
    assert_eq!(
        edit()
            .json(&json!({"models":["failed"],"kind":"subscription","base":format!("{upstream}/unsaved")}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    std::fs::remove_dir(temp).unwrap();
    assert_eq!(std::fs::read(&config).unwrap(), original_config);
    let overview: Value = providers(&base).send().await.unwrap().json().await.unwrap();
    assert_eq!(overview["providers"][0]["id"], "local");
    assert_eq!(overview["providers"][0]["kind"], "free_tier");
    assert_eq!(overview["providers"][0]["base"], original_base);
    let changed: Value = edit()
        .json(&json!({"models":[" after ","after"," other "],"kind":"subscription","base":format!(" {upstream}/ ")}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["models"], json!(["after", "other"]));
    let discover = |base: &str| {
        client
            .get(format!("{base}/v1/models"))
            .bearer_auth("aubridge-opencode")
    };
    let models: Value = discover(&base).send().await.unwrap().json().await.unwrap();
    let models: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        models.contains(&"after") && !models.contains(&"before") && !models.contains(&"failed")
    );
    let overview: Value = providers(&base).send().await.unwrap().json().await.unwrap();
    assert_eq!(overview["providers"][0]["kind"], "subscription");
    assert_eq!(overview["providers"][0]["base"], upstream);
    edit()
        .json(&json!({"models":["after"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let (_, base, second) = gateway(state()).await;
    let overview: Value = providers(&base).send().await.unwrap().json().await.unwrap();
    assert_eq!(overview["providers"][0]["kind"], "subscription");
    assert_eq!(overview["providers"][0]["base"], upstream);
    assert_eq!(overview["providers"][0]["models"], json!(["after"]));
    assert_eq!(overview["providers"][0]["key_set"], true);
    assert_eq!(
        messages(&client, &base, "before", false)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        messages(&client, &base, "after", false)
            .send()
            .await
            .unwrap()
            .headers()["x-aubridge-provider"],
        "local"
    );
    let seen = replay.seen_headers.lock().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["authorization"], "Bearer fixture-key");
    assert_eq!(
        serde_json::from_str::<Value>(&replay.seen_bodies.lock().await[0]).unwrap()["model"],
        "after"
    );
    for task in [second, upstream_task] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn custom_model_edit_ignores_failure_from_an_old_base_path() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new()
        .route(
            "/stale/chat/completions",
            post({
                let started = started.clone();
                let release = release.clone();
                move |headers: HeaderMap| {
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        assert_eq!(headers["authorization"], "Bearer fixture-key");
                        started.notify_one();
                        release.notified().await;
                        (
                            StatusCode::FORBIDDEN,
                            [("content-type", "application/json")],
                            r#"{"error":{"message":"account has been suspended"}}"#,
                        )
                    }
                }
            }),
        )
        .route(
            "/fixed/chat/completions",
            post(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                ([("content-type", "text/event-stream")], CHAT_TOOL)
            }),
        );
    let (upstream, upstream_task) = http_app(upstream).await;
    let dir = std::env::temp_dir().join(format!("aubridge-edit-base-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = GatewayState::from_manifests(BTreeMap::new(), "aubridge")
        .with_settings(dir.join("config.json"), dir.join("stash"))
        .with_key_store(Arc::new(credentials::MemoryStore::default()))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, task) = gateway(state).await;
    let admin = |path: &str| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
    };
    admin("providers")
        .json(&json!({"id":"local","base":format!("{upstream}/stale"),"models":[MODEL],"key":"fixture-key"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pending = messages(&client, &base, MODEL, false);
    let request = tokio::spawn(async move { pending.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    admin("providers/local/models")
        .json(&json!({"models":[MODEL],"base":format!("{upstream}/fixed")}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    release.notify_one();
    assert_eq!(request.await.unwrap().status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        latest(&client, &base).await["recent_decisions"][0]["attempts"][0]["reason"],
        "access-denied"
    );
    let reply = messages(&client, &base, MODEL, false).send().await.unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["x-aubridge-provider"], "local");
    for task in [task, upstream_task] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn gateway_history_filters_and_pages_same_second_records() {
    let dir = std::env::temp_dir().join(format!("aubridge-history-{}", std::process::id()));
    let db = dir.join("ledger.sqlite");
    let ledger = ledger::Ledger::open(&db).unwrap();
    let row = |index: u32, agent: &str, provider: &str, outcome: &str| ledger::CompletedRequest {
        decision: ledger::DecisionRecord {
            ts: 123,
            model: format!("request-{index}"),
            served_model: (provider == "kimi-code" && outcome == "ok")
                .then(|| "vendor/plan:model".into()),
            agent: agent.into(),
            provider: provider.into(),
            outcome: outcome.into(),
            attempts: if outcome == "ok" {
                let kind = if provider == "kimi-code" {
                    "subscription"
                } else {
                    "free_tier"
                };
                let prior = if index.is_multiple_of(2) {
                    "fixture-free"
                } else {
                    "other-plan"
                };
                json!([
                    {"id":prior,"status":503,"chosen":false},
                    {"id":provider,"kind":kind,"status":200,"chosen":true}
                ])
                .to_string()
            } else {
                r#"[{"id":"kimi-code","status":503}]"#.into()
            },
            skipped: (!index.is_multiple_of(2))
                .then(|| r#"[{"id":"fixture-free","reason":"credential-rejected"}]"#.into()),
            ..Default::default()
        },
        usage: vec![],
    };
    let mut rows: Vec<_> = (0..25)
        .map(|index| row(index, "opencode", "kimi-code", "ok"))
        .collect();
    rows.extend([
        row(25, "opencode", "openrouter", "ok"),
        row(26, "claude-code", "kimi-code", "ok"),
        row(27, "opencode", "kimi-code", "stream-error"),
        row(28, "opencode", "kimi-code", "cancelled"),
    ]);
    rows[25].decision.model = "daily/coding".into();
    ledger.record_requests(&rows).unwrap();
    let state = GatewayState::from_manifests(BTreeMap::new(), "aubridge")
        .with_ledger(ledger::Ledger::open(&db).unwrap());
    let (client, base, task) = gateway(state).await;
    let url = format!("{base}/admin/gateway");
    assert_eq!(
        client
            .get(format!("{url}?before=bad"))
            .bearer_auth("aubridge-opencode")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let admin = |query: &str| {
        client
            .get(format!("{url}{query}"))
            .bearer_auth("aubridge-admin")
    };
    for invalid in [
        "?before=bad",
        "?before=123:0",
        "?outcome=other",
        "?kind=other",
    ] {
        assert_eq!(
            admin(invalid).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    let page = |query: String| {
        let request = admin(&query);
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let default = page(String::new()).await;
    assert_eq!(default["requests"].as_array().unwrap().len(), 20);
    assert_eq!(default["requests"][0]["model"], "request-28");
    assert_eq!(
        default["requests"][0].get("duration_ms"),
        Some(&Value::Null)
    );
    assert_eq!(
        default["requests"][0].get("first_content_ms"),
        Some(&Value::Null)
    );
    assert_eq!(default["serving"], true);
    let alias = page("?model=daily%2Fcoding".into()).await;
    assert_eq!(alias["requests"].as_array().unwrap().len(), 1);
    assert_eq!(alias["requests"][0]["model"], "daily/coding");
    assert!(alias["requests"][0]["served_model"].is_null());
    let partial = page("?model=vendor".into()).await;
    assert!(partial["requests"].as_array().unwrap().is_empty());
    let related_prefix = page("?related_provider=fixture".into()).await;
    assert!(related_prefix["requests"].as_array().unwrap().is_empty());
    let selected = page("?provider=fixture-free".into()).await;
    assert!(selected["requests"].as_array().unwrap().is_empty());
    for model in ["request-0", "request-1"] {
        let related = page(format!("?related_provider=fixture-free&model={model}")).await;
        assert_eq!(related["requests"].as_array().unwrap().len(), 1);
        assert_eq!(related["requests"][0]["provider"], "kimi-code");
    }

    let filters = "?agent=opencode&provider=kimi-code&outcome=ok&kind=subscription&model=vendor%2Fplan%3Amodel&related_provider=fixture-free";
    let first = page(filters.into()).await;
    let next = first["next"].as_str().unwrap();
    assert_eq!(first["requests"].as_array().unwrap().len(), 20);
    assert_eq!(first["requests"][0]["model"], "request-24");
    assert_eq!(first["requests"][19]["model"], "request-5");
    ledger
        .record_requests(&[row(29, "opencode", "kimi-code", "ok")])
        .unwrap();
    let older = page(format!("{filters}&before={next}")).await;
    let models: Vec<_> = first["requests"]
        .as_array()
        .unwrap()
        .iter()
        .chain(older["requests"].as_array().unwrap())
        .map(|r| r["model"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        (0..25)
            .rev()
            .map(|index| format!("request-{index}"))
            .collect::<Vec<_>>()
    );
    assert!(older["next"].is_null());
    for (filter, outcome) in [("failed", "stream-error"), ("cancelled", "cancelled")] {
        let result = page(format!("?outcome={filter}")).await;
        assert_eq!(result["requests"].as_array().unwrap().len(), 1);
        assert_eq!(result["requests"][0]["outcome"], outcome);
    }

    let mut legacy_rows: Vec<_> = [
        "legacy text",
        r#"["old item",{"id":"kimi-code","status":200}]"#,
        r#"{"unexpected":{"id":"kimi-code","kind":"subscription","chosen":true}}"#,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, attempts)| {
        let mut record = row(30 + index as u32, "opencode", "kimi-code", "ok");
        record.decision.attempts = attempts.into();
        record.decision.skipped = Some(r#"[1,"legacy",{"meta":{"id":"fixture-free"}}]"#.into());
        record
    })
    .collect();
    let mut unchosen = row(33, "opencode", "", "cancelled");
    unchosen.decision.attempts =
        r#"[{"id":"kimi-code","kind":"subscription","chosen":false}]"#.into();
    legacy_rows.push(unchosen);
    ledger.record_requests(&legacy_rows).unwrap();
    let all = page(String::new()).await;
    assert_eq!(all["requests"][0]["model"], "request-33");
    assert_eq!(all["requests"][3]["attempts"], "legacy text");
    let plan = page("?kind=subscription".into()).await;
    assert_eq!(plan["requests"][0]["model"], "request-29");
    let free = page("?kind=free_tier".into()).await;
    assert_eq!(free["requests"].as_array().unwrap().len(), 1);
    assert_eq!(free["requests"][0]["model"], "daily/coding");
    let legacy = page("?related_provider=kimi-code&model=request-30".into()).await;
    assert_eq!(legacy["requests"][0]["provider"], "kimi-code");
    let unrelated = page("?related_provider=fixture-free&model=request-30".into()).await;
    assert!(unrelated["requests"].as_array().unwrap().is_empty());

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(ledger);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn rejected_static_key_skips_all_models_until_credentials_are_saved() {
    let reply = || Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (free, free_url, free_task) = spawn(vec![
        Step::Fail {
            status: 401,
            body: "{}".into(),
        },
        reply(),
    ])
    .await;
    let (_, plan_url, plan_task) = spawn(vec![reply(), reply()]).await;
    let (id, mut free_manifest) = source("free", &free_url, "free-a", "free_tier", "chat");
    free_manifest.free_models.push("free-b".into());
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            (id, free_manifest),
            source("plan", &plan_url, "plan-model", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_key_store(Arc::new(credentials::MemoryStore::default()))
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    state.set_provider_key("free", "rejected-fixture").unwrap();
    let (client, base, server) = gateway(state).await;
    let model = "group/free-a+free-b+plan-model";
    for skipped in [1, 2] {
        let result = messages(&client, &base, model, false).send().await.unwrap();
        assert_eq!(result.headers()["x-aubridge-provider"], "plan");
        result.bytes().await.unwrap();
        let history = latest(&client, &base).await;
        let row = &history["recent_decisions"][0];
        let entries = row["skipped"].as_array().unwrap();
        assert_eq!(entries.len(), skipped);
        assert!(entries.iter().all(|entry| entry["id"] == "free"
            && entry["reason"] == "credential-rejected"
            && entry["retry_at"].as_u64().unwrap() > row["ts"].as_u64().unwrap()));
    }
    assert_eq!(free.seen_bodies.lock().await.len(), 1);
    client
        .post(format!("{base}/admin/keys"))
        .bearer_auth("aubridge-admin")
        .json(&json!({"id":"free","key":"replacement-fixture"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let result = messages(&client, &base, model, false).send().await.unwrap();
    assert_eq!(result.headers()["x-aubridge-provider"], "free");
    result.bytes().await.unwrap();
    let headers = free.seen_headers.lock().await;
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0]["authorization"], "Bearer rejected-fixture");
    assert_eq!(headers[1]["authorization"], "Bearer replacement-fixture");
    for task in [server, free_task, plan_task] {
        task.abort();
    }
}

#[tokio::test]
async fn provider_routing_status_projects_subscription_model_and_account_blocks() {
    for account in [false, true] {
        let reset = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let (limited, limited_base, limited_task) = spawn(vec![Step::FailWithHeaders {
            status: 429,
            body: json!({"error":{"code":if account {"quota_exceeded"} else {"model_rate_limit_exceeded"},
                "scope":if account {"account"} else {"model"}}}).to_string(),
            headers: vec![
                ("retry-after".into(), "120".into()),
                ("x-ratelimit-reset".into(), reset.to_string()),
            ],
        }]).await;
        let (backup, backup_base, backup_task) = spawn(vec![Step::Sse {
            body: CHAT_TOOL.into(),
        }])
        .await;
        let (id, mut manifest) = source(
            "limited",
            &limited_base,
            "limited-model",
            "subscription",
            "chat",
        );
        manifest.free_models.push("sibling-model".into());
        let dir = std::env::temp_dir().join(format!(
            "aubridge-routing-status-{}-{account}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([
                    (id, manifest),
                    source(
                        "backup",
                        &backup_base,
                        "backup-model",
                        "subscription",
                        "chat",
                    ),
                    source("free", &backup_base, "free-model", "free_tier", "chat"),
                    source(
                        "disabled",
                        &backup_base,
                        "disabled-model",
                        "subscription",
                        "chat",
                    ),
                    source(
                        "missing",
                        "https://routing-status.invalid",
                        "missing-model",
                        "subscription",
                        "chat",
                    ),
                ]),
                "aubridge",
            )
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_settings(dir.join("config.json"), dir.join("stash"))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
        );
        state.apply_failure(
            "disabled",
            &router::UpstreamSignal {
                status: 403,
                account_restricted: true,
                ..Default::default()
            },
            reset - 600,
            0,
        );
        state.apply_failure(
            "missing",
            &router::UpstreamSignal {
                status: 429,
                retry_after: Some(120),
                ..Default::default()
            },
            reset - 600,
            0,
        );
        let (base, task) = http_app(gateway::app(state)).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap();
        let get = || async {
            client
                .get(format!("{base}/admin/providers"))
                .bearer_auth("aubridge-admin")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        };
        let reply = messages(&client, &base, "group/limited-model+backup-model", true)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(reply.headers()["x-aubridge-provider"], "backup");
        reply.bytes().await.unwrap();
        let history = latest(&client, &base).await["recent_decisions"].clone();
        let view = get().await;
        let rows = view["providers"].as_array().unwrap();
        let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
        let statuses = row("limited")["routing_status"]
            .as_array()
            .expect("subscription routing_status");
        assert_eq!(statuses.len(), 2);
        let reason = if account {
            "account-quota-exhausted"
        } else {
            "model-rate-limit"
        };
        assert_eq!(statuses[0]["model"], "limited-model");
        assert_eq!(statuses[0]["status"], reason);
        assert!(statuses[0]["retry_at"].as_u64().unwrap() >= reset - 480);
        assert_eq!(
            statuses[1]["status"],
            if account { reason } else { "eligible" }
        );
        assert_eq!(statuses[1]["retry_at"].is_null(), !account);
        assert!(row("limited")["free_requests"].is_null());
        assert_eq!(
            row("backup")["routing_status"],
            json!([{"model":"backup-model","status":"eligible","retry_at":null}])
        );
        assert_eq!(
            row("disabled")["routing_status"],
            json!([{"model":"disabled-model","status":"provider-disabled","retry_at":null}])
        );
        assert_eq!(
            row("missing")["routing_status"],
            json!([{"model":"missing-model","status":"missing-credential","retry_at":null}])
        );
        assert_eq!(
            row("free")["free_requests"],
            json!([{"model":"free-model","status":"eligible","retry_at":null}])
        );
        client
            .post(format!("{base}/admin/providers/limited/pause"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"paused":true}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let paused = get().await;
        let paused = paused["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "limited")
            .unwrap();
        assert_eq!(
            paused["routing_status"],
            json!([
                {"model":"limited-model","status":"provider-paused","retry_at":null},
                {"model":"sibling-model","status":"provider-paused","retry_at":null},
            ])
        );
        get().await;
        assert_eq!(latest(&client, &base).await["recent_decisions"], history);
        assert_eq!(limited.seen_bodies.lock().await.len(), 1);
        assert_eq!(backup.seen_bodies.lock().await.len(), 1);
        for server in [limited_task, backup_task, task] {
            server.abort();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn group_preview_reports_current_candidates_without_advancing_or_sending() {
    let success = || Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (free, free_base, free_task) = spawn(vec![
        success(),
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":{"code":"model_rate_limit_exceeded"}}"#.into(),
            headers: vec![("retry-after".into(), "90".into())],
        },
    ])
    .await;
    let (paid, paid_base, paid_task) = spawn(vec![success(), success()]).await;
    let dir = std::env::temp_dir().join(format!("aubridge-group-preview-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (free_id, mut free_manifest) = source(
        "preview-free",
        &free_base,
        "free-model",
        "free_tier",
        "chat",
    );
    free_manifest
        .models_without_tools
        .insert("free-model".into());
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            (free_id, free_manifest),
            source(
                "preview-paid",
                &paid_base,
                "plan-model",
                "subscription",
                "chat",
            ),
            source(
                "preview-missing",
                "https://preview-missing.invalid",
                "missing-model",
                "free_tier",
                "chat",
            ),
        ]),
        "aubridge",
    )
    .with_settings(dir.join("config.json"), dir.join("stash"))
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    for (name, rule) in [
        ("daily/preview", "free_first"),
        (".", "free_first"),
        ("..", "free_first"),
        ("free-only", "free_only"),
        ("rotation", "round_robin"),
    ] {
        state
            .upsert_model_group(&credentials::settings::ModelGroup {
                name: name.into(),
                members: vec![
                    "plan-model".into(),
                    "free-model".into(),
                    "missing-model".into(),
                ],
                rule: rule.into(),
            })
            .unwrap();
    }
    let (client, base, task) = gateway(state).await;
    let preview_url = format!("{base}/admin/routing/preview");
    let request = |name: &str| client.get(&preview_url).query(&[("model", name)]);
    let read_preview = |request: reqwest::RequestBuilder| {
        let request = request.bearer_auth("aubridge-admin");
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let preview = |name: &str| read_preview(request(name));
    let tool_preview = |name: &str| read_preview(request(name).query(&[("tools", "true")]));
    let text_request = |model: &str| {
        client
            .post(format!("{base}/v1/messages"))
            .bearer_auth("aubridge-claude-code")
            .json(&json!({
                "model":model,"max_tokens":80,"stream":false,
                "messages":[{"role":"user","content":"hello"}],
            }))
    };
    let group = "daily/preview";
    assert_eq!(
        request(group)
            .bearer_auth("aubridge-claude-code")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&preview_url)
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    for name in ["unknown", "free-model", "group/free-model", "aubridge"] {
        assert_eq!(
            request(name)
                .bearer_auth("aubridge-admin")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    let free_candidate = json!({"id":"preview-free","model":"free-model","kind":"free_tier"});
    let paid_candidate = json!({"id":"preview-paid","model":"plan-model","kind":"subscription"});
    let ready = preview(group).await;
    assert_eq!(ready["model"], group);
    assert_eq!(ready["candidates"], json!([free_candidate, paid_candidate]));
    assert_eq!(
        ready["skipped"],
        json!([{"id":"preview-missing","model":"missing-model","kind":"free_tier",
            "reason":"missing-credential"}])
    );
    let tools = tool_preview(group).await;
    assert_eq!(tools["candidates"], json!([paid_candidate]));
    assert!(tools["skipped"].as_array().unwrap().iter().any(|item| item
        == &json!({
            "id":"preview-free","model":"free-model","kind":"free_tier",
            "reason":"tools-unsupported"
        })));
    assert_eq!(
        read_preview(request(group).query(&[("tools", "false")])).await,
        ready
    );
    assert_eq!(
        request(group)
            .query(&[("tools", "invalid")])
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    for dot_name in [".", ".."] {
        let dot = preview(dot_name).await;
        assert_eq!(dot["model"], dot_name);
        assert_eq!(dot["candidates"], ready["candidates"]);
    }
    let free_only = preview("free-only").await;
    assert_eq!(free_only["candidates"], json!([free_candidate]));
    assert!(free_only["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == "preview-paid" && item["reason"] == "subscription-excluded"));

    let set_serving = |serving| {
        client
            .post(format!("{base}/admin/gateway"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"serving":serving}))
            .send()
    };
    let rotation_before = preview("rotation").await;
    set_serving(false)
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        text_request("rotation").send().await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    for (mut stopped, active) in [
        (preview(group).await, &ready),
        (tool_preview(group).await, &tools),
    ] {
        assert_eq!(stopped["serving"], false);
        stopped["serving"] = json!(true);
        assert_eq!(&stopped, active);
    }
    set_serving(true).await.unwrap().error_for_status().unwrap();
    assert_eq!(ready["serving"], true);
    assert_eq!(tools["serving"], true);
    assert_eq!(preview(group).await, ready);
    assert_eq!(tool_preview(group).await, tools);
    assert_eq!(preview("rotation").await, rotation_before);

    assert!(free.seen_bodies.lock().await.is_empty());
    assert!(paid.seen_bodies.lock().await.is_empty());
    assert_eq!(latest(&client, &base).await["recent_decisions"], json!([]));

    for (order, id) in [
        (json!([paid_candidate, free_candidate]), "preview-paid"),
        (json!([free_candidate, paid_candidate]), "preview-free"),
    ] {
        for _ in 0..2 {
            assert_eq!(
                tool_preview("rotation").await["candidates"],
                json!([paid_candidate])
            );
            assert_eq!(preview("rotation").await["candidates"], order);
        }
        let response = text_request("rotation").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-aubridge-provider"], id);
        response.bytes().await.unwrap();
    }

    for paused in [true, false] {
        client
            .post(format!("{base}/admin/providers/preview-free/pause"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"paused":paused}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let state = preview(group).await;
        if paused {
            assert_eq!(state["candidates"], json!([paid_candidate]));
            assert!(state["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"] == "preview-free" && item["reason"] == "provider-paused"));
        } else {
            assert_eq!(state, ready);
        }
    }

    let response = text_request(group).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-aubridge-provider"], "preview-paid");
    response.bytes().await.unwrap();
    let limited = preview(group).await;
    assert_eq!(limited["candidates"], json!([paid_candidate]));
    let cooling = limited["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "preview-free")
        .unwrap();
    assert_eq!(cooling["reason"], "model-rate-limit");
    assert!(cooling["retry_at"].is_u64());
    assert_eq!(preview("free-only").await["candidates"], json!([]));
    assert_eq!(free.seen_bodies.lock().await.len(), 2);
    assert_eq!(paid.seen_bodies.lock().await.len(), 2);
    let history = latest(&client, &base).await;
    assert_eq!(history["recent_decisions"].as_array().unwrap().len(), 3);

    for server in [task, free_task, paid_task] {
        server.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn named_group_order_takes_precedence_over_session_affinity() {
    let success = || Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (first, first_url, first_task) = spawn(vec![
        success(),
        Step::Fail {
            status: 503,
            body: "{}".into(),
        },
        success(),
    ])
    .await;
    let (second, second_url, second_task) = spawn(vec![success(), success(), success()]).await;
    let dir = std::env::temp_dir().join(format!("aubridge-group-order-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("first", &first_url, "plan-a", "subscription", "chat"),
            source("second", &second_url, "plan-b", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_settings(dir.join("config.json"), dir.join("stash"))
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    state.record_affinity("known-session", "second", 2048);
    let (client, base, task) = gateway(state).await;
    for rule in ["sequential", "round_robin", "free_first"] {
        client
            .post(format!("{base}/admin/model-groups"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"name":rule,"rule":rule,"members":["plan-a","plan-b"]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    for (group, expected) in [
        ("sequential", "first"),
        ("sequential", "second"),
        ("round_robin", "first"),
        ("round_robin", "second"),
        ("free_first", "second"),
    ] {
        let response = messages(&client, &base, group, false)
            .header("x-session-id", "known-session")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            expected,
            "{group}"
        );
        response.bytes().await.unwrap();
    }
    assert_eq!(first.seen_bodies.lock().await.len(), 3);
    assert_eq!(second.seen_bodies.lock().await.len(), 3);
    for server in [task, first_task, second_task] {
        server.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn confirmed_context_error_only_falls_back_to_a_known_wider_window() {
    let cases = [
        (
            "chat",
            400,
            Step::Fail {
                status: 400,
                body: r#"{"error":{"code":"context_length_exceeded"}}"#.into(),
            },
        ),
        (
            "chat",
            200,
            Step::Sse {
                body: "data: {\"error\":{\"code\":\"context_window_exceeded\"}}\n\n".into(),
            },
        ),
        (
            "responses",
            200,
            Step::Sse {
                body: sse_frames(&[json!({"type":"response.failed","response":{
                    "id":"resp_context","error":{"code":"context_length_exceeded"}}})]),
            },
        ),
    ];
    for (protocol, status, context) in cases {
        let (limited, limited_url, limited_task) = spawn(vec![context.clone(), context]).await;
        let (skipped, skipped_url, skipped_task) = spawn(vec![]).await;
        let (wide, wide_url, wide_task) = spawn(vec![Step::Sse {
            body: if protocol == "chat" {
                CHAT_TOOL.into()
            } else {
                CODEX_REPLY.into()
            },
        }])
        .await;
        let with_window = |id: &str, url: &str, kind: &str, window| {
            let (key, mut manifest) = source(id, url, id, kind, protocol);
            manifest.context_windows.insert(id.to_string(), window);
            (key, manifest)
        };
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                with_window("limited", &limited_url, "free_tier", 8192),
                with_window("same", &skipped_url, "free_tier", 8192),
                source(
                    "unknown-window",
                    &skipped_url,
                    "unknown-window",
                    "free_tier",
                    protocol,
                ),
                with_window("wide", &wide_url, "subscription", 32768),
            ]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let (client, base, task) = gateway(state).await;
        let turn = |model: &str| {
            if protocol == "chat" {
                messages(&client, &base, model, false)
            } else {
                post_responses(
                    &client,
                    &base,
                    &json!({"model":model,"input":"reply briefly","stream":true}),
                )
            }
        };
        let reply = turn("group/limited+same+unknown-window+wide")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.headers()["x-aubridge-provider"], "wide");
        reply.text().await.unwrap();
        let history = latest(&client, &base).await;
        let row = &history["recent_decisions"][0];
        assert_eq!(row["outcome"], "ok");
        assert_eq!(row["attempts"][0]["reason"], "context_length_exceeded");
        assert_eq!(row["attempts"][0]["status"], status);
        let skipped_entries = row["skipped"].as_array().unwrap();
        assert_eq!(skipped_entries.len(), 2);
        for entry in skipped_entries {
            assert_eq!(entry["reason"], "context-fallback-window");
            assert!(entry["estimated_input_tokens"].as_u64().unwrap() > 0);
            if entry["id"] == "same" {
                assert_eq!(entry["context_window"], 8192);
            } else {
                assert_eq!(entry["id"], "unknown-window");
                assert!(entry.get("context_window").is_none());
            }
        }
        let reply = turn("group/limited+same+unknown-window")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 400);
        if protocol == "responses" {
            assert_eq!(
                reply.json::<Value>().await.unwrap()["error"]["code"],
                "context_length_exceeded"
            );
        }
        let history = latest(&client, &base).await;
        assert_eq!(
            history["recent_decisions"][0]["outcome"],
            "context_length_exceeded"
        );
        assert!(skipped.seen_bodies.lock().await.is_empty());
        assert_eq!(wide.seen_bodies.lock().await.len(), 1);
        assert_eq!(limited.seen_bodies.lock().await.len(), 2);
        for task in [task, limited_task, skipped_task, wide_task] {
            task.abort();
        }
    }
}

#[tokio::test]
async fn context_preflight_defers_small_models_and_keeps_them_for_last_resort() {
    let reply = || Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (small, small_url, small_task) = spawn(vec![
        reply(),
        reply(),
        Step::Fail {
            status: 400,
            body: r#"{"error":{"code":"context_length_exceeded"}}"#.into(),
        },
    ])
    .await;
    let (_, wide_url, wide_task) = spawn(vec![
        reply(),
        Step::Fail {
            status: 503,
            body: String::new(),
        },
    ])
    .await;
    let with_window = |id: &str, url: &str, model: &str, kind: &str, window| {
        let (id, source) = source(id, url, model, kind, "chat");
        let mut manifest = serde_json::to_value(source).unwrap();
        manifest["context_windows"] = json!({(model):window});
        (id, serde_json::from_value(manifest).unwrap())
    };
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            with_window("small", &small_url, "tiny-model", "free_tier", 128),
            with_window("wide", &wide_url, "wide-model", "subscription", 4096),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let long = "long input with distinct words ".repeat(40);
    let group = "group/tiny-model+wide-model";
    let post = |model: &str, text: &str| {
        client
            .post(format!("{base}/v1/messages"))
            .bearer_auth("aubridge-claude-code")
            .json(&json!({
                "model":model, "max_tokens":64, "stream":false,
                "messages":[{"role":"user","content":text}],
            }))
            .send()
    };

    let response = post(group, &long).await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "wide");
    assert!(small.seen_bodies.lock().await.is_empty());
    let row = &latest(&client, &base).await["recent_decisions"][0];
    let skipped = &row["skipped"][0];
    assert_eq!(skipped["reason"], "context-window-too-small");
    assert_eq!(skipped["context_window"], 128);
    assert!(skipped["estimated_input_tokens"].as_u64().unwrap() > 128);

    let short = post(group, "short input").await.unwrap();
    assert_eq!(short.headers()["x-aubridge-provider"], "small");
    let response = post(group, &long).await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "small");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["id"], "wide");
    assert_eq!(row["attempts"][1]["id"], "small");
    assert_eq!(row["skipped"], json!([]));

    let response = post("tiny-model", &long).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["outcome"], "context_length_exceeded");
    assert_eq!(row["skipped"], json!([]));
    for task in [gateway_task, small_task, wide_task] {
        task.abort();
    }
}

fn post_responses(client: &reqwest::Client, base: &str, input: &Value) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/responses"))
        .bearer_auth("aubridge-codex")
        .json(input)
}

#[tokio::test]
async fn responses_string_input_is_normalized_for_native_upstreams() {
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CODEX_REPLY.into(),
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([source(
            "native",
            &upstream,
            "native-model",
            "free_tier",
            "responses",
        )]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let reply = post_responses(
        &client,
        &base,
        &json!({"model":"native-model","stream":true,"input":"只回复两个字:收到"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(reply.text().await.unwrap().contains("response.completed"));

    let body: Value = serde_json::from_str(&replay.seen_bodies.lock().await[0]).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(
        body["input"],
        json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "只回复两个字:收到"}],
        }])
    );

    gateway_task.abort();
    upstream_task.abort();
}

fn sse_frames(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| proto::sse::frame_event(event["type"].as_str().unwrap(), &event.to_string()))
        .collect()
}

#[tokio::test]
async fn responses_done_alias_preserves_the_terminal_status() {
    let done = |status: &str, with_text: bool, named_only: bool| {
        let mut response = json!({
            "id":"resp_alias","model":"alias-model","status":status,
            "usage":{"input_tokens":11,"output_tokens":3},
        });
        if status == "failed" {
            response["error"] =
                json!({"code":"model_rate_limit_exceeded","message":"fixture limit"});
        } else if status == "incomplete" {
            response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        let event = if named_only {
            json!({"response":response})
        } else {
            json!({"type":"response.done","response":response})
        };
        let mut body = if with_text {
            sse_frames(&[json!({"type":"response.output_text.delta","delta":"alias reply"})])
        } else {
            String::new()
        };
        body.push_str(&proto::sse::frame_event(
            "response.done",
            &event.to_string(),
        ));
        Step::Sse { body }
    };
    let (first, first_url, first_task) = spawn(vec![
        done("completed", true, false),
        done("completed", true, true),
        done("incomplete", true, false),
        done("in_progress", false, false),
        done("failed", false, false),
    ])
    .await;
    let (backup, backup_url, backup_task) = spawn(vec![Step::Sse {
        body: CODEX_REPLY.into(),
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("alias", &first_url, "alias-model", "free_tier", "responses"),
            source(
                "backup",
                &backup_url,
                "plan-model",
                "subscription",
                "responses",
            ),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let request = |model: &str| {
        post_responses(
            &client,
            &base,
            &json!({"model":model,"input":"hello","stream":true}),
        )
    };
    let completed = request("alias-model")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(completed.contains("event: response.completed") && completed.contains("alias reply"));
    assert!(!completed.contains("response.done"));
    let row = latest(&client, &base).await;
    assert_eq!(row["recent_decisions"][0]["outcome"], "ok");
    assert_eq!(row["recent_decisions"][0]["output_tokens"], 3);

    let translated = messages(&client, &base, "alias-model", true)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        translated.contains("alias reply") && translated.contains("\"stop_reason\":\"end_turn\"")
    );
    assert!(translated.contains("event: message_stop") && !translated.contains("event: error"));
    let partial = request("alias-model")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(partial.contains("event: response.incomplete"));
    assert_eq!(
        latest(&client, &base).await["recent_decisions"][0]["outcome"],
        "incomplete"
    );

    assert_eq!(request("alias-model").send().await.unwrap().status(), 502);
    let response = request("group/alias-model+plan-model")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "backup");
    response.bytes().await.unwrap();
    let row = latest(&client, &base).await;
    assert_eq!(
        row["recent_decisions"][0]["attempts"][0]["reason"],
        "model-rate-limit"
    );
    assert_eq!(row["recent_decisions"][0]["attempts"][0]["status"], 200);
    assert_eq!(first.seen_bodies.lock().await.len(), 5);
    assert_eq!(backup.seen_bodies.lock().await.len(), 1);
    for task in [gateway_task, first_task, backup_task] {
        task.abort();
    }
}

fn anth_start() -> Value {
    json!({"type":"message_start", "message":{"id":"msg_fixture", "model":"plan-resp",
        "role":"assistant", "content":[], "usage":{"input_tokens":12, "cache_read_input_tokens":3}}})
}

fn anth_finish(reason: &str) -> [Value; 2] {
    [
        json!({"type":"message_delta", "delta":{"stop_reason":reason},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}

#[tokio::test]
async fn responses_tools_roundtrip_from_empty_free_source_to_messages() {
    let empty = || Step::Sse {
        body: String::new(),
    };
    let (free, free_base, free_task) = spawn(vec![empty(), empty()]).await;
    let mut tools = vec![anth_start()];
    for (index, id, name, args) in [
        (0, "call_read", "desk__read", r#"{"path":"probe"}"#),
        (1, "call_custom", "write_patch", r#"{"input":"demo patch"}"#),
    ] {
        tools.extend([
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args}}),
            json!({"type":"content_block_stop","index":index}),
        ]);
    }
    tools.extend(anth_finish("tool_use"));
    let mut text = vec![
        anth_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"file read"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    text.extend(anth_finish("end_turn"));
    let (paid, paid_base, paid_task) = spawn(vec![
        Step::Sse {
            body: sse_frames(&tools),
        },
        Step::Sse {
            body: sse_frames(&text),
        },
    ])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("free", &free_base, "free-resp", "free_tier", "chat"),
            source("plan", &paid_base, "plan-resp", "subscription", "anthropic"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, task) = gateway(state).await;
    let mut input = json!({
        "model":"group/plan-resp+free-resp", "stream":true,"store":false,
        "input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"Use tools"}]},
            {"type":"message","role":"user","content":"Read a file"}],
        "tools":[{"type":"namespace","name":"desk","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]},
                 {"type":"custom","name":"write_patch","format":{"type":"text"}}],
    });
    let response = post_responses(&client, &base, &input).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    let response = response.text().await.unwrap();
    let events: Vec<Value> = proto::sse::parse(&response)
        .0
        .iter()
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["type"] == "response.function_call_arguments.delta"));
    let completed = &events.last().unwrap()["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["end_turn"], false);
    assert_eq!(completed["usage"]["input_tokens"], 15);
    assert_eq!(
        completed["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );
    let outputs = completed["output"].as_array().unwrap();
    assert_eq!(outputs[0]["namespace"], "desk");
    assert_eq!(outputs[0]["name"], "read");
    assert_eq!(outputs[0]["arguments"], r#"{"path":"probe"}"#);
    assert_eq!(outputs[1]["type"], "custom_tool_call");
    assert_eq!(outputs[1]["input"], "demo patch");
    input["input"].as_array_mut().unwrap().extend([
        json!({"type":"reasoning","encrypted_content":"opaque-other-source"}),
        outputs[0].clone(), outputs[1].clone(),
        json!({"type":"function_call_output","call_id":"call_read","output":[{"type":"input_text","text":"fixture result"}]}),
        json!({"type":"custom_tool_call_output","call_id":"call_custom","output":"fixture custom output"}),
    ]);
    let response = post_responses(&client, &base, &input)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(response.contains("file read") && response.contains("response.completed"));
    let paid_bodies = paid.seen_bodies.lock().await;
    let second: Value = serde_json::from_str(&paid_bodies[1]).unwrap();
    assert_eq!(second["system"], "Use tools");
    assert_eq!(second["messages"][1]["content"][0]["name"], "desk__read");
    assert_eq!(
        second["messages"][1]["content"][1]["input"]["input"],
        "demo patch"
    );
    assert_eq!(
        second["messages"][2]["content"][0]["tool_use_id"],
        "call_read"
    );
    assert_eq!(
        second["messages"][2]["content"][1]["tool_use_id"],
        "call_custom"
    );
    assert!(!paid_bodies[1].contains("opaque-other-source"));
    assert!(!free.seen_bodies.lock().await[1].contains("opaque-other-source"));
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["agent"], "codex");
    assert_eq!(row["attempts"][0]["reason"], "stream-error");
    assert_eq!(row["output_tokens"], 7);
    for task in [task, free_task, paid_task] {
        task.abort();
    }
}

#[tokio::test]
async fn embedded_credential_error_cools_only_before_first_content() {
    for (protocol, error, committed, cools) in [
        (
            "chat",
            json!({"error":{"code":"invalid_api_key"}}),
            false,
            true,
        ),
        (
            "anthropic",
            json!({"type":"error","error":{"type":"authentication_error"}}),
            false,
            true,
        ),
        (
            "responses",
            json!({"type":"response.failed","response":{"error":{"status":401}}}),
            false,
            true,
        ),
        ("chat", json!({"error":{"code":"401"}}), true, false),
        (
            "chat",
            json!({"error":{"message":"401 invalid_api_key"}}),
            false,
            false,
        ),
    ] {
        let mut frames = Vec::new();
        if committed {
            frames.push(json!({"choices":[{"delta":{"content":"partial text"}}]}));
        }
        frames.push(error);
        let reply = || Step::Sse {
            body: CHAT_TOOL.into(),
        };
        let (free, free_url, free_task) = spawn(vec![
            Step::Sse {
                body: frames
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect(),
            },
            reply(),
        ])
        .await;
        let (plan, plan_url, plan_task) = spawn(vec![reply(), reply()]).await;
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                source("free", &free_url, "free-model", "free_tier", protocol),
                source("plan", &plan_url, "plan-model", "subscription", "chat"),
            ]),
            "aubridge",
        )
        .with_key_store(Arc::new(credentials::MemoryStore::default()))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        state.set_provider_key("free", "rejected-fixture").unwrap();
        let (client, base, task) = gateway(state).await;
        let request = || messages(&client, &base, "group/plan-model+free-model", true);
        let response = request().send().await.unwrap();
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if committed { "free" } else { "plan" }
        );
        let body = response.text().await.unwrap();
        let row = &latest(&client, &base).await["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 200);
        assert_eq!(
            row["attempts"][0]["reason"],
            if cools {
                "credential-rejected"
            } else {
                "stream-error"
            },
            "{protocol} committed={committed}"
        );
        if committed {
            assert!(body.contains("partial text") && body.contains("event: error"));
            assert!(plan.seen_bodies.lock().await.is_empty());
        }
        let response = request().send().await.unwrap();
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if cools { "plan" } else { "free" }
        );
        response.bytes().await.unwrap();
        if cools {
            let history = latest(&client, &base).await;
            assert_eq!(
                history["recent_decisions"][0]["skipped"][0]["reason"],
                "credential-rejected"
            );
        }
        assert_eq!(
            free.seen_bodies.lock().await.len(),
            if cools { 1 } else { 2 }
        );
        for server in [task, free_task, plan_task] {
            server.abort();
        }
    }
}

#[tokio::test]
async fn responses_embedded_rate_limit_cools_only_before_first_content() {
    for (native, committed) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut frames = vec![
            json!({"type":"response.created","response":{"id":"resp_replay","model":"limited"}}),
        ];
        if committed {
            frames.extend([
                json!({"type":"response.output_item.added","output_index":0,
                    "item":{"type":"message","id":"msg_replay","role":"assistant"}}),
                json!({"type":"response.output_text.delta","item_id":"msg_replay","output_index":0,"delta":"partial text"}),
            ]);
        }
        let error = if native {
            json!({"type":"response.failed","response":{"id":"resp_replay","error":{
                "code":"rate_limit_exceeded","scope":"account"}}})
        } else {
            json!({"type":"error","code":"rate_limit_exceeded","scope":"account"})
        };
        frames.push(error);
        let (limited, first, first_task) = spawn(vec![Step::SseWithHeaders {
            body: sse_frames(&frames),
            headers: vec![("retry-after".into(), "120".into())],
        }])
        .await;
        let reply = Step::Sse {
            body: CODEX_REPLY.into(),
        };
        let (backup, second, second_task) = spawn(vec![reply.clone(), reply]).await;
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                source("free", &first, "limited", "free_tier", "responses"),
                source("plan", &second, "backup", "subscription", "responses"),
            ]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let (client, base, task) = gateway(state).await;
        let model = "group/backup+limited";
        let request = || {
            if native {
                post_responses(
                    &client,
                    &base,
                    &json!({"model":model,"input":"hello","stream":true}),
                )
            } else {
                messages(&client, &base, model, true)
            }
        };
        let response = request().send().await.unwrap();
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if committed { "free" } else { "plan" }
        );
        let body = response.text().await.unwrap();
        let history = latest(&client, &base).await;
        let row = &history["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 200);
        if committed {
            assert!(body.contains("partial text"));
            if native {
                assert!(body.contains("response.failed"));
            } else {
                assert!(body.contains("event: error") && body.contains("message_stop"));
            }
            assert_eq!(row["outcome"], "stream-error");
            assert!(backup.seen_bodies.lock().await.is_empty());
        } else {
            assert_eq!(row["attempts"][0]["reason"], "account-rate-limit");
            assert_eq!(row["outcome"], "ok");
            let response = request().send().await.unwrap();
            assert_eq!(response.headers()["x-aubridge-provider"], "plan");
            response.text().await.unwrap();
            let history = latest(&client, &base).await;
            assert_eq!(
                history["recent_decisions"][0]["skipped"][0]["reason"],
                "account-rate-limit"
            );
            assert_eq!(backup.seen_bodies.lock().await.len(), 2);
        }
        assert_eq!(limited.seen_bodies.lock().await.len(), 1);
        for task in [task, first_task, second_task] {
            task.abort();
        }
    }
}

#[tokio::test]
async fn responses_committed_text_failure_stays_on_source_and_stateful_requests_are_rejected() {
    let (first, first_base, first_task) = spawn(vec![Step::Sse { body: sse_frames(&[
        anth_start(), json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial text"}}),
    ]) }]).await;
    let (second, second_base, second_task) = spawn(vec![]).await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("free", &first_base, "free-resp", "free_tier", "anthropic"),
            source("plan", &second_base, "plan-resp", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, task) = gateway(state).await;
    let input = json!({"model":"group/plan-resp+free-resp","input":"Read a file","stream":true});
    let unauthenticated = client
        .post(format!("{base}/v1/responses"))
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let mut stateful = input.clone();
    stateful["previous_response_id"] = json!("resp_old");
    let rejected = post_responses(&client, &base, &stateful)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(first.seen_bodies.lock().await.is_empty());
    let response = post_responses(&client, &base, &input).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let body = response.text().await.unwrap();
    assert!(body.contains("partial text") && body.contains("response.failed"));
    assert!(!body.contains("response.completed"));
    assert!(second.seen_bodies.lock().await.is_empty());
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["outcome"], "stream-error");
    assert!(row["output_tokens"].is_null());
    for task in [task, first_task, second_task] {
        task.abort();
    }
}

fn catalog_model(id: &str, tools: bool) -> Value {
    json!({
        "id": id,
        "pricing": {"prompt": "0", "completion": "0.000"},
        "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
        "supported_parameters": if tools { vec!["tools"] } else { vec![] },
        "context_length": 65536,
    })
}

#[tokio::test]
async fn directory_filters_models_retains_groups_and_survives_a_failed_refresh_and_restart() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WITH_TOOLS: &str = "vendor/tool-model:free";
    const TEXT_ONLY: &str = "vendor/text-model:free";
    const STATIC: &str = "vendor/old-static:free";
    let mut paid_variant = catalog_model("vendor/nonzero:free", true);
    paid_variant["pricing"]["request"] = json!("0.00000000001");
    let mut image_only = catalog_model("vendor/image-only:free", false);
    image_only["architecture"]["output_modalities"] = json!(["image"]);
    let mut unpriced = catalog_model("vendor/unpriced:free", true);
    unpriced["pricing"] = json!({"prompt":"0"});
    let mut full = json!({"data":[
        catalog_model(WITH_TOOLS, true), catalog_model(TEXT_ONLY, false),
        catalog_model("vendor/no-free-suffix", true), paid_variant, image_only, unpriced,
        catalog_model("vendor/ambiguous:free", true), catalog_model("vendor/ambiguous:free", false),
    ]});
    let directory = Arc::new(std::sync::Mutex::new(full.clone()));
    let directory_calls = Arc::new(AtomicUsize::new(0));
    let (directory_base, directory_task) = http_app(Router::new().route(
        "/models",
        axum::routing::get({
            let directory = directory.clone();
            let directory_calls = directory_calls.clone();
            move |headers: HeaderMap| {
                directory_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("x-api-key"));
                    axum::Json(directory.lock().unwrap().clone())
                }
            }
        }),
    ))
    .await;
    let tool = || Step::Sse {
        body: CHAT_TOOL.into(),
    };
    let (free, free_base, free_task) = spawn(vec![tool(), tool()]).await;
    let (paid, paid_base, paid_task) = spawn(vec![tool(), tool()]).await;
    let manifests = BTreeMap::from([
        source("openrouter", &free_base, STATIC, "free_tier", "chat"),
        source("plan", &paid_base, "plan-model", "subscription", "chat"),
    ]);
    let dir = std::env::temp_dir().join(format!("aubridge-model-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("config.json");
    std::fs::write(&settings, r#"{"future_fixture_setting":true}"#).unwrap();
    let database = dir.join("ledger.sqlite");
    let opencode = dir.join("opencode.jsonc");
    let agent_state = dir.join("agents");
    std::fs::write(
        &opencode,
        "{\n  // fixture preference\n  \"autoupdate\": false\n}\n",
    )
    .unwrap();
    let make_state = || {
        GatewayState::with_keys(
            manifests.clone(),
            "aubridge",
            BTreeMap::from([("openrouter".into(), "private-fixture-key".into())]),
        )
        .with_openrouter_catalog_endpoint(format!("{directory_base}/models"))
        .with_settings(settings.clone(), dir.join("stash"))
        .with_agents(agents::AgentsPaths {
            claude_settings: dir.join("claude.json"),
            codex_config: dir.join("codex.toml"),
            opencode_config: opencode.clone(),
            gemini_settings: dir.join("gemini.json"),
            gemini_env: dir.join("gemini.env"),
            state_dir: agent_state.clone(),
        })
        .with_ledger(ledger::Ledger::open(&database).unwrap())
    };
    let (client, base, initial_task) = gateway(make_state()).await;
    let admin = |base: &str, path: &str| {
        client
            .post(format!("{base}/admin/{path}"))
            .bearer_auth("aubridge-admin")
    };
    let catalog = "providers/openrouter/catalog";
    admin(&base, "agents/opencode/enable")
        .json(&json!({"on":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(std::fs::read_to_string(&opencode).unwrap().contains(STATIC));
    let before_read_settings = std::fs::read(&settings).unwrap();
    let before_read_opencode = std::fs::read(&opencode).unwrap();
    let unauthorized_read = client
        .get(format!("{base}/admin/{catalog}"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized_read.status(), StatusCode::UNAUTHORIZED);
    for _ in 0..2 {
        let response = client
            .get(format!("{base}/admin/{catalog}"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cached: Value = response.json().await.unwrap();
        assert_eq!(cached["fetched_at"], Value::Null);
        assert_eq!(cached["checked_at"], Value::Null);
        assert_eq!(cached["models"], json!([]));
        assert_eq!(cached["stale"], true);
        assert_eq!(directory_calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&settings).unwrap(), before_read_settings);
        assert_eq!(std::fs::read(&opencode).unwrap(), before_read_opencode);
    }
    let unauthorized = client
        .post(format!("{base}/admin/{catalog}"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let view: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(directory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(view["issue"], Value::Null);
    assert_eq!(view.get("opencode_issue"), Some(&Value::Null));
    let synced = std::fs::read_to_string(&opencode).unwrap();
    assert!(synced.contains(WITH_TOOLS) && synced.contains(TEXT_ONLY) && !synced.contains(STATIC));
    assert!(synced.contains("// fixture preference"));
    assert_eq!(
        view["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(
                |model| json!({"id":model["id"],"tools":model["tools"],"context":model["context"]})
            )
            .collect::<Vec<_>>(),
        vec![
            json!({"id":TEXT_ONLY,"tools":false,"context":65536}),
            json!({"id":WITH_TOOLS,"tools":true,"context":65536}),
        ]
    );

    let after_refresh_settings = std::fs::read(&settings).unwrap();
    let after_refresh_opencode = std::fs::read(&opencode).unwrap();
    let cached_response = client
        .get(format!("{base}/admin/{catalog}"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(cached_response.status(), StatusCode::OK);
    let cached_view: Value = cached_response.json().await.unwrap();
    assert_eq!(cached_view["fetched_at"], view["fetched_at"]);
    assert_eq!(cached_view["checked_at"], view["fetched_at"]);
    assert_eq!(cached_view["stale"], false);
    assert_eq!(
        cached_view["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| &model["id"])
            .collect::<Vec<_>>(),
        view["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| &model["id"])
            .collect::<Vec<_>>()
    );
    assert_eq!(directory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&settings).unwrap(), after_refresh_settings);
    assert_eq!(std::fs::read(&opencode).unwrap(), after_refresh_opencode);

    // Only discount is metadata. Other pricing fields must still describe
    // exact zero cost, including prices too small for floating-point checks.
    full["data"][0]["pricing"]["discount"] = json!(0);
    full["data"][1]["pricing"]["discount"] = json!("0.000");
    for (id, field, value) in [
        ("vendor/discounted:free", "discount", json!(1)),
        ("vendor/bad-discount:free", "discount", json!(false)),
        ("vendor/unknown-fee:free", "future_fee", json!(0)),
        ("vendor/conditional:free", "overrides", json!([])),
        (
            "vendor/tiny-fee:free",
            "request",
            json!(format!("0.{}1", "0".repeat(400))),
        ),
    ] {
        let mut model = catalog_model(id, true);
        model["pricing"]["discount"] = json!(0);
        model["pricing"][field] = value;
        full["data"].as_array_mut().unwrap().push(model);
    }
    *directory.lock().unwrap() = full.clone();
    let discounted: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discounted["issue"], Value::Null);
    assert_eq!(discounted["models"], view["models"]);
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == WITH_TOOLS));
    let cached = ledger::Ledger::open(&database)
        .unwrap()
        .load_provider_catalog("openrouter")
        .unwrap()
        .unwrap();
    let cached: Value = serde_json::from_str(&cached).unwrap();
    assert_eq!(cached["models"], view["models"]);

    let group = json!({"name":"daily", "members":["plan-model", TEXT_ONLY, WITH_TOOLS], "rule":"free_first"});
    admin(&base, "model-groups")
        .json(&group)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let call = |base: &str| messages(&client, base, "daily", false).send();
    let response = call(&base).await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "openrouter");
    response.bytes().await.unwrap();
    let history = latest(&client, &base).await;
    assert_eq!(history["recent_decisions"][0]["served_model"], WITH_TOOLS);
    assert_eq!(
        history["recent_decisions"][0]["skipped"][0]["reason"],
        "tools-unsupported"
    );

    *directory.lock().unwrap() = json!({"data":[catalog_model(TEXT_ONLY, false)]});
    let stash = agent_state.join("opencode.stash.json");
    let backup_stash = stash.with_extension("fixture-backup");
    std::fs::rename(&stash, &backup_stash).unwrap();
    std::fs::create_dir(&stash).unwrap();
    let file_error: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(file_error["issue"], Value::Null);
    assert!(file_error["opencode_issue"].is_string());
    assert!(std::fs::read_to_string(&opencode)
        .unwrap()
        .contains(WITH_TOOLS));
    std::fs::remove_dir(&stash).unwrap();
    std::fs::rename(&backup_stash, &stash).unwrap();
    let retired: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retired.get("opencode_issue"), Some(&Value::Null));
    assert!(!std::fs::read_to_string(&opencode)
        .unwrap()
        .contains(WITH_TOOLS));
    assert_eq!(retired["models"].as_array().unwrap().len(), 1);
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["aubridge", "plan-model", TEXT_ONLY, "daily"]);
    let mut edited = group.clone();
    edited["rule"] = json!("sequential");
    admin(&base, "model-groups")
        .json(&edited)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut invalid = group.clone();
    invalid["name"] = json!("new");
    assert_eq!(
        admin(&base, "model-groups")
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    admin(&base, "model-groups")
        .json(&group)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        call(&base).await.unwrap().headers()["x-aubridge-provider"],
        "plan"
    );

    *directory.lock().unwrap() = json!({"data":[]});
    let stale: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stale["issue"], "invalid-directory");
    assert_eq!(stale["stale"], true);
    assert_eq!(stale["fetched_at"], retired["fetched_at"]);
    assert_eq!(stale["models"], retired["models"]);

    initial_task.abort();
    let _ = initial_task.await;
    let restored = make_state();
    assert_eq!(
        restored.manifest("openrouter").unwrap().context_windows,
        BTreeMap::from([(TEXT_ONLY.into(), 65536)])
    );
    let (other, base, restarted_task) = gateway(restored).await;
    let overview: Value = other
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["openrouter_catalog"]["models"], retired["models"]);
    assert_eq!(
        call(&base).await.unwrap().headers()["x-aubridge-provider"],
        "plan"
    );
    *directory.lock().unwrap() = full;
    admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        call(&base).await.unwrap().headers()["x-aubridge-provider"],
        "openrouter"
    );
    assert_eq!(free.seen_bodies.lock().await.len(), 2);
    assert_eq!(paid.seen_bodies.lock().await.len(), 2);
    let mut now_paid = catalog_model(WITH_TOOLS, true);
    now_paid["pricing"]["completion"] = json!("0.00001");
    *directory.lock().unwrap() = json!({"data":[now_paid]});
    let no_free: Value = admin(&base, catalog)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(no_free["issue"], Value::Null);
    assert_eq!(no_free["models"], json!([]));
    assert!(make_state()
        .manifest("openrouter")
        .unwrap()
        .free_models
        .is_empty());

    restarted_task.abort();
    let _ = restarted_task.await;
    ledger::Ledger::open(&database)
        .unwrap()
        .save_provider_catalog("openrouter", "unreadable cache")
        .unwrap();
    let recovered = make_state();
    assert_eq!(
        recovered.manifest("openrouter").unwrap().free_models,
        [STATIC]
    );
    drop(recovered);
    for task in [directory_task, free_task, paid_task] {
        task.abort();
        let _ = task.await;
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn directory_without_recognizable_prices_preserves_last_snapshot() {
    const FIRST: &str = "vendor/first:free";
    const NEXT: &str = "vendor/next:free";
    let directory = Arc::new(std::sync::Mutex::new(
        json!({"data":[catalog_model(FIRST, true)]}),
    ));
    let (endpoint, upstream) = http_app(Router::new().route(
        "/models",
        axum::routing::get({
            let directory = directory.clone();
            move |headers: HeaderMap| async move {
                assert!(!headers.contains_key("authorization"));
                axum::Json(directory.lock().unwrap().clone())
            }
        }),
    ))
    .await;
    let dir = std::env::temp_dir().join(format!("aubridge-invalid-prices-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ledger.sqlite");
    let state = || {
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "openrouter",
                &endpoint,
                "static:free",
                "free_tier",
                "chat",
            )]),
            "aubridge",
        )
        .with_openrouter_catalog_endpoint(format!("{endpoint}/models"))
        .with_ledger(ledger::Ledger::open(&db).unwrap())
    };
    let refresh = |client: &reqwest::Client, base: &str| {
        let request = client
            .post(format!("{base}/admin/providers/openrouter/catalog"))
            .bearer_auth("aubridge-admin");
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let get = |client: &reqwest::Client, base: &str, path: &str| {
        let token = if path == "/v1/models" {
            "aubridge-opencode"
        } else {
            "aubridge-admin"
        };
        let request = client.get(format!("{base}{path}")).bearer_auth(token);
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let (client, base, task) = gateway(state()).await;
    let initial = refresh(&client, &base).await;
    assert!(initial["issue"].is_null());
    assert_eq!(initial["models"].as_array().unwrap().len(), 1);
    assert_eq!(initial["models"][0]["id"], FIRST);
    assert_eq!(initial["models"][0]["tools"], true);
    assert_eq!(initial["models"][0]["context"], 65536);
    assert_eq!(
        initial["models"][0]["facts"]["free_evidence"],
        "vendor-zero-price"
    );
    let reader = ledger::Ledger::open(&db).unwrap();
    let saved = reader.load_provider_catalog("openrouter").unwrap().unwrap();
    let initial_models = get(&client, &base, "/v1/models").await;
    assert!(initial_models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == FIRST));

    for data in [
        json!([{"id":"vendor/bad","pricing":{"prompt":"N/A","completion":"unknown"}}]),
        json!([
            {"id":"vendor/prompt-only","pricing":{"prompt":"1e-6","completion":"N/A"}},
            {"id":"vendor/completion-only","pricing":{"prompt":"N/A","completion":"0.01"}},
        ]),
        json!([
            {"id":"vendor/negative-zero","pricing":{"prompt":"-0","completion":"0"}},
            {"id":"vendor/negative-underflow","pricing":{"prompt":"0","completion":"-1e-999"}},
            {"id":"vendor/non-finite","pricing":{"prompt":"NaN","completion":"0"}},
            {"id":"vendor/infinite","pricing":{"prompt":"0","completion":"Infinity"}},
            {"id":"vendor/missing","pricing":{"prompt":"0"}},
            {"id":"vendor/numeric","pricing":{"prompt":0,"completion":"0"}},
        ]),
    ] {
        *directory.lock().unwrap() = json!({"data":data});
        let stale = refresh(&client, &base).await;
        assert_eq!(stale["issue"], "invalid-directory", "{data}");
        assert_eq!(stale["stale"], true, "{data}");
        assert_eq!(stale["fetched_at"], initial["fetched_at"]);
        assert_eq!(stale["models"], initial["models"]);
        assert_eq!(
            reader
                .load_provider_catalog("openrouter")
                .unwrap()
                .as_deref(),
            Some(saved.as_str())
        );
        assert_eq!(get(&client, &base, "/v1/models").await, initial_models);
    }
    task.abort();
    let _ = task.await;
    let (client, base, task) = gateway(state()).await;
    let overview = get(&client, &base, "/admin/providers").await;
    assert_eq!(overview["openrouter_catalog"]["models"], initial["models"]);
    assert_eq!(
        overview["openrouter_catalog"]["fetched_at"],
        initial["fetched_at"]
    );
    assert_eq!(get(&client, &base, "/v1/models").await, initial_models);

    let paid = json!({"id":"vendor/paid","pricing":{"prompt":"1e-6","completion":"2.5e-6"}});
    *directory.lock().unwrap() = json!({"data":[
        catalog_model(NEXT, false),
        paid.clone(),
        {"id":"vendor/still-bad","pricing":{"prompt":"NaN","completion":null}},
    ]});
    let updated = refresh(&client, &base).await;
    assert!(updated["issue"].is_null());
    assert_eq!(updated["stale"], false);
    assert_eq!(updated["models"].as_array().unwrap().len(), 1);
    assert_eq!(updated["models"][0]["id"], NEXT);
    assert_eq!(updated["models"][0]["tools"], false);
    assert_eq!(updated["models"][0]["context"], 65536);

    let mut tiny = catalog_model("vendor/tiny:free", true);
    tiny["pricing"]["completion"] = json!("1e-999");
    for model in [paid, tiny] {
        *directory.lock().unwrap() = json!({"data":[model]});
        let empty = refresh(&client, &base).await;
        assert!(empty["issue"].is_null(), "{model}");
        assert_eq!(empty["stale"], false, "{model}");
        assert_eq!(empty["models"], json!([]), "{model}");
    }
    task.abort();
    let _ = task.await;
    assert!(state()
        .manifest("openrouter")
        .unwrap()
        .free_models
        .is_empty());
    drop(reader);
    upstream.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn refresh_cannot_overlap_an_active_directory_fetch() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (endpoint, upstream) = http_app(Router::new().route(
        "/models",
        axum::routing::get({
            let entered = entered.clone();
            let release = release.clone();
            move || async move {
                entered.notify_one();
                release.notified().await;
                axum::Json(json!({"data":[catalog_model("vendor/tool:free", true)]}))
            }
        }),
    ))
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([source("openrouter", &endpoint, "old", "free_tier", "chat")]),
        "aubridge",
    )
    .with_openrouter_catalog_endpoint(format!("{endpoint}/models"));
    let (client, base, task) = gateway(state).await;
    let request = client
        .post(format!("{base}/admin/providers/openrouter/catalog"))
        .bearer_auth("aubridge-admin");
    let first = tokio::spawn(request.try_clone().unwrap().send());
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    assert_eq!(request.send().await.unwrap().status(), StatusCode::CONFLICT);
    release.notify_one();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    task.abort();
    upstream.abort();
}

#[tokio::test]
async fn empty_200_falls_back_and_ledger_uses_real_usage() {
    let (empty, a, a_task) = spawn(vec![Step::Sse {
        body: String::new(),
    }])
    .await;
    let (good, b, b_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("a", &a, MODEL, "free_tier", "chat"),
            source("b", &b, MODEL, "free_tier", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, MODEL, true).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "b");
    let body = response.text().await.unwrap();
    assert!(
        body.contains("message_stop") && body.contains("call_w_1"),
        "{body}"
    );
    assert!(!body.contains("event: error"));
    assert_eq!(empty.seen_bodies.lock().await.len(), 1);
    assert_eq!(good.seen_bodies.lock().await.len(), 1);

    let dashboard = latest(&client, &base).await;
    let result = &dashboard["recent_decisions"][0];
    assert_eq!(result["attempts"][0]["status"], 200);
    assert_eq!(result["attempts"][0]["reason"], "stream-error");
    assert_eq!(result["provider"], "b");
    assert_eq!(result["served_model"], MODEL);
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["input_tokens"], 25);
    assert_eq!(result["output_tokens"], 30);
    assert!(dashboard["monthly_savings_usd"].as_f64().unwrap() > 0.0);
    for task in [a_task, b_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn committed_output_errors_without_retry_and_unknown_usage_stays_null() {
    // Reuse the recorded tool fixture up to the first tool call. The tool
    // has already been exposed to the client when this upstream ends.
    let mut prefix = String::new();
    for frame in CHAT_TOOL.split_inclusive("\n\n") {
        prefix.push_str(frame);
        if frame.contains("\"id\":\"call_w_1\"") {
            break;
        }
    }
    let (_, a, a_task) = spawn(vec![Step::Sse { body: prefix }]).await;
    let (other, b, b_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("a", &a, MODEL, "free_tier", "chat"),
            source("b", &b, MODEL, "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, MODEL, true).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "a");
    let body = response.text().await.unwrap();
    assert!(
        body.contains("call_w_1") && body.contains("event: error"),
        "{body}"
    );
    assert!(body.contains("message_stop"));
    assert!(other.seen_bodies.lock().await.is_empty());

    let dashboard = latest(&client, &base).await;
    let result = &dashboard["recent_decisions"][0];
    assert_eq!(result["outcome"], "stream-error");
    assert_eq!(result["provider"], "a");
    assert!(result["input_tokens"].is_null());
    assert!(result["output_tokens"].is_null());
    assert!(
        result["duration_ms"].as_u64().unwrap() >= result["first_content_ms"].as_u64().unwrap()
    );
    assert_eq!(dashboard["monthly_savings_usd"], 0.0);
    for task in [a_task, b_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn request_history_records_time_spent_before_output_and_in_each_attempt() {
    let (_, first, first_task) = spawn(vec![Step::SseAfterDelay {
        body: ": keepalive\n\n".into(),
        delay_ms: 35,
    }])
    .await;
    let (_, second, second_task) = spawn(vec![Step::SseAfterDelay {
        body: CHAT_TOOL.into(),
        delay_ms: 35,
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("first", &first, "free-a", "free_tier", "chat"),
            source("second", &second, "free-b", "free_tier", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let reply = messages(&client, &base, "group/free-a+free-b", true)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.headers()["x-aubridge-provider"], "second");
    reply.text().await.unwrap();
    let history: Value = client
        .get(format!("{base}/admin/gateway?agent=claude-code"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &history["requests"][0];
    let first_content = row["first_content_ms"]
        .as_u64()
        .expect("first content timing");
    let duration = row["duration_ms"].as_u64().expect("request timing");
    assert!(first_content >= 60 && duration >= first_content);
    let attempts = row["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| {
        attempt["duration_ms"]
            .as_u64()
            .is_some_and(|ms| (30..=duration).contains(&ms))
    }));

    let reply = messages(&client, &base, "unknown", false)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 404);
    let last = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(last["outcome"], "model_not_found");
    assert!(last["first_content_ms"].is_null() && last["duration_ms"].is_number());
    for task in [first_task, second_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn unknown_literal_model_is_distinct_from_unavailable_provider() {
    let (replay, upstream, upstream_task) = spawn(vec![]).await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("ready", &upstream, "configured", "free_tier", "chat"),
            source(
                "missing",
                "https://missing.invalid",
                "missing-key",
                "free_tier",
                "chat",
            ),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, task) = gateway(state).await;
    let unknown = "model-removed";
    let reply = messages(&client, &base, unknown, true)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 404);
    assert_eq!(
        reply.json::<Value>().await.unwrap()["error"]["type"],
        "not_found_error"
    );
    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("aubridge-opencode")
        .json(
            &json!({"model":unknown,"stream":true,"messages":[{"role":"user","content":"hello"}]}),
        );
    let responses = post_responses(
        &client,
        &base,
        &json!({"model":unknown,"input":"hello","stream":true}),
    );
    for request in [chat, responses] {
        let reply = request.send().await.unwrap();
        assert_eq!(reply.status(), 404);
        assert_eq!(
            reply.json::<Value>().await.unwrap()["error"]["code"],
            "model_not_found"
        );
    }
    let history = latest(&client, &base).await;
    assert_eq!(history["recent_decisions"][0]["outcome"], "model_not_found");
    assert_eq!(history["recent_decisions"][0]["attempts"], json!([]));
    for model in ["missing-key", "group/model-removed"] {
        let reply = messages(&client, &base, model, false).send().await.unwrap();
        assert_eq!(reply.status(), 502);
    }
    assert!(replay.seen_bodies.lock().await.is_empty());
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn client_disconnect_is_a_cancelled_request() {
    let chunk = CHAT_TOOL.split("data: ").nth(2).unwrap();
    let (upstream, upstream_task) = http_app(stalled_chat(format!("data: {chunk}"))).await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([source("a", &upstream, MODEL, "free_tier", "chat")]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let mut response = messages(&client, &base, MODEL, true).send().await.unwrap();
    assert!(response.chunk().await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(60)).await;
    drop(response);

    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let dashboard = latest(&client, &base).await;
            if let Some(row) = dashboard["recent_decisions"].get(0) {
                break row.clone();
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("client drop should settle the request");
    assert_eq!(result["outcome"], "cancelled");
    assert_eq!(result["provider"], "a");
    let first_content = result["first_content_ms"].as_u64().unwrap();
    let duration = result["duration_ms"].as_u64().unwrap();
    assert!(duration >= first_content + 50);
    upstream_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn native_anthropic_partial_usage_counts_only_reported_tokens() {
    let dir = std::env::temp_dir().join(format!("aubridge-partial-usage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (index, (model, outcome, input, output, expected_cost)) in [
        (MODEL, "stream-error", Some(1000), None, 0.00011),
        (MODEL, "cancelled", Some(1000), None, 0.00011),
        (MODEL, "ok", Some(1000), Some(7), 0.00011077),
        (MODEL, "stream-error", None, None, 0.0),
        ("unpriced-model", "stream-error", Some(1000), None, 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        let mut start = anth_start();
        start["message"]["model"] = json!(model);
        start["message"]["usage"] = json!({"output_tokens":0});
        if let Some(input) = input {
            start["message"]["usage"]["input_tokens"] = json!(input);
        }
        let mut frames = vec![
            start,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"fixture text"}}),
        ];
        if outcome == "ok" {
            frames.push(json!({"type":"content_block_stop","index":0}));
            frames.extend(anth_finish("end_turn"));
        }
        let body = sse_frames(&frames);
        let (upstream, upstream_task) = if outcome == "cancelled" {
            http_app(stalled_sse("/messages", body)).await
        } else {
            let (_, base, task) = spawn(vec![Step::Sse { body }]).await;
            (base, task)
        };
        let db = dir.join(format!("{index}.sqlite"));
        let state = GatewayState::from_manifests(
            BTreeMap::from([source("a", &upstream, model, "free_tier", "anthropic")]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(&db).unwrap());
        let (client, base, gateway_task) = gateway(state).await;
        let mut response = messages(&client, &base, model, true)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        if outcome == "cancelled" {
            let mut prefix = Vec::new();
            while !String::from_utf8_lossy(&prefix).contains("fixture text") {
                prefix.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            }
            drop(response);
        } else {
            let body = response.text().await.unwrap();
            assert!(body.contains("fixture text"), "{body}");
            assert_eq!(body.contains("event: error"), outcome == "stream-error");
        }

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if latest(&client, &base).await["recent_decisions"]
                    .get(0)
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("stream should settle");
        let dashboard = latest(&client, &base).await;
        let decision = &dashboard["recent_decisions"][0];
        assert_eq!(decision["outcome"], outcome);
        assert_eq!(decision["input_tokens"], json!(input));
        assert_eq!(decision["output_tokens"], json!(output));
        if model == "unpriced-model" {
            assert!(dashboard["monthly_savings_usd"].is_null());
            assert_eq!(dashboard["value_summary"]["unpriced_records"], 1);
        } else {
            let actual = dashboard["monthly_savings_usd"].as_f64().unwrap();
            assert!(
                (actual - expected_cost).abs() < 1e-12,
                "{model} {outcome}: {actual}"
            );
        }

        let persisted = ledger::Ledger::open(&db)
            .unwrap()
            .recent_decisions(2)
            .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].outcome, outcome);
        assert_eq!(persisted[0].input_tokens, input);
        assert_eq!(persisted[0].output_tokens, output);
        upstream_task.abort();
        gateway_task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn committed_sse_keeps_alive_without_masking_upstream_timeout() {
    let chunk = CHAT_TOOL.split("data: ").nth(2).unwrap();
    let (first, first_task) = http_app(stalled_chat(format!("data: {chunk}"))).await;
    let (backup, backup_url, backup_task) = spawn(vec![]).await;
    let upstream_client = reqwest::Client::builder()
        .no_proxy()
        .read_timeout(Duration::from_secs(7))
        .build()
        .unwrap();
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("free", &first, "free-model", "free_tier", "chat"),
            source("plan", &backup_url, "plan-model", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_client(upstream_client)
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (admin, base, server) = gateway(state).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut response = messages(&client, &base, "group/free-model+plan-model", true)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let prefix = tokio::time::timeout(Duration::from_secs(6), async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            bytes.extend_from_slice(&chunk);
            if bytes.ends_with(b": keepalive\n") {
                return bytes;
            }
        }
        panic!("stream ended without a keepalive");
    })
    .await
    .expect("keepalive arrived before upstream timeout");
    assert!(String::from_utf8(prefix).unwrap().contains("text_delta"));
    let tail = response.text().await.unwrap();
    assert!(tail.contains("event: error") && tail.contains("event: message_stop"));
    assert!(!tail.starts_with('\n'));
    assert!(backup.seen_bodies.lock().await.is_empty());
    assert_eq!(
        latest(&admin, &base).await["recent_decisions"][0]["outcome"],
        "stream-error"
    );
    for task in [server, first_task, backup_task] {
        task.abort();
    }
}

#[tokio::test]
async fn read_timeout_before_usable_content_still_falls_back() {
    let role_only = CHAT_TOOL.split_inclusive("\n\n").next().unwrap();
    let (a, a_task) = http_app(stalled_chat(role_only.to_string())).await;
    let (_, b, b_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let upstream_client = reqwest::Client::builder()
        .no_proxy()
        .read_timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("a", &a, MODEL, "free_tier", "chat"),
            source("b", &b, MODEL, "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_client(upstream_client)
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, MODEL, true).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "b");
    assert!(response.text().await.unwrap().contains("message_stop"));
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["status"], 200);
    assert_eq!(row["attempts"][0]["reason"], "stream-error");
    for task in [a_task, b_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn named_group_skips_unready_and_incompatible_models_and_prefers_free() {
    let (free, free_base, free_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let (paid, paid_base, paid_task) = spawn(Vec::new()).await;
    let (free_id, mut free_manifest) = source("free", &free_base, MODEL, "free_tier", "chat");
    free_manifest.free_models.insert(0, "no-tools".into());
    free_manifest.models_without_tools.insert("no-tools".into());
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            (free_id, free_manifest),
            source("paid", &paid_base, "plan-model", "subscription", "chat"),
            source(
                "missing",
                "https://missing.invalid",
                "missing-model",
                "free_tier",
                "chat",
            ),
        ]),
        "aubridge",
    );
    let dir = std::env::temp_dir().join(format!("aubridge-v1-groups-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    let state = state
        .with_settings(config, dir.join("stash"))
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    state
        .upsert_model_group(&credentials::settings::ModelGroup {
            name: "release-group".into(),
            members: vec![
                "plan-model".into(),
                "missing-model".into(),
                "no-tools".into(),
                MODEL.into(),
            ],
            rule: "free_first".into(),
        })
        .unwrap();
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, "release-group", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["content"][0]["text"], "Let me check the weather.");
    assert_eq!(response["content"][1]["input"]["city"], "Beijing");
    assert_eq!(response["stop_reason"], "tool_use");
    assert_eq!(response["usage"]["output_tokens"], 30);
    let decision = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(decision["attempts"].as_array().unwrap().len(), 1);
    let skipped = decision["skipped"].as_array().unwrap();
    assert!(skipped
        .iter()
        .any(|entry| entry["id"] == "missing" && entry["reason"] == "missing-credential"));
    assert!(skipped
        .iter()
        .any(|entry| entry["model"] == "no-tools" && entry["reason"] == "tools-unsupported"));
    assert!(paid.seen_bodies.lock().await.is_empty());
    let request: Value = serde_json::from_str(&free.seen_bodies.lock().await[0]).unwrap();
    assert_eq!(request["model"], MODEL);
    for task in [free_task, paid_task, gateway_task] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn committed_stream_limit_updates_anthropic_route_without_replaying() {
    for committed in [false, true] {
        let mut frames = vec![anth_start()];
        if committed {
            frames.push(json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":"partial text"}}));
        }
        frames.push(json!({"type":"error","error":{"type":"rate_limit_error","scope":"account"}}));
        let (free, free_base, free_task) = spawn(vec![Step::SseWithHeaders {
            body: sse_frames(&frames),
            headers: vec![("retry-after".into(), "120".into())],
        }])
        .await;
        let reply = Step::Sse {
            body: CHAT_TOOL.into(),
        };
        let (plan, plan_base, plan_task) = spawn(vec![reply.clone(), reply]).await;
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                source(
                    "free",
                    &free_base,
                    "limited-model",
                    "free_tier",
                    "anthropic",
                ),
                source("plan", &plan_base, "backup-model", "subscription", "chat"),
            ]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let (client, base, task) = gateway(state).await;
        let model = "group/backup-model+limited-model";
        let response = messages(&client, &base, model, true).send().await.unwrap();
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if committed { "free" } else { "plan" }
        );
        let body = response.text().await.unwrap();
        let page = latest(&client, &base).await;
        let row = &page["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 200);
        if committed {
            assert!(
                body.contains("partial text")
                    && body.contains("rate_limit_error")
                    && body.contains("message_stop")
            );
            assert_eq!(row["outcome"], "stream-error");
            assert_eq!(row["attempts"].as_array().unwrap().len(), 1);
            assert_eq!(row["input_tokens"], 12);
            assert!(row["output_tokens"].is_null());
            assert!(plan.seen_bodies.lock().await.is_empty());
        } else {
            assert_eq!(row["attempts"][0]["reason"], "account-rate-limit");
            assert_eq!(row["outcome"], "ok");
        }
        let followup = messages(&client, &base, model, true).send().await.unwrap();
        assert_eq!(followup.headers()["x-aubridge-provider"], "plan");
        followup.bytes().await.unwrap();
        let second = latest(&client, &base).await;
        let row = &second["recent_decisions"][0];
        assert_eq!(row["provider"], "plan");
        assert_eq!(row["skipped"][0]["reason"], "account-rate-limit");
        assert_eq!(
            plan.seen_bodies.lock().await.len(),
            if committed { 1 } else { 2 }
        );
        assert_eq!(free.seen_bodies.lock().await.len(), 1);
        for task in [task, free_task, plan_task] {
            task.abort();
        }
    }
}

#[tokio::test]
async fn committed_stream_limit_updates_only_current_connection_and_expected_scope() {
    for (protocol, error, expected_skip, rotate_key) in [
        (
            "chat",
            json!({"code":"model_rate_limit_exceeded","scope":"model"}),
            Some("model-rate-limit"),
            false,
        ),
        (
            "responses",
            json!({"status":402,"code":"insufficient_balance"}),
            Some("account-credit-exhausted"),
            false,
        ),
        (
            "chat",
            json!({"code":"account_rate_limit_exceeded","scope":"account"}),
            None,
            true,
        ),
        (
            "chat",
            json!({"message":"429 account_rate_limit_exceeded"}),
            None,
            false,
        ),
        (
            "chat",
            json!({"status":401,"code":"invalid_api_key"}),
            None,
            false,
        ),
    ] {
        use futures_util::{stream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let (path, prefix, error, success) = if protocol == "chat" {
            (
                "/chat/completions",
                format!(
                    "data: {}\n\n",
                    json!({"id":"fixture","model":MODEL,"choices":[{"delta":{"content":"partial fixture text"}}],"usage":{"prompt_tokens":1000}})
                ),
                format!("data: {}\n\n", json!({"error":error})),
                CHAT_TOOL,
            )
        } else {
            (
                "/responses",
                sse_frames(&[
                    json!({"type":"response.created","response":{"id":"fixture","model":MODEL,"usage":{"input_tokens":1000}}}),
                    json!({"type":"response.output_text.delta","delta":"partial fixture text"}),
                ]),
                sse_frames(&[json!({"type":"response.failed","response":{"error":error}})]),
                CODEX_REPLY,
            )
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let (upstream, upstream_task) = http_app(Router::new().route(
            path,
            post({
                let release = release.clone();
                let calls = calls.clone();
                move |headers: HeaderMap| {
                    let first = calls.fetch_add(1, Ordering::Relaxed) == 0;
                    let release = release.clone();
                    let prefix = prefix.clone();
                    let error = error.clone();
                    async move {
                        let body = if first {
                            assert_eq!(headers["authorization"], "Bearer fixture-old");
                            let prefix = stream::once(async move {
                                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(prefix))
                            });
                            let tail = stream::once(async move {
                                release.notified().await;
                                Ok(axum::body::Bytes::from(error))
                            });
                            Body::from_stream(prefix.chain(tail))
                        } else {
                            assert_eq!(
                                headers["authorization"],
                                if rotate_key {
                                    "Bearer fixture-new"
                                } else {
                                    "Bearer fixture-old"
                                }
                            );
                            Body::from(success)
                        };
                        (
                            [
                                ("content-type", "text/event-stream"),
                                ("retry-after", "120"),
                            ],
                            body,
                        )
                    }
                }
            }),
        ))
        .await;
        let (backup, backup_base, backup_task) = spawn(vec![Step::Sse {
            body: CHAT_TOOL.into(),
        }])
        .await;
        let (id, mut manifest) = source("free", &upstream, MODEL, "free_tier", protocol);
        manifest.free_models.push("sibling-model".into());
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([
                    (id, manifest),
                    source(
                        "backup",
                        &backup_base,
                        "backup-model",
                        "subscription",
                        "chat",
                    ),
                ]),
                "aubridge",
            )
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
        );
        state.set_provider_key("free", "fixture-old").unwrap();
        let (base, task) = http_app(gateway::app(state.clone())).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap();
        let group = format!("group/{MODEL}+backup-model");
        let mut response = messages(&client, &base, &group, true).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-aubridge-provider"], "free");
        let mut prefix = Vec::new();
        while !String::from_utf8_lossy(&prefix).contains("partial fixture text") {
            prefix.extend_from_slice(&response.chunk().await.unwrap().unwrap());
        }
        if rotate_key {
            state.set_provider_key("free", "fixture-new").unwrap();
        }
        release.notify_one();
        let tail = response.text().await.unwrap();
        assert!(
            tail.contains("event: error") && tail.contains("event: message_stop"),
            "{protocol}: {tail}"
        );
        assert!(!tail.contains("partial fixture text"));
        assert!(backup.seen_bodies.lock().await.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let dashboard = latest(&client, &base).await;
        let row = &dashboard["recent_decisions"][0];
        assert_eq!(row["outcome"], "stream-error");
        assert_eq!(row["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(row["attempts"][0]["id"], "free");
        assert_eq!(row["attempts"][0]["status"], 200);
        assert_eq!(row["attempts"][0]["chosen"], true);
        assert_eq!(row["input_tokens"], 1000);
        assert!(row["output_tokens"].is_null());
        let free = state
            .snapshot()
            .into_iter()
            .find(|provider| provider.id == "free")
            .unwrap();
        assert_eq!(
            free.breaker.fail_streak,
            u8::from(expected_skip.is_none() && !rotate_key)
        );

        let response = messages(&client, &base, &group, true).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-aubridge-provider"],
            if expected_skip.is_some() {
                "backup"
            } else {
                "free"
            },
            "{protocol}"
        );
        response.bytes().await.unwrap();
        let dashboard = latest(&client, &base).await;
        let row = &dashboard["recent_decisions"][0];
        assert_eq!(row["attempts"].as_array().unwrap().len(), 1);
        if let Some(reason) = expected_skip {
            assert_eq!(row["skipped"][0]["id"], "free");
            assert_eq!(row["skipped"][0]["reason"], reason);
            assert_eq!(backup.seen_bodies.lock().await.len(), 1);
        }
        if expected_skip == Some("model-rate-limit") {
            let response = messages(&client, &base, "sibling-model", true)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-aubridge-provider"], "free");
            response.bytes().await.unwrap();
        }
        for server in [upstream_task, backup_task, task] {
            server.abort();
        }
    }
}

#[tokio::test]
async fn rate_limit_exhaustion_reports_retry_after_in_each_protocol() {
    let retry_date =
        httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(80));
    let (replay, upstream, upstream_task) = spawn(vec![
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":{"code":"model_rate_limit_exceeded"}}"#.into(),
            headers: vec![("retry-after".into(), retry_date)],
        },
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":{"code":"account_quota_exhausted","message":"private-upstream-details"}}"#.into(),
            headers: vec![("retry-after".into(), "30".into())],
        },
    ])
    .await;
    let (id, mut manifest) = source("shared", &upstream, "free-a", "free_tier", "chat");
    manifest.free_models.push("free-b".into());
    let state = GatewayState::from_manifests(BTreeMap::from([(id, manifest)]), "aubridge")
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let group = "group/free-a+free-b";
    let check = |response: reqwest::Response, seconds: std::ops::RangeInclusive<u64>| async move {
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let delay = response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(seconds.contains(&delay), "unexpected Retry-After {delay}");
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert!(!body.to_string().contains("private-upstream-details"));
        body
    };
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(check(response, 27..=31).await["type"], "error");
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("aubridge-opencode")
        .json(
            &json!({"model":"free-a","messages":[{"role":"user","content":"retry"}],"stream":true}),
        )
        .send()
        .await
        .unwrap();
    // The earlier account reset still leaves this particular model cooling.
    assert_eq!(
        check(response, 76..=80).await["error"]["code"],
        "rate_limit_exceeded"
    );
    let response = client
        .post(format!("{base}/v1/responses"))
        .bearer_auth("aubridge-codex")
        .json(&json!({"model":group,"input":"retry","stream":true,"store":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        check(response, 27..=31).await["error"]["code"],
        "rate_limit_exceeded"
    );
    assert_eq!(replay.seen_bodies.lock().await.len(), 2);
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["outcome"], "rate-limited");
    assert_eq!(row["attempts"], json!([]));
    assert_eq!(row["skipped"].as_array().unwrap().len(), 2);
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("aubridge-opencode")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "free-a"));
    upstream_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn rate_limit_exhaustion_leaves_mixed_or_non_quota_failures_as_gateway_errors() {
    let (_, limited, limited_task) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"code":"model_rate_limit_exceeded"}}"#.into(),
    }])
    .await;
    let (_, overloaded, overloaded_task) = spawn(vec![Step::Fail {
        status: 529,
        body: String::new(),
    }])
    .await;
    let (_, credit, credit_task) = spawn(vec![Step::Fail {
        status: 402,
        body: String::new(),
    }])
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("limited", &limited, "limited", "free_tier", "chat"),
            source("overloaded", &overloaded, "overloaded", "free_tier", "chat"),
            source("credit", &credit, "credit", "free_tier", "chat"),
        ]),
        "aubridge",
    );
    let (client, base, gateway_task) = gateway(state).await;
    for model in ["group/limited+overloaded", "overloaded", "credit", "credit"] {
        let response = messages(&client, &base, model, false).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{model}");
        assert!(!response.headers().contains_key("retry-after"));
    }
    for task in [limited_task, overloaded_task, credit_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn free_group_tries_a_sibling_then_the_subscription_then_recovers() {
    let (free, free_base, free_task) = spawn(vec![
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":{"code":"model_rate_limit_exceeded","message":"rate limit"}}"#.into(),
            headers: vec![("retry-after".into(), "4".into())],
        },
        Step::Sse { body: CHAT_TOOL.into() },
        Step::FailWithHeaders {
            status: 429,
            body: r#"{"error":{"code":"account_quota_exhausted","message":"sensitive-upstream-text"}}"#.into(),
            headers: vec![("retry-after".into(), "3".into())],
        },
        Step::Sse { body: CHAT_TOOL.into() },
    ]).await;
    let (paid, paid_base, paid_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let (id, mut manifest) = source("free", &free_base, "free-a", "free_tier", "chat");
    manifest.free_models.push("free-b".into());
    let manifests = BTreeMap::from([
        (id, manifest),
        source("plan", &paid_base, "plan-model", "subscription", "chat"),
    ]);
    let dir = std::env::temp_dir().join(format!("aubridge-v1-cooldowns-{}", std::process::id()));
    let path = dir.join("ledger.sqlite");
    let state = GatewayState::from_manifests(manifests.clone(), "aubridge")
        .with_ledger(ledger::Ledger::open(&path).unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let group = "group/plan-model+free-a+free-b";
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["requested_model"], "free-a");
    assert_eq!(row["attempts"][0]["reason"], "model-rate-limit");
    assert_eq!(row["attempts"][1]["requested_model"], "free-b");
    assert_eq!(row["attempts"][1]["chosen"], true);
    assert!(paid.seen_bodies.lock().await.is_empty());
    gateway_task.abort();

    let state = GatewayState::from_manifests(manifests, "aubridge")
        .with_ledger(ledger::Ledger::open(&path).unwrap());
    assert!(state
        .snapshot()
        .iter()
        .find(|source| source.id == "free")
        .unwrap()
        .model_cooldowns
        .contains_key("free-a"));
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["skipped"][0]["model"], "free-a");
    assert_eq!(row["skipped"][0]["reason"], "model-rate-limit");
    assert_eq!(row["attempts"][0]["reason"], "account-quota-exhausted");
    assert_eq!(row["attempts"][1]["kind"], "subscription");
    assert!(!row.to_string().contains("sensitive-upstream-text"));
    assert_eq!(free.seen_bodies.lock().await.len(), 3);

    tokio::time::sleep(Duration::from_secs(5)).await;
    let response = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["requested_model"], "free-a");
    assert_eq!(row["skipped"], json!([]));
    assert_eq!(paid.seen_bodies.lock().await.len(), 1);
    for task in [free_task, paid_task, gateway_task] {
        task.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn account_quota_skips_already_planned_models_before_another_send() {
    let (free, free_base, free_task) = spawn(vec![Step::Fail {
        status: 429,
        body: r#"{"error":{"code":"account_quota_exhausted"}}"#.into(),
    }])
    .await;
    let (_, paid_base, paid_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let (id, mut manifest) = source("free", &free_base, "free-a", "free_tier", "chat");
    manifest.free_models.push("free-b".into());
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            (id, manifest),
            source("plan", &paid_base, "plan-model", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, "group/plan-model+free-a+free-b", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    assert_eq!(free.seen_bodies.lock().await.len(), 1);
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["skipped"][0]["model"], "free-b");
    assert_eq!(row["skipped"][0]["reason"], "account-quota-exhausted");
    assert!(row["skipped"][0]["retry_at"].is_number());
    for task in [free_task, paid_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn forbidden_denials_disable_only_with_explicit_account_evidence() {
    for (case, content_type, body, disabled) in [
        (
            "tool",
            "application/json",
            r#"{"error":{"message":"Tool execution is disabled"}}"#,
            false,
        ),
        (
            "quoted message and nested code",
            "application/json",
            r#"{"error":{"message":"Request rejected: account has been suspended","metadata":{"code":"account_disabled"}}}"#,
            false,
        ),
        (
            "request scoped",
            "text/plain",
            "Account disabled tool execution for this request",
            false,
        ),
        (
            "model scoped wording",
            "text/plain",
            "Account is disabled for this model",
            false,
        ),
        (
            "markup",
            "application/json",
            "\u{feff} <!doctype html><p>Your account has been suspended</p>",
            false,
        ),
        (
            "HTML content type",
            "Text/HTML; charset=UTF-8",
            "Account has been suspended",
            false,
        ),
        (
            "plain account restriction",
            "text/plain",
            "Account has been suspended.",
            true,
        ),
        (
            "structured account message",
            "application/json",
            r#"{"error":{"message":"account has been suspended"}}"#,
            true,
        ),
        (
            "explicit Chinese account message",
            "application/json",
            r#"{"error":{"message":"您的账号已封禁，请联系客服"}}"#,
            true,
        ),
        (
            "structured account code",
            "application/json",
            r#"{"error":{"type":"permission_error","code":"account_disabled","message":"Contact support"}}"#,
            true,
        ),
        (
            "structured account type",
            "application/json",
            r#"{"error":{"type":"account_suspended","message":"Contact support"}}"#,
            true,
        ),
    ] {
        let success = || Step::Sse {
            body: CHAT_TOOL.into(),
        };
        let (preferred, preferred_base, preferred_task) = spawn(vec![
            Step::FailWithHeaders {
                status: 403,
                body: body.into(),
                headers: vec![("content-type".into(), content_type.into())],
            },
            success(),
        ])
        .await;
        let (_, fallback_base, fallback_task) = spawn(vec![success(), success()]).await;
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                source(
                    "preferred",
                    &preferred_base,
                    "free-model",
                    "free_tier",
                    "chat",
                ),
                source(
                    "fallback",
                    &fallback_base,
                    "paid-model",
                    "subscription",
                    "chat",
                ),
            ]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let (client, base, gateway_task) = gateway(state).await;
        let group = "group/paid-model+free-model";
        let first = messages(&client, &base, group, false).send().await.unwrap();
        assert_eq!(first.headers()["x-aubridge-provider"], "fallback", "{case}");
        let row = &latest(&client, &base).await["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 403, "{case}");
        assert_eq!(row["attempts"][0]["reason"], "access-denied", "{case}");

        let next = messages(&client, &base, group, false).send().await.unwrap();
        assert_eq!(
            next.headers()["x-aubridge-provider"],
            if disabled { "fallback" } else { "preferred" },
            "{case}"
        );
        if disabled {
            let row = &latest(&client, &base).await["recent_decisions"][0];
            assert_eq!(row["skipped"][0]["reason"], "provider-disabled", "{case}");
            assert_eq!(preferred.seen_bodies.lock().await.len(), 1, "{case}");
            client
                .post(format!("{base}/admin/providers/preferred/resume-routing"))
                .bearer_auth("aubridge-admin")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            let resumed = messages(&client, &base, group, false).send().await.unwrap();
            assert_eq!(
                resumed.headers()["x-aubridge-provider"],
                "preferred",
                "{case}"
            );
        }
        assert_eq!(preferred.seen_bodies.lock().await.len(), 2, "{case}");
        for task in [preferred_task, fallback_task, gateway_task] {
            task.abort();
        }
    }
}

#[tokio::test]
async fn forbidden_model_access_leaves_sibling_model_available() {
    let (_, upstream, upstream_task) = spawn(vec![
        Step::Fail {
            status: 403,
            body: r#"{"error":{"code":"model_access_denied","message":"This model is disabled"}}"#
                .into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (id, mut manifest) = source("free", &upstream, "free-a", "free_tier", "chat");
    manifest.free_models.push("free-b".into());
    let state = GatewayState::from_manifests(BTreeMap::from([(id, manifest)]), "aubridge")
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let response = messages(&client, &base, "group/free-a+free-b", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "free");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["status"], 403);
    assert_eq!(row["attempts"][0]["reason"], "model-unavailable");
    assert_eq!(row["attempts"][1]["requested_model"], "free-b");
    assert_eq!(row["attempts"][1]["chosen"], true);
    upstream_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn anthropic_429_uses_only_exhausted_window_resets() {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let start = now();
    let short = (start + 120).to_string();
    let long = (start + 7 * 86_400).to_string();
    let generic = (start + 240).to_string();
    let expired = (start - 1).to_string();
    let too_far = (start + 33 * 86_400).to_string();
    let milliseconds = ((start + 120) * 1000).to_string();
    let rejected_5h = ("5h-status", "rejected");
    let used_7d = ("7d-utilization", "1");
    let threshold_7d = ("7d-surpassed-threshold", "true");
    let quota = "account-quota-exhausted";
    let rate = "account-rate-limit";
    // Each row checks which candidates cool, the recorded deadline, and the
    // next routing decision without sleeping for the cooldown to expire.
    for (case, windows, extra, reason, expected_reset, delay) in [
        (
            "5h only",
            vec![rejected_5h, ("5h-reset", &short), ("7d-reset", &long)],
            vec![("retry-after", "30"), ("x-ratelimit-remaining", "1")],
            quota,
            Some(start + 120),
            0,
        ),
        (
            "7d only",
            vec![used_7d, ("5h-reset", &short), ("7d-reset", &long)],
            vec![],
            quota,
            Some(start + 7 * 86_400),
            0,
        ),
        (
            "both windows",
            vec![
                rejected_5h,
                threshold_7d,
                ("5h-reset", &short),
                ("7d-reset", &long),
            ],
            vec![],
            quota,
            Some(start + 7 * 86_400),
            0,
        ),
        (
            "only resets",
            vec![("5h-reset", &short), ("7d-reset", &long)],
            vec![("retry-after", "30"), ("x-ratelimit-scope", "account")],
            rate,
            None,
            30,
        ),
        (
            "nonfinite values preserve model limit",
            vec![
                ("5h-utilization", "NaN"),
                ("7d-utilization", "Infinity"),
                ("7d-reset", &long),
            ],
            vec![("retry-after", "30"), ("x-ratelimit-scope", "model")],
            "model-rate-limit",
            None,
            30,
        ),
        (
            "invalid and expired resets use generic reset",
            vec![
                rejected_5h,
                used_7d,
                ("5h-reset", "bad"),
                ("7d-reset", &expired),
            ],
            vec![("x-ratelimit-reset", &generic), ("retry-after", "30")],
            quota,
            Some(start + 240),
            0,
        ),
        (
            "millisecond and out of range resets use retry-after",
            vec![
                rejected_5h,
                used_7d,
                ("5h-reset", &milliseconds),
                ("7d-reset", &too_far),
            ],
            vec![("retry-after", "30")],
            quota,
            None,
            31,
        ),
        (
            "missing reset uses quota default",
            vec![rejected_5h, ("7d-utilization", "0.9"), ("7d-reset", &long)],
            vec![],
            quota,
            None,
            901,
        ),
    ] {
        let model_scoped = reason == "model-rate-limit";
        let headers = windows
            .into_iter()
            .map(|(name, value)| {
                (
                    format!("anthropic-ratelimit-unified-{name}"),
                    value.to_owned(),
                )
            })
            .chain(
                extra
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned())),
            )
            .collect();
        let reply = || Step::Sse {
            body: CHAT_TOOL.into(),
        };
        let (limited, limited_base, limited_task) = spawn(vec![Step::FailWithHeaders {
            status: 429,
            body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
                .into(),
            headers,
        }])
        .await;
        let (_, backup_base, backup_task) = spawn(vec![reply(), reply()]).await;
        let (id, mut manifest) = source(
            "limited",
            &limited_base,
            "limited-a",
            "free_tier",
            "anthropic",
        );
        manifest.free_models.push("limited-b".into());
        let state = GatewayState::from_manifests(
            BTreeMap::from([
                (id, manifest),
                source(
                    "backup",
                    &backup_base,
                    "backup-model",
                    "subscription",
                    "chat",
                ),
            ]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let (client, base, gateway_task) = gateway(state).await;
        let group = if model_scoped {
            "group/limited-a+backup-model"
        } else {
            "group/limited-a+limited-b+backup-model"
        };
        let before = now();
        let first = messages(&client, &base, group, false).send().await.unwrap();
        let after = now();
        assert_eq!(first.headers()["x-aubridge-provider"], "backup", "{case}");
        let row = &latest(&client, &base).await["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 429, "{case}");
        assert_eq!(row["attempts"][0]["reason"], reason, "{case}");
        let next = messages(&client, &base, group, false).send().await.unwrap();
        assert_eq!(next.headers()["x-aubridge-provider"], "backup", "{case}");
        let row = &latest(&client, &base).await["recent_decisions"][0];
        let skipped = row["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), if model_scoped { 1 } else { 2 }, "{case}");
        for entry in skipped {
            assert_eq!(entry["id"], "limited", "{case}");
            assert_eq!(entry["reason"], reason, "{case}");
            let until = entry["retry_at"].as_u64().unwrap();
            if let Some(reset) = expected_reset {
                assert_eq!(until, reset + 1, "{case}");
            } else {
                assert!(
                    (before + delay..=after + delay).contains(&until),
                    "{case}: {until}"
                );
            }
        }
        assert_eq!(limited.seen_bodies.lock().await.len(), 1, "{case}");
        for task in [limited_task, backup_task, gateway_task] {
            task.abort();
        }
    }
}

#[tokio::test]
async fn kimi_subscription_quota_403_cools_the_account_but_other_denials_do_not() {
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 120;
    let (kimi, kimi_base, kimi_task) = spawn(vec![Step::FailWithHeaders {
        status: 403,
        body: json!({"error":{
            "type":"access_terminated_error",
            "message":"You've reached your weekly (7-day) usage limit. Your quota will reset when the current 7-day window ends.",
        }})
        .to_string(),
        headers: vec![
            ("x-ratelimit-reset".into(), reset.to_string()),
            ("retry-after".into(), "2".into()),
            ("x-ratelimit-remaining".into(), "1".into()),
            ("x-ratelimit-scope".into(), "model".into()),
        ],
    }])
    .await;
    let (backup, backup_base, backup_task) = spawn(vec![
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (denied, denied_base, denied_task) = spawn(vec![
        Step::FailWithHeaders {
            status: 403,
            body: json!({"error":{
                "type":"access_terminated_error",
                "message":"You've reached your concurrent request limit. Please wait for your ongoing requests to finish and try again.",
            }})
            .to_string(),
            headers: vec![("retry-after".into(), "90".into())],
        },
        Step::Fail {
            status: 403,
            body: r#"{"error":{"message":"You've reached your weekly usage limit."}}"#.into(),
        },
    ])
    .await;
    let (id, mut manifest) = source("kimi", &kimi_base, "kimi-a", "subscription", "anthropic");
    manifest.free_models.push("kimi-b".into());
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            (id, manifest),
            source(
                "backup",
                &backup_base,
                "backup-model",
                "subscription",
                "chat",
            ),
            source(
                "denied",
                &denied_base,
                "denied-model",
                "subscription",
                "anthropic",
            ),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let group = "group/kimi-a+kimi-b+backup-model";
    let first = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(first.headers()["x-aubridge-provider"], "backup");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"][0]["status"], 403);
    assert_eq!(row["attempts"][0]["reason"], "account-quota-exhausted");
    assert_eq!(row["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(row["skipped"][0]["model"], "kimi-b");
    assert_eq!(row["skipped"][0]["reason"], "account-quota-exhausted");
    assert_eq!(row["skipped"][0]["retry_at"], reset + 1);
    assert!(!row.to_string().contains("weekly"));

    let second = messages(&client, &base, group, false).send().await.unwrap();
    assert_eq!(second.headers()["x-aubridge-provider"], "backup");
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["skipped"].as_array().unwrap().len(), 2);
    assert!(row["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| { entry["id"] == "kimi" && entry["reason"] == "account-quota-exhausted" }));
    assert_eq!(kimi.seen_bodies.lock().await.len(), 1);
    assert_eq!(backup.seen_bodies.lock().await.len(), 2);

    for _ in 0..2 {
        let response = messages(&client, &base, "denied-model", false)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.headers().contains_key("retry-after"));
        let row = &latest(&client, &base).await["recent_decisions"][0];
        assert_eq!(row["attempts"][0]["status"], 403);
        assert_eq!(row["attempts"][0]["reason"], "access-denied");
        assert_eq!(row["skipped"], json!([]));
    }
    assert_eq!(denied.seen_bodies.lock().await.len(), 2);
    for task in [kimi_task, backup_task, denied_task, gateway_task] {
        task.abort();
    }
}

#[tokio::test]
async fn openrouter_budget_is_cached_without_marking_free_requests_exhausted() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (upstream, upstream_task) = http_app(Router::new().route(
        "/api/v1/key",
        axum::routing::get(move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer mock-openrouter");
                seen.fetch_add(1, Ordering::Relaxed);
                axum::Json(
                    json!({"data":{"limit":10, "limit_remaining":0, "limit_reset":"monthly"}}),
                )
            }
        }),
    ))
    .await;
    let state = GatewayState::with_keys(
        BTreeMap::from([("openrouter".into(), registry::openrouter())]),
        "aubridge",
        BTreeMap::from([("openrouter".into(), "mock-openrouter".into())]),
    )
    .with_openrouter_endpoint(format!("{upstream}/api/v1/key"));
    let (client, base, gateway_task) = gateway(state).await;
    for _ in 0..2 {
        let reply: Value = client
            .get(format!("{base}/admin/providers"))
            .bearer_auth("aubridge-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            reply["openrouter_key_budget"]["limit_remaining"].as_f64(),
            Some(0.0)
        );
        assert_eq!(
            reply["providers"][0]["free_requests"][0]["status"],
            "eligible"
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let dashboard = latest(&client, &base).await;
    assert!(dashboard["providers"][0]["remaining"].is_null());
    assert_eq!(dashboard["providers"][0]["resting"], false);
    upstream_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn provider_endpoints_reject_ambiguous_addresses_before_sending_credentials() {
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let encoded = upstream.replace("127.0.0.1", "%31%32%37.0.0.1");
    assert_eq!(
        reqwest::Url::parse(&encoded).unwrap().host_str(),
        Some("127.0.0.1")
    );
    let (id, mut unsafe_manifest) = source("unsafe", &encoded, "unsafe-model", "free_tier", "chat");
    unsafe_manifest.permissions.net = vec!["%31%32%37.0.0.1".into()];
    let dir =
        std::env::temp_dir().join(format!("aubridge-provider-address-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = GatewayState::with_keys(
        BTreeMap::from([(id, unsafe_manifest)]),
        "aubridge",
        BTreeMap::from([("unsafe".into(), "fixture-key".into())]),
    )
    .with_settings(dir.join("config.json"), dir.join("stash"))
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, task) = gateway(state).await;
    let add = |id: &str, address: &str| {
        client
            .post(format!("{base}/admin/providers"))
            .bearer_auth("aubridge-admin")
            .json(&json!({"id":id,"base":address,"models":["fixture-model"]}))
    };
    for address in [
        "http://local:fixture-private@127.0.0.1/v1",
        "https://local.invalid/v1?key=fixture-private",
        "https://local.invalid/v1#fixture-private",
        "http://127.0.0.1:65536/v1",
    ] {
        let response = add("rejected", address).send().await.unwrap();
        assert_eq!(response.status(), 400);
        assert!(!response.text().await.unwrap().contains("fixture-private"));
    }
    for (id, address) in [
        ("lan", "http://192.168.31.191:15721/relay/v1"),
        ("ipv6", "http://[::1]:15721/relay/v1"),
    ] {
        add(id, address)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let response = messages(&client, &base, "unsafe-model", false)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(replay.seen_bodies.lock().await.is_empty());
    assert!(replay.seen_headers.lock().await.is_empty());
    let row = &latest(&client, &base).await["recent_decisions"][0];
    assert_eq!(row["attempts"], json!([]));
    assert_eq!(
        row["skipped"],
        json!([{"id":"unsafe","model":"unsafe-model","kind":"free_tier","reason":"invalid-provider-url"}])
    );
    task.abort();
    upstream_task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn provider_check_is_admin_only_and_does_not_change_routing_or_log_requests() {
    let second_model = "vendor/second:free";
    let (upstream, url, upstream_task) = spawn(vec![
        Step::Fail {
            status: 401,
            body: r#"{"error":{"message":"sensitive-upstream-text"}}"#.into(),
        },
        Step::Sse {
            body:
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial reply\"}}]}\n\n"
                    .into(),
        },
        Step::Sse {
            body: CHAT_TOOL.into(),
        },
    ])
    .await;
    let (id, mut manifest) = source("free", &url, MODEL, "free_tier", "chat");
    manifest.free_models.push(second_model.into());
    let state = GatewayState::with_keys(
        BTreeMap::from([(id, manifest)]),
        "aubridge",
        BTreeMap::from([("free".into(), "probe-test-secret".into())]),
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let (client, base, gateway_task) = gateway(state).await;
    let check = format!("{base}/admin/providers/free/check");
    assert_eq!(
        client
            .post(&check)
            .bearer_auth("aubridge-claude-code")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(upstream.seen_bodies.lock().await.is_empty());
    for (model, expected, reported, upstream_status) in [
        (None, "credential-rejected", MODEL, Some(401)),
        (Some(second_model), "stream-error", second_model, Some(200)),
        (Some(second_model), "ok", second_model, Some(200)),
        (
            Some("unknown-model"),
            "model-unavailable",
            "unknown-model",
            None,
        ),
    ] {
        let mut request = client.post(&check).bearer_auth("aubridge-admin");
        if let Some(model) = model {
            request = request.query(&[("model", model)]);
        }
        let check: Value = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(check["status"], expected);
        assert_eq!(check["model"], reported);
        assert_eq!(check["upstream_status"], json!(upstream_status));
        assert!(!check.to_string().contains("sensitive-upstream-text"));
        assert!(!check.to_string().contains("partial reply"));
        assert!(!check.to_string().contains("probe-test-secret"));
    }
    let bodies = upstream.seen_bodies.lock().await;
    assert_eq!(bodies.len(), 3);
    for (body, model) in bodies.iter().zip([MODEL, second_model, second_model]) {
        assert_eq!(serde_json::from_str::<Value>(body).unwrap()["model"], model);
    }
    assert_eq!(
        upstream.seen_headers.lock().await[0]["authorization"],
        "Bearer probe-test-secret"
    );
    assert_eq!(latest(&client, &base).await["recent_decisions"], json!([]));
    upstream_task.abort();
    gateway_task.abort();
}

#[tokio::test]
async fn messages_translation_removes_only_the_anthropic_attribution() {
    const ATTRIBUTION: &str =
        "x-anthropic-billing-header: cc_version=2.1.216.abc; cc_entrypoint=cli; cch=12345;";
    const KEEP: &str =
        "Keep project instructions.\nx-anthropic-billing-header: a normal discussion";
    let systems = [
        json!([{"type":"text","text":ATTRIBUTION},{"type":"text","text":KEEP}]),
        json!(format!("{ATTRIBUTION}\n{KEEP}")),
    ];
    let body = |system: &Value| {
        json!({"model":"demo","stream":true,"max_tokens":64,
        "system":system,"messages":[{"role":"user","content":ATTRIBUTION}]})
    };
    for (protocol, reply) in [("chat", CHAT_TOOL), ("responses", CODEX_REPLY)] {
        let (replay, upstream, upstream_task) =
            spawn(vec![Step::Sse { body: reply.into() }; systems.len()]).await;
        let state = GatewayState::from_manifests(
            BTreeMap::from([source("free", &upstream, "demo", "free_tier", protocol)]),
            "aubridge",
        );
        let (client, base, task) = gateway(state).await;
        let count = |system: &Value| {
            let request = client
                .post(format!("{base}/v1/messages/count_tokens"))
                .bearer_auth("aubridge-claude-code")
                .json(&body(system));
            async move {
                request
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            }
        };
        for (index, system) in systems.iter().enumerate() {
            client
                .post(format!("{base}/v1/messages"))
                .bearer_auth("aubridge-claude-code")
                .json(&body(system))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            let outbound: Value =
                serde_json::from_str(&replay.seen_bodies.lock().await[index]).unwrap();
            if protocol == "chat" {
                assert_eq!(outbound["messages"][0]["content"], KEEP);
                assert_eq!(outbound["messages"][1]["content"], ATTRIBUTION);
            } else {
                assert_eq!(outbound["instructions"], KEEP);
                assert_eq!(outbound["input"][0]["content"][0]["text"], ATTRIBUTION);
            }
            assert_eq!(count(system).await, count(&json!(KEEP)).await);
        }
        assert_eq!(replay.seen_bodies.lock().await.len(), systems.len());
        upstream_task.abort();
        task.abort();
    }

    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse { body: sse_frames(&[
        anth_start(), json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
        json!({"type":"message_stop"}),
    ]) }]).await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([source(
            "native",
            &upstream,
            "demo",
            "free_tier",
            "anthropic",
        )]),
        "aubridge",
    );
    let (client, base, task) = gateway(state).await;
    let mut native = body(&systems[0]);
    native["system"][0]["cache_control"] = json!({"type":"ephemeral"});
    let raw = serde_json::to_string_pretty(&native).unwrap();
    client
        .post(format!("{base}/v1/messages"))
        .bearer_auth("aubridge-claude-code")
        .body(raw.clone())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(&*replay.seen_bodies.lock().await, &[raw]);
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn subscription_busy_skips_to_ready_fallback_without_spending_attempts() {
    for busy_count in [1, 5] {
        let (busy, busy_base, busy_task) = spawn(vec![]).await;
        let (ready, ready_base, ready_task) = spawn(vec![Step::Sse {
            body: CHAT_TOOL.into(),
        }])
        .await;
        let ids: Vec<_> = (0..busy_count).map(|n| format!("busy-{n}")).collect();
        let manifests = ids
            .iter()
            .map(|id| source(id, &busy_base, id, "subscription", "chat"))
            .chain([source(
                "ready",
                &ready_base,
                "ready",
                "subscription",
                "chat",
            )])
            .collect();
        let state = GatewayState::from_manifests(manifests, "aubridge")
            .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        let mut held = Vec::new();
        for id in &ids {
            let slot = state
                .subscription_slot(id, manifest::schema::ProviderKind::Subscription)
                .unwrap();
            held.push(slot.try_acquire_many_owned(2).unwrap());
        }
        let group = format!("group/{}+ready", ids.join("+"));
        let (client, base, task) = gateway(state).await;
        let response = messages(&client, &base, &group, false)
            .send()
            .await
            .expect("the ready provider should respond while every busy slot is still held")
            .error_for_status()
            .unwrap();
        assert_eq!(response.headers()["x-aubridge-provider"], "ready");
        response.bytes().await.unwrap();
        assert!(busy.seen_bodies.lock().await.is_empty());
        assert_eq!(ready.seen_bodies.lock().await.len(), 1);
        let dashboard = latest(&client, &base).await;
        let row = &dashboard["recent_decisions"][0];
        let attempts = row["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0]["id"], "ready");
        let skipped = row["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), busy_count);
        for (skip, id) in skipped.iter().zip(ids) {
            assert_eq!(skip["id"], id);
            assert_eq!(skip["model"], id);
            assert_eq!(skip["reason"], "provider-busy");
        }
        drop(held);
        for server in [busy_task, ready_task, task] {
            server.abort();
        }
    }
}

#[tokio::test]
async fn subscription_busy_revisits_first_provider_after_fallback_fails() {
    let (first, first_base, first_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let released_on_fallback = Arc::new(std::sync::Mutex::new(None));
    let (second_base, second_task) = http_app(Router::new().route(
        "/chat/completions",
        post({
            let release = released_on_fallback.clone();
            move || async move {
                drop(release.lock().unwrap().take().expect("one fallback call"));
                (StatusCode::SERVICE_UNAVAILABLE, "{}")
            }
        }),
    ))
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("first", &first_base, "plan-a", "subscription", "chat"),
            source("second", &second_base, "plan-b", "subscription", "chat"),
        ]),
        "aubridge",
    )
    .with_ledger(ledger::Ledger::open(":memory:").unwrap());
    let slot = state
        .subscription_slot("first", manifest::schema::ProviderKind::Subscription)
        .unwrap();
    let still_held = slot.clone().try_acquire_owned().unwrap();
    *released_on_fallback.lock().unwrap() = Some(slot.try_acquire_owned().unwrap());
    let (client, base, task) = gateway(state).await;
    let response = messages(&client, &base, "group/plan-a+plan-b", false)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "first");
    response.bytes().await.unwrap();
    assert!(released_on_fallback.lock().unwrap().is_none());
    assert_eq!(first.seen_bodies.lock().await.len(), 1);
    let dashboard = latest(&client, &base).await;
    let row = &dashboard["recent_decisions"][0];
    let attempts = row["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["id"], "second");
    assert_eq!(attempts[0]["status"], 503);
    assert_eq!(attempts[1]["id"], "first");
    assert_eq!(attempts[1]["status"], 200);
    assert_eq!(row["skipped"], json!([]));
    drop(still_held);
    for server in [first_task, second_task, task] {
        server.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn subscription_busy_wait_timeout_and_cancellation_leave_slots_reusable() {
    // Keep virtual time stationary while the loopback requests are in flight.
    let ticker = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source("plan", &upstream, MODEL, "subscription", "chat")]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let slot = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap();
    let held = slot.clone().try_acquire_many_owned(2).unwrap();
    let waiting = Arc::new(tokio::sync::Notify::new());
    let cancel = Arc::new(tokio::sync::Notify::new());
    let app = gateway::app(state).layer(axum::middleware::from_fn({
        let waiting = waiting.clone();
        let cancel = cancel.clone();
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let waiting = waiting.clone();
            let cancel = cancel.clone();
            async move {
                use axum::response::IntoResponse;

                if !req.headers().contains_key("x-test-wait") {
                    return next.run(req).await;
                }
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, 16_384).await.unwrap();
                let req = axum::extract::Request::from_parts(parts, Body::from(body));
                // Poll the gateway first, then signal the test that the
                // request is pending. Dropping it models request cancellation.
                tokio::select! {
                    biased;
                    response = next.run(req) => response,
                    _ = async {
                        waiting.notify_one();
                        cancel.notified().await;
                    } => StatusCode::REQUEST_TIMEOUT.into_response(),
                }
            }
        }
    }));
    let (base, task) = http_app(app).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for outcome in ["no-candidate", "cancelled"] {
        let request = messages(&client, &base, MODEL, false).header("x-test-wait", "1");
        let mut pending = tokio::spawn(request.send());
        tokio::select! {
            _ = waiting.notified() => {}
            result = &mut pending => panic!("request completed before waiting: {result:?}"),
        }
        assert!(replay.seen_bodies.lock().await.is_empty());
        let status = if outcome == "no-candidate" {
            tokio::time::advance(Duration::from_secs(31)).await;
            StatusCode::BAD_GATEWAY
        } else {
            cancel.notify_one();
            StatusCode::REQUEST_TIMEOUT
        };
        assert_eq!(pending.await.unwrap().unwrap().status(), status);
        let dashboard = latest(&client, &base).await;
        let row = &dashboard["recent_decisions"][0];
        assert_eq!(row["outcome"], outcome);
        assert_eq!(row["attempts"], json!([]));
        assert_eq!(
            row["skipped"],
            json!([{"id":"plan","model":MODEL,"kind":"subscription","reason":"provider-busy"}])
        );
    }
    assert!(replay.seen_bodies.lock().await.is_empty());
    drop(held);
    assert_eq!(slot.available_permits(), 2);
    let response = messages(&client, &base, MODEL, false)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.headers()["x-aubridge-provider"], "plan");
    assert_eq!(replay.seen_bodies.lock().await.len(), 1);
    assert_eq!(slot.available_permits(), 2);
    for server in [ticker, upstream_task, task] {
        server.abort();
    }
}

#[tokio::test]
async fn subscription_connection_check_respects_occupied_slots() {
    let (replay, upstream, upstream_task) = spawn(vec![Step::Sse {
        body: CHAT_TOOL.into(),
    }])
    .await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "plan",
                &upstream,
                "plan-model",
                "subscription",
                "chat",
            )]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let slot = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap();
    let held = slot.acquire_many_owned(2).await.unwrap();
    let (base, task) = http_app(gateway::app(state)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let check = || {
        let request = client
            .post(format!("{base}/admin/providers/plan/check"))
            .bearer_auth("aubridge-admin");
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let busy = check().await;
    assert_eq!(busy["status"], "provider-busy");
    assert_eq!(busy["model"], "plan-model");
    assert!(busy["upstream_status"].is_null());
    assert!(replay.seen_bodies.lock().await.is_empty());
    drop(held);
    assert_eq!(check().await["status"], "ok");
    assert_eq!(replay.seen_bodies.lock().await.len(), 1);
    assert_eq!(latest(&client, &base).await["recent_decisions"], json!([]));
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn count_tokens_forwards_native_and_estimates_chat() {
    let (native, native_task) = http_app(Router::new().route(
        "/messages/count_tokens",
        post(
            |headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                assert_eq!(headers["anthropic-version"], "2023-06-01");
                assert_eq!(body["model"], "native-model");
                assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
                assert!(body["system"][0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("x-anthropic-billing-header:"));
                assert!(body.get("stream").is_none());
                assert!(body.get("max_tokens").is_none());
                axum::Json(json!({"input_tokens":88}))
            },
        ),
    ))
    .await;
    let state = GatewayState::from_manifests(
        BTreeMap::from([
            source("native", &native, "native-model", "free_tier", "anthropic"),
            source("chat", &native, MODEL, "free_tier", "chat"),
        ]),
        "aubridge",
    );
    let (client, base, gateway_task) = gateway(state).await;
    for (model, expected) in [("native-model", false), (MODEL, true)] {
        let reply: Value = client
            .post(format!("{base}/v1/messages/count_tokens"))
            .bearer_auth("aubridge")
            .json(&json!({
                "model":model,"messages":[{"role":"user","content":"hello world"}],
                "system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.216;", "cache_control":{"type":"ephemeral"}}],
            }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if expected {
            assert_eq!(reply["estimated"], true);
            assert!(reply["input_tokens"].as_u64().unwrap() > 0);
        } else {
            assert_eq!(reply["input_tokens"], 88);
            assert!(reply.get("estimated").is_none());
        }
    }
    native_task.abort();
    gateway_task.abort();
}

#[tokio::test(start_paused = true)]
async fn count_tokens_subscription_shares_slots_through_body_and_releases_them() {
    use axum::response::IntoResponse;
    use futures_util::{stream, StreamExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    // Real loopback I/O must finish before advancing the virtual timeout.
    let ticker = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let hold_body = Arc::new(AtomicBool::new(false));
    let body_started = Arc::new(Notify::new());
    let release_tail = Arc::new(Notify::new());
    let (upstream, upstream_task) = http_app(Router::new().route(
        "/messages/count_tokens",
        post({
            let calls = calls.clone();
            let hold_body = hold_body.clone();
            let body_started = body_started.clone();
            let release_tail = release_tail.clone();
            move || {
                let call = calls.fetch_add(1, Ordering::Relaxed);
                let hold_body = hold_body.load(Ordering::Relaxed);
                let body_started = body_started.clone();
                let release_tail = release_tail.clone();
                async move {
                    if !hold_body {
                        return axum::Json(json!({"input_tokens":88})).into_response();
                    }
                    match call {
                        1 => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        2 => "invalid json".into_response(),
                        3 => {
                            let error = std::io::Error::other("fixture disconnected");
                            Body::from_stream(stream::iter([
                                Ok(axum::body::Bytes::from_static(b"{")),
                                Err(error),
                            ]))
                            .into_response()
                        }
                        _ => {
                            let prefix = stream::once(async {
                                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                                    b"{\"input_tokens\":",
                                ))
                            });
                            let tail = stream::once(async move {
                                body_started.notify_one();
                                if call == 0 {
                                    release_tail.notified().await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                                Ok(axum::body::Bytes::from_static(b"88}"))
                            });
                            Body::from_stream(prefix.chain(tail)).into_response()
                        }
                    }
                }
            }
        }),
    ))
    .await;
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([source(
                "plan",
                &upstream,
                MODEL,
                "subscription",
                "anthropic",
            )]),
            "aubridge",
        )
        .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let slot = state
        .subscription_slot("plan", manifest::schema::ProviderKind::Subscription)
        .unwrap();
    let held = slot.clone().try_acquire_many_owned(2).unwrap();
    let cancel = Arc::new(Notify::new());
    let app = gateway::app(state).layer(axum::middleware::from_fn({
        let cancel = cancel.clone();
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let cancel = cancel.clone();
            async move {
                if !req.headers().contains_key("x-test-cancel") {
                    return next.run(req).await;
                }
                tokio::select! {
                    response = next.run(req) => response,
                    _ = cancel.notified() => StatusCode::REQUEST_TIMEOUT.into_response(),
                }
            }
        }
    }));
    let (base, task) = http_app(app).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let count = || {
        client
            .post(format!("{base}/v1/messages/count_tokens"))
            .bearer_auth("aubridge-claude-code")
            .json(&json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}))
    };
    let busy: Value = count().send().await.unwrap().json().await.unwrap();
    assert_eq!(busy["estimated"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    hold_body.store(true, Ordering::Relaxed);
    drop(held);

    let request = tokio::spawn(count().send());
    body_started.notified().await;
    assert_eq!(slot.available_permits(), 1);
    assert!(!request.is_finished());
    release_tail.notify_one();
    let exact: Value = request.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(exact, json!({"input_tokens":88}));
    assert_eq!(slot.available_permits(), 2);

    for _ in 0..3 {
        let reply: Value = count().send().await.unwrap().json().await.unwrap();
        assert_eq!(reply["estimated"], true);
        assert_eq!(slot.available_permits(), 2);
    }
    let request = tokio::spawn(count().send());
    body_started.notified().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(!request.is_finished());
    assert_eq!(slot.available_permits(), 1);
    tokio::time::advance(Duration::from_millis(1001)).await;
    let timed_out: Value = request.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(timed_out["estimated"], true);
    assert_eq!(slot.available_permits(), 2);

    let request = tokio::spawn(count().header("x-test-cancel", "1").send());
    body_started.notified().await;
    assert_eq!(slot.available_permits(), 1);
    cancel.notify_one();
    assert_eq!(
        request.await.unwrap().unwrap().status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert_eq!(slot.available_permits(), 2);
    assert_eq!(calls.load(Ordering::Relaxed), 6);
    let dashboard = latest(&client, &base).await;
    assert_eq!(dashboard["recent_decisions"], json!([]));
    assert_eq!(dashboard["monthly_savings_usd"], 0.0);
    for server in [ticker, upstream_task, task] {
        server.abort();
    }
}

#[tokio::test]
async fn third_party_responses_uses_its_own_key_instead_of_codex_oauth() {
    let (replay, url, upstream_task) = spawn(vec![Step::Sse {
        body: CODEX_REPLY.into(),
    }])
    .await;
    let dir = std::env::temp_dir().join(format!("aubridge-v1-oauth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"tokens":{"access_token":"private-test-token","account_id":"private-test-account"}}"#,
    )
    .unwrap();
    let state = GatewayState::with_keys(
        BTreeMap::from([source(
            "third-party",
            &url,
            "responses-model",
            "subscription",
            "responses",
        )]),
        "aubridge",
        BTreeMap::from([("third-party".into(), "provider-key".into())]),
    )
    .with_codex_channel(gateway::chatgpt::ChatGptChannel::new(
        auth,
        dir.join("config.toml"),
    ));
    let (client, base, gateway_task) = gateway(state).await;
    let reply = messages(&client, &base, "responses-model", true)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.headers()["x-aubridge-provider"], "third-party");
    assert!(reply.text().await.unwrap().contains("message_stop"));
    let seen = replay.seen_headers.lock().await;
    assert_eq!(seen[0]["authorization"], "Bearer provider-key");
    assert!(!seen[0].contains_key("chatgpt-account-id"));
    upstream_task.abort();
    gateway_task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

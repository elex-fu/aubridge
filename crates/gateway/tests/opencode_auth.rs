//! Source selection and live OpenCode reads through the production HTTP gateway.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::{routing::post, Router};
use gateway::chatgpt::{ChatGptChannel, LoginSource};
use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::{json, Value};

const TEXT_SSE: &str = include_str!("../../translate/tests/fixtures/codex_text.sse");

struct Fixture {
    dir: PathBuf,
    codex: PathBuf,
    opencode: PathBuf,
    settings: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "aubridge-oc-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let codex = dir.join("codex.json");
        let opencode = dir.join("opencode.json");
        let settings = dir.join("settings.json");
        std::fs::write(&codex, json!({"tokens":{
            "access_token":"codex-access-fixture", "refresh_token":"codex-refresh-fixture", "account_id":"codex-account"
        }}).to_string()).unwrap();
        std::fs::write(&opencode, oc_login("oc-access-fixture", "oc-account")).unwrap();
        Self {
            dir,
            codex,
            opencode,
            settings,
        }
    }

    fn channel(&self) -> ChatGptChannel {
        ChatGptChannel::new(self.codex.clone(), self.dir.join("codex.toml"))
            .with_opencode_auth(self.opencode.clone())
    }

    fn state(&self) -> GatewayState {
        GatewayState::from_manifests(BTreeMap::new(), "fixture")
            .with_codex_channel(self.channel())
            .with_settings(self.settings.clone(), self.dir.join("stash"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn oc_login(access: &str, account: &str) -> String {
    json!({"openai":{
        "type":"oauth", "access":access, "refresh":"oc-refresh-fixture", "accountId":account
    }, "unrelated":{"type":"unknown","broken":42}})
    .to_string()
}

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

#[tokio::test]
async fn source_settings_and_opencode_hot_reload_use_only_the_selected_login() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.settings, r#"{"future":42}"#).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let upstream = Router::new().route(
        "/responses",
        post({
            let seen = seen.clone();
            move |headers: HeaderMap| {
                seen.lock().unwrap().push(headers);
                async { ([("content-type", "text/event-stream")], TEXT_SSE) }
            }
        }),
    );
    let (base, upstream_task) = serve(upstream).await;
    let manifest: Manifest = serde_json::from_value(json!({
        "id":"chatgpt", "version":"1", "kind":"subscription",
        "provider":{"base":base,"auth":"bearer","protocol":"responses"},
        "free_models":["fixture-model"],
        "permissions":{"net":["127.0.0.1"],"store_key":false}
    }))
    .unwrap();
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("chatgpt".to_string(), manifest)]),
            "fixture",
        )
        .with_codex_channel(fixture.channel().with_responses_base(base))
        .with_settings(fixture.settings.clone(), fixture.dir.join("stash"))
        .with_client(client()),
    );
    let (gateway, gateway_task) = serve(gateway::app(state.clone())).await;
    let client = client();
    let request = || {
        client
            .post(format!("{gateway}/v1/messages"))
            .bearer_auth("fixture-ui-test")
            .json(
                &json!({"model":"fixture-model", "stream":false, "max_tokens":512,
            "messages":[{"role":"user","content":"只回复 OK。"}]}),
            )
    };
    let initial = request().send().await.unwrap();
    assert_eq!(initial.status(), 200);
    assert_eq!(initial.headers()["x-aubridge-provider"], "chatgpt");
    assert_eq!(initial.json::<Value>().await.unwrap()["type"], "message");
    assert_eq!(
        seen.lock().unwrap()[0]["authorization"],
        "Bearer codex-access-fixture"
    );

    for invalid in [
        json!({}),
        json!({"share_login":false,"chatgpt_login_source":"opencode"}),
        json!({"chatgpt_login_source":"unknown"}),
        json!({"chatgpt_login_source":null}),
        json!({"share_login":"false"}),
    ] {
        assert_eq!(
            client
                .post(format!("{gateway}/admin/settings"))
                .bearer_auth("fixture-admin")
                .json(&invalid)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(state.chatgpt_login_metadata().source, Some("codex"));
    }
    assert_eq!(
        client
            .post(format!("{gateway}/admin/settings"))
            .bearer_auth("fixture-ui-test")
            .json(&json!({"chatgpt_login_source":"opencode"}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let changed: Value = client
        .post(format!("{gateway}/admin/settings"))
        .bearer_auth("fixture-admin")
        .json(&json!({"chatgpt_login_source":"opencode","unknown":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["chatgpt_login_source"], "opencode");
    let saved: Value = serde_json::from_slice(&std::fs::read(&fixture.settings).unwrap()).unwrap();
    assert_eq!(saved["chatgpt_login_source"], "opencode");
    assert_eq!(saved["future"], 42);

    let mut expected = 1;
    for (content, success, status) in [
        (
            Some(oc_login("oc-access-fixture", "oc-account")),
            true,
            "ready",
        ),
        (
            Some(oc_login("rotated-access-fixture", "rotated-account")),
            true,
            "ready",
        ),
        (Some("{".to_string()), false, "unavailable"),
        (None, false, "missing"),
        (
            Some(r#"{"openai":{"type":"api","key":"not-oauth"}}"#.to_string()),
            false,
            "missing",
        ),
        (
            Some(oc_login("recovered-access-fixture", "recovered-account")),
            true,
            "ready",
        ),
    ] {
        if let Some(content) = content {
            std::fs::write(&fixture.opencode, content).unwrap();
        } else {
            std::fs::remove_file(&fixture.opencode).unwrap();
        }
        let response = request().send().await.unwrap();
        assert_eq!(response.status().is_success(), success);
        let _ = response.bytes().await.unwrap();
        expected += usize::from(success);
        assert_eq!(seen.lock().unwrap().len(), expected);
        let metadata: Value = client
            .get(format!("{gateway}/admin/settings"))
            .bearer_auth("fixture-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(metadata["chatgpt_login_source"], "opencode");
        assert_eq!(metadata["chatgpt_login_status"], status);
        assert_eq!(
            metadata["chatgpt_login_path"],
            fixture.opencode.to_string_lossy().as_ref()
        );
        assert!(!metadata.to_string().contains("access-fixture"));
        assert!(!metadata.to_string().contains("refresh-fixture"));
    }
    let sent = seen.lock().unwrap();
    for (headers, (access, account)) in sent.iter().skip(1).zip([
        ("oc-access-fixture", "oc-account"),
        ("rotated-access-fixture", "rotated-account"),
        ("recovered-access-fixture", "recovered-account"),
    ]) {
        assert_eq!(headers["authorization"], format!("Bearer {access}"));
        assert_eq!(headers["chatgpt-account-id"], account);
    }
    drop(sent);
    assert_eq!(state.providers_overview()[0]["login_source"], "opencode");
    gateway_task.abort();
    upstream_task.abort();
}

/// 首启自动判定(opencode-auth-design §4,2026-11-14):无持久化
/// 选择时,Codex 登录可读则 codex;Codex 干净缺失且 OpenCode openai
/// 登录可读才 opencode;Codex 损坏或缺两者则 codex。已有持久化选择
/// 不受检测影响(现有安装不换账号)。
#[test]
fn first_run_auto_detects_login_source_only_without_a_saved_choice() {
    // 双登录都在:与原缺省一致,判 codex。
    let fixture = Fixture::new();
    assert_eq!(
        fixture.state().chatgpt_login_metadata().source,
        Some("codex")
    );

    // Codex 干净缺失、OpenCode 有登录:首次启动缺省 opencode。
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.codex).unwrap();
    let state = fixture.state();
    let metadata = state.chatgpt_login_metadata();
    assert_eq!(metadata.source, Some("opencode"));
    assert_eq!(metadata.status, "ready");

    // 两者皆缺:回 codex 缺省(状态 missing,不 fail-closed)。
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.codex).unwrap();
    std::fs::remove_file(&fixture.opencode).unwrap();
    let metadata = fixture.state().chatgpt_login_metadata();
    assert_eq!(metadata.source, Some("codex"));
    assert_eq!(metadata.status, "missing");

    // Codex 文件损坏时不判给 opencode:保持 codex,由状态暴露错误。
    let fixture = Fixture::new();
    std::fs::write(&fixture.codex, "{").unwrap();
    let metadata = fixture.state().chatgpt_login_metadata();
    assert_eq!(metadata.source, Some("codex"));
    assert_eq!(metadata.status, "unavailable");

    // 已持久化显式选择(现有安装):有 OpenCode 登录也不重估。
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.codex).unwrap();
    std::fs::write(&fixture.settings, r#"{"chatgpt_login_source":"codex"}"#).unwrap();
    let state = fixture.state();
    assert_eq!(state.chatgpt_login_metadata().source, Some("codex"));
    // 显式选择后保存仍是完整值,不会落回空串触发再判定。
    state
        .set_chatgpt_login_source(LoginSource::OpenCode)
        .unwrap();
    let saved = std::fs::read_to_string(&fixture.settings).unwrap();
    assert!(saved.contains(r#""chatgpt_login_source": "opencode""#));
    assert_eq!(
        fixture.state().chatgpt_login_metadata().source,
        Some("opencode")
    );
}

#[test]
fn invalid_source_configuration_fails_closed_and_failed_save_preserves_selection() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.state().chatgpt_login_metadata().source,
        Some("codex")
    );
    for document in [
        "{",
        r#"{"chatgpt_login_source":"unknown"}"#,
        r#"{"chatgpt_login_source":null}"#,
        r#"{"chatgpt_login_source":42}"#,
    ] {
        std::fs::write(&fixture.settings, document).unwrap();
        let metadata = fixture.state().chatgpt_login_metadata();
        assert_eq!(metadata.source, None);
        assert_eq!(metadata.status, "unavailable");
        assert_eq!(metadata.path, None);
    }
    std::fs::write(&fixture.settings, r#"{"chatgpt_login_source":"opencode"}"#).unwrap();
    let state = fixture.state();
    assert_eq!(state.chatgpt_login_metadata().source, Some("opencode"));
    std::fs::remove_file(&fixture.settings).unwrap();
    std::fs::create_dir(&fixture.settings).unwrap();
    assert!(state.set_chatgpt_login_source(LoginSource::Codex).is_err());
    assert_eq!(state.chatgpt_login_metadata().source, Some("opencode"));
    assert!(!fixture.dir.join("stash").exists());
}

#[tokio::test]
async fn an_inflight_401_cannot_switch_login_sources_even_with_identical_access() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = Fixture::new();
    std::fs::write(&fixture.settings, r#"{"chatgpt_login_source":"codex"}"#).unwrap();
    std::fs::write(
        &fixture.opencode,
        oc_login("codex-access-fixture", "oc-account"),
    )
    .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let upstream = Router::new()
        .route(
            "/responses",
            post({
                let started = started.clone();
                let release = release.clone();
                let requests = requests.clone();
                move |headers: HeaderMap| {
                    let started = started.clone();
                    let release = release.clone();
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        if headers["chatgpt-account-id"] == "oc-account" {
                            return axum::response::IntoResponse::into_response((
                                [("content-type", "text/event-stream")],
                                TEXT_SSE,
                            ));
                        }
                        assert_eq!(headers["chatgpt-account-id"], "codex-account");
                        started.notify_one();
                        release.notified().await;
                        axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error":{"type":"unauthorized"}})),
                        ))
                    }
                }
            }),
        )
        .route(
            "/token",
            post({
                let refreshes = refreshes.clone();
                move || {
                    let refreshes = refreshes.clone();
                    async move {
                        refreshes.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({"access_token":"must-not-be-used"}))
                    }
                }
            }),
        );
    let (base, upstream_task) = serve(upstream).await;
    let manifest: Manifest = serde_json::from_value(json!({
        "id":"chatgpt", "version":"1", "kind":"subscription",
        "provider":{"base":base,"auth":"bearer","protocol":"responses"},
        "free_models":["fixture-model"],
        "permissions":{"net":["127.0.0.1"],"store_key":false}
    }))
    .unwrap();
    let channel = fixture
        .channel()
        .with_token_endpoint(format!("{base}/token"))
        .with_responses_base(base);
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("chatgpt".to_string(), manifest)]),
            "fixture",
        )
        .with_codex_channel(channel)
        .with_settings(fixture.settings.clone(), fixture.dir.join("stash"))
        .with_client(client()),
    );
    let (gateway, gateway_task) = serve(gateway::app(state.clone())).await;
    let request_gateway = gateway.clone();
    let pending = tokio::spawn(async move {
        client()
            .post(format!("{request_gateway}/v1/messages"))
            .bearer_auth("fixture-ui-test")
            .json(
                &json!({"model":"fixture-model","stream":false,"max_tokens":512,
                "messages":[{"role":"user","content":"hi"}]}),
            )
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    state
        .set_chatgpt_login_source(LoginSource::OpenCode)
        .unwrap();
    release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(!response.status().is_success());
    let _ = response.bytes().await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(state.chatgpt_login_metadata().source, Some("opencode"));
    let next = client()
        .post(format!("{gateway}/v1/messages"))
        .bearer_auth("fixture-ui-test")
        .json(
            &json!({"model":"fixture-model","stream":false,"max_tokens":512,
            "messages":[{"role":"user","content":"hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        next.status(),
        200,
        "old login rejection must not freeze the new selection"
    );
    let _ = next.bytes().await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    gateway_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn opencode_file_rotation_after_401_does_not_use_api_key_cooldown_or_fallback() {
    use axum::response::IntoResponse;
    let fixture = Fixture::new();
    std::fs::write(&fixture.settings, r#"{"chatgpt_login_source":"opencode"}"#).unwrap();
    let old = json!({"openai":{"type":"oauth","access":"old-oc-access","accountId":"oc-account"}});
    std::fs::write(&fixture.opencode, old.to_string()).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let upstream = Router::new().route(
        "/responses",
        post({
            let seen = seen.clone();
            move |headers: HeaderMap| {
                seen.lock().unwrap().push(headers.clone());
                async move {
                    if headers["authorization"] == "Bearer new-oc-access" {
                        ([("content-type", "text/event-stream")], TEXT_SSE).into_response()
                    } else {
                        (
                            axum::http::StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"error":{"type":"unauthorized"}})),
                        )
                            .into_response()
                    }
                }
            }
        }),
    );
    let (base, upstream_task) = serve(upstream).await;
    let manifest: Manifest = serde_json::from_value(json!({
        "id":"chatgpt", "version":"1", "kind":"subscription",
        "provider":{"base":base,"auth":"bearer","protocol":"responses"},
        "free_models":["fixture-model"],
        "permissions":{"net":["127.0.0.1"],"store_key":false}
    }))
    .unwrap();
    let state = Arc::new(
        GatewayState::from_manifests(
            BTreeMap::from([("chatgpt".to_string(), manifest)]),
            "fixture",
        )
        .with_codex_channel(fixture.channel().with_responses_base(base))
        .with_settings(fixture.settings.clone(), fixture.dir.join("stash"))
        .with_client(client()),
    );
    let (gateway, gateway_task) = serve(gateway::app(state.clone())).await;
    let client = client();
    let request = || {
        client
            .post(format!("{gateway}/v1/messages"))
            .bearer_auth("fixture-ui-test")
            .json(
                &json!({"model":"fixture-model","stream":false,"max_tokens":512,
            "messages":[{"role":"user","content":"hi"}]}),
            )
    };
    let rejected = request().send().await.unwrap();
    assert!(!rejected.status().is_success());
    let _ = rejected.bytes().await.unwrap();
    assert_eq!(state.snapshot()[0].key_retry_at, None);
    let mut fresh = old;
    fresh["openai"]["access"] = json!("new-oc-access");
    let replacement = fixture.dir.join("replacement.json");
    std::fs::write(&replacement, fresh.to_string()).unwrap();
    std::fs::rename(replacement, &fixture.opencode).unwrap();
    let recovered = request().send().await.unwrap();
    assert_eq!(
        recovered.status(),
        200,
        "a rotated OAuth login must be read on the next ordinary request"
    );
    let _ = recovered.bytes().await.unwrap();
    assert_eq!(state.snapshot()[0].key_retry_at, None);
    let sent = seen.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["authorization"], "Bearer old-oc-access");
    assert_eq!(sent[1]["authorization"], "Bearer new-oc-access");
    assert!(sent
        .iter()
        .all(|headers| headers["chatgpt-account-id"] == "oc-account"));
    drop(sent);
    gateway_task.abort();
    upstream_task.abort();
}

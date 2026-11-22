//! Late quota replies must not mutate a replacement connection or login.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::{
    routing::{get, post},
    Json, Router,
};
use gateway::chatgpt::{ChatGptChannel, LoginSource};
use gateway::state::GatewayState;
use serde_json::{json, Value};
use tokio::sync::Notify;

#[derive(Default)]
struct QuotaStub {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, server)
}

async fn quota_stub() -> (String, Arc<QuotaStub>, tokio::task::JoinHandle<()>) {
    let control = Arc::new(QuotaStub::default());
    let shared = control.clone();
    let app = Router::new().route(
        "/quota",
        get(move |headers: HeaderMap| {
            let shared = shared.clone();
            async move {
                shared.calls.fetch_add(1, Ordering::SeqCst);
                let old = headers["authorization"]
                    .to_str()
                    .unwrap()
                    .ends_with("key-a");
                if old {
                    shared.entered.notify_one();
                    shared.release.notified().await;
                }
                let used = if old { 11 } else { 77 };
                Json(json!({
                    "data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,
                        "percentage":used,"nextResetTime":1800000000000_i64}]},
                    "rate_limit":{"secondary_window":{"used_percent":used,"reset_at":1800000000}}
                }))
            }
        }),
    );
    let (base, task) = serve(app).await;
    (format!("{base}/quota"), control, task)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn wait_for_query(stub: &QuotaStub) {
    tokio::time::timeout(Duration::from_secs(3), stub.entered.notified())
        .await
        .expect("the old credential's quota request reached the local upstream");
}

async fn providers(base: &str) -> Value {
    client()
        .get(format!("{base}/admin/providers"))
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn plan_reply_is_discarded_after_key_rotation_or_removal() {
    for remove in [false, true] {
        let (endpoint, stub, upstream) = quota_stub().await;
        let mut manifest = registry::zhipu();
        manifest.id = "quota-plan-fixture".into();
        let base = manifest.provider.base.clone();
        let state = Arc::new(
            GatewayState::with_keys(
                BTreeMap::from([(manifest.id.clone(), manifest)]),
                "aubridge",
                BTreeMap::from([("quota-plan-fixture".into(), "synthetic-key-a".into())]),
            )
            .with_client(client())
            .with_key_store(Arc::new(credentials::MemoryStore::default()))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap())
            .with_plan_fetcher_for(
                &base,
                Arc::new(credentials::planquota::PlanFetcher::new(
                    endpoint,
                    credentials::planquota::PlanAuth::RawKey,
                )),
            ),
        );
        let (gateway, server) = serve(gateway::app(state.clone())).await;
        let first = tokio::spawn({
            let gateway = gateway.clone();
            async move { providers(&gateway).await }
        });
        wait_for_query(&stub).await;
        if remove {
            state.clear_provider_key("quota-plan-fixture").unwrap();
        } else {
            state
                .set_provider_key("quota-plan-fixture", "synthetic-key-b")
                .unwrap();
        }
        stub.release.notify_one();
        first.await.unwrap();
        assert_eq!(
            state.snapshot()[0].util_5h,
            None,
            "old key's result changed current state"
        );
        assert!(
            state
                .quota_history(Some("quota-plan-fixture"), 0)
                .is_empty(),
            "old key's result entered history"
        );
        let body = providers(&gateway).await;
        let expected = if remove { Value::Null } else { json!(77.0) };
        assert_eq!(
            body["providers"][0]["util_5h"], expected,
            "new credential must read its own quota"
        );
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            if remove { 1 } else { 2 }
        );
        server.abort();
        upstream.abort();
    }
}

struct LoginFiles {
    root: PathBuf,
    codex: PathBuf,
    opencode: PathBuf,
}

impl LoginFiles {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aubridge-quota-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let files = Self {
            codex: root.join("codex.json"),
            opencode: root.join("opencode.json"),
            root,
        };
        files.codex_key("synthetic-key-a");
        std::fs::write(
            &files.opencode,
            json!({"openai":{
                "type":"oauth", "access":"synthetic-key-b", "refresh":"", "accountId":"account-b"
            }})
            .to_string(),
        )
        .unwrap();
        files
    }

    fn codex_key(&self, key: &str) {
        let tmp = self.root.join("codex.new");
        std::fs::write(
            &tmp,
            json!({"tokens":{
                "access_token":key, "refresh_token":"", "account_id":key
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::rename(tmp, &self.codex).unwrap();
    }
}

impl Drop for LoginFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn wham_reply_is_discarded_after_source_change_file_rotation_or_logout() {
    for change in ["source", "file", "logout"] {
        let files = LoginFiles::new();
        let (endpoint, stub, upstream) = quota_stub().await;
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([("chatgpt".into(), registry::chatgpt())]),
                "aubridge",
            )
            .with_client(client())
            .with_codex_channel(
                ChatGptChannel::new(files.codex.clone(), files.root.join("codex.toml"))
                    .with_opencode_auth(files.opencode.clone()),
            )
            .with_settings(files.root.join("settings.json"), files.root.join("stash"))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap())
            .with_wham_endpoint(endpoint),
        );
        let (base, server) = serve(gateway::app(state.clone())).await;
        let first = tokio::spawn({
            let base = base.clone();
            async move {
                client()
                    .get(format!("{base}/admin/dashboard"))
                    .bearer_auth("aubridge-admin")
                    .send()
                    .await
                    .unwrap()
            }
        });
        wait_for_query(&stub).await;
        match change {
            "source" => state
                .set_chatgpt_login_source(LoginSource::OpenCode)
                .unwrap(),
            "file" => files.codex_key("synthetic-key-b"),
            "logout" => std::fs::remove_file(&files.codex).unwrap(),
            _ => unreachable!(),
        }
        stub.release.notify_one();
        assert!(first.await.unwrap().status().is_success());
        assert_eq!(
            state.snapshot()[0].util_5h,
            None,
            "{change}: old login's result changed current state"
        );
        assert!(
            state.quota_history(Some("chatgpt"), 0).is_empty(),
            "{change}: old login's result entered history"
        );
        state.sample_wham_usage().await;
        assert_eq!(
            state.snapshot()[0].util_5h,
            (change != "logout").then_some(77.0)
        );
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            if change == "logout" { 1 } else { 2 }
        );
        server.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn late_inference_headers_cannot_cool_down_or_sample_a_replacement_login() {
    for change in ["source", "file", "logout"] {
        let files = LoginFiles::new();
        let gate = Arc::new(QuotaStub::default());
        let upstream = Router::new().route("/responses", post({
            let gate = gate.clone();
            move || {
                let gate = gate.clone();
                async move {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                    (axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("x-codex-primary-used-percent", "100"),
                         ("x-codex-primary-reset-after-seconds", "3600")],
                        Json(json!({"error":{"type":"usage_limit_reached","message":"window exhausted"}})))
                }
            }
        }));
        let (base, upstream) = serve(upstream).await;
        let mut manifest = registry::chatgpt();
        manifest.provider.base = base.clone();
        manifest.permissions.net = vec!["127.0.0.1".into()];
        manifest.free_models = vec!["fixture-model".into()];
        let state = Arc::new(
            GatewayState::from_manifests(
                BTreeMap::from([("chatgpt".into(), manifest)]),
                "aubridge",
            )
            .with_client(client())
            .with_codex_channel(
                ChatGptChannel::new(files.codex.clone(), files.root.join("codex.toml"))
                    .with_opencode_auth(files.opencode.clone())
                    .with_responses_base(base),
            )
            .with_settings(files.root.join("settings.json"), files.root.join("stash"))
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
        );
        let (base, server) = serve(gateway::app(state.clone())).await;
        let mut request = tokio::spawn(async move {
            client()
                .post(format!("{base}/v1/messages"))
                .bearer_auth("aubridge-ui-test")
                .json(
                    &json!({"model":"fixture-model","max_tokens":16,"stream":false,
                    "messages":[{"role":"user","content":"OK"}]}),
                )
                .send()
                .await
                .unwrap()
        });
        tokio::select! {
            response = &mut request => panic!("request ended before upstream: {}", response.unwrap().text().await.unwrap()),
            () = wait_for_query(&gate) => {}
        }
        match change {
            "source" => state
                .set_chatgpt_login_source(LoginSource::OpenCode)
                .unwrap(),
            "file" => files.codex_key("synthetic-key-b"),
            "logout" => std::fs::remove_file(&files.codex).unwrap(),
            _ => unreachable!(),
        }
        gate.release.notify_one();
        // The old account no longer belongs to the candidate set, so its
        // failure cannot manufacture a current-account cooldown response.
        assert_eq!(request.await.unwrap().status(), 502);
        let source = &state.snapshot()[0];
        assert!(
            source.quota.windows.is_empty(),
            "{change}: old response sampled quota"
        );
        assert!(source.healthy, "{change}: old response changed health");
        assert_eq!(
            source.rest_until, None,
            "{change}: old response changed cooldown"
        );
        assert!(!source.sunk, "{change}: old response sank the new login");
        assert!(state.quota_history(Some("chatgpt"), 0).is_empty());
        server.abort();
        upstream.abort();
    }
}

//! 配额窗口历史 + plan 查询集成测试(quota-plan-design §5):
//! ① record_utilization 咽喉落历史,quota-history 路由读回 +
//! admin key 401 口径;② 本地假 upstream(plan fetcher 注入)→
//! providers 路由触发 sample_plan_quotas → util 字段 + 历史;
//! ③ 智谱裸 key Authorization 头形态与缓存单飞;
//! ④ 未挂 ledger 零影响。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::routing::get as axum_get;
use axum::Router;
use gateway::state::GatewayState;
use manifest::schema::Manifest;
use serde_json::{json, Value};

fn zhipu_manifest() -> Manifest {
    // registry::zhipu() 的形状(base = open.bigmodel.cn,plan_source
    // 分发命中;auth api-key)。
    serde_json::from_value(serde_json::json!({
        "id": "zhipu", "version": "1",
        "provider": { "base": "https://open.bigmodel.cn/api/paas/v4", "auth": "api-key" },
        "free_models": ["glm-4.5-flash"],
        "permissions": { "net": ["open.bigmodel.cn"], "store_key": true }
    }))
    .unwrap()
}

fn kimi_manifest() -> Manifest {
    serde_json::from_value(serde_json::json!({
        "id": "kimi-code", "version": "1", "kind": "subscription",
        "provider": { "base": "https://api.kimi.com/coding/v1", "auth": "api-key",
                      "protocol": "anthropic" },
        "free_models": ["kimi-k3"],
        "permissions": { "net": ["api.kimi.com"], "store_key": true }
    }))
    .unwrap()
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

async fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn record_utilization_lands_in_history_route() {
    let manifests = BTreeMap::from([("kimi-code".to_string(), kimi_manifest())]);
    let state = Arc::new(
        GatewayState::from_manifests(manifests, "aubridge")
            .with_admin_token("test-admin".into())
            .with_ledger(ledger::Ledger::open(":memory:").unwrap()),
    );
    let (base, server) = serve(state.clone()).await;
    let client = client().await;

    // 咽喉落历史:钦定 resets_at,两次读数(同周期 <5min → 折叠
    // 为一点)。
    state.record_utilization("kimi-code", Some(20.0), Some(60.0));
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 18_000;
    state.apply_reset_sample("kimi-code", reset);
    state.record_utilization("kimi-code", Some(25.0), Some(61.0));

    let body: Value = client
        .get(format!("{base}/admin/quota-history?provider=kimi-code"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    // 两窗口各一组(节流折叠);left = 100 - util。
    assert_eq!(items.len(), 2, "按窗口分组: {items:?}");
    let w5h = items.iter().find(|p| p["window"] == "5h").unwrap()["points"]
        .as_array()
        .unwrap();
    let w7d = items.iter().find(|p| p["window"] == "7d").unwrap()["points"]
        .as_array()
        .unwrap();
    assert_eq!(w5h.len(), 1, "节流折叠: {items:?}");
    assert_eq!(w7d.len(), 1);
    assert_eq!(w5h[0]["left"], json!(75.0));
    assert_eq!(w7d[0]["left"], json!(39.0));
    assert_eq!(w5h[0]["resets_at"], json!(reset));

    // 无 provider 过滤返回全部窗口。
    let all: Value = client
        .get(format!("{base}/admin/quota-history"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 2);

    // 鉴权口径:无 admin key → 401。
    let status = client
        .get(format!("{base}/admin/quota-history"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 401);
    server.abort();

    // 未挂 ledger:quota-history 空,record_utilization 仍更新内存态。
    let state2 = Arc::new(GatewayState::from_manifests(
        BTreeMap::from([("zhipu".to_string(), zhipu_manifest())]),
        "aubridge",
    ));
    state2.record_utilization("zhipu", Some(30.0), None);
    let (base2, server2) = serve(state2).await;
    let empty: Value = client
        .get(format!("{base2}/admin/quota-history?provider=zhipu"))
        // from_manifests 默认 admin token = <token>-admin。
        .bearer_auth("aubridge-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty["items"], json!([]), "未挂 ledger → 空历史");
    server2.abort();
}

/// 智谱假 upstream:回 plan JSON,记录 Authorization 头(应为裸
/// key,非 Bearer——magpie 实测口径)与命中次数。
async fn zhipu_stub() -> (String, Arc<Mutex<Option<String>>>, Arc<AtomicUsize>) {
    let seen = Arc::new(Mutex::new(None));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();
    let hits2 = hits.clone();
    let app = Router::new().route(
        "/api/monitor/usage/quota/limit",
        axum_get(move |headers: axum::http::HeaderMap| {
            let seen = seen2.clone();
            let hits = hits2.clone();
            async move {
                *seen.lock().unwrap() = headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().into());
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({
                    "success": true,
                    "data": { "level": "pro", "limits": [
                        {"type": "TOKENS_LIMIT", "unit": 3, "number": 5,
                         "percentage": 12, "nextResetTime": 1758800000000i64},
                        {"type": "TOKENS_LIMIT", "unit": 6, "number": 1,
                         "percentage": 40, "nextResetTime": 1758864000000i64}
                    ]}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen, hits)
}

/// zhipu 源 + 注入本地假端点的 fetcher(kimi-code 不给 key,
/// 避免测试真实外呼 api.kimi.com)。
fn wired_state(stub: &str, seen: Arc<Mutex<Option<String>>>) -> Arc<GatewayState> {
    let fetcher = Arc::new(credentials::planquota::PlanFetcher::new(
        format!("{stub}/api/monitor/usage/quota/limit"),
        credentials::planquota::PlanAuth::RawKey,
    ));
    let manifests = BTreeMap::from([
        ("zhipu".to_string(), zhipu_manifest()),
        ("kimi-code".to_string(), kimi_manifest()),
    ]);
    // 只给 zhipu key:kimi 无 key → sample_plan_quotas 跳过。
    let keys = BTreeMap::from([("zhipu".to_string(), "fixture-zhipu-key".to_string())]);
    let _ = seen;
    Arc::new(
        GatewayState::with_keys(manifests, "aubridge", keys)
            .with_client(reqwest::Client::builder().no_proxy().build().unwrap())
            .with_admin_token("test-admin".into())
            .with_ledger(ledger::Ledger::open(":memory:").unwrap())
            .with_plan_fetcher_for("https://open.bigmodel.cn/api/paas/v4", fetcher),
    )
}

#[tokio::test]
async fn providers_route_samples_plan_quota_and_shows_util() {
    let (stub, seen, _hits) = zhipu_stub().await;
    let state = wired_state(&stub, seen.clone());
    let (base, server) = serve(state).await;
    let client = client().await;

    // providers 路由触发 sample_plan_quotas。
    let body: Value = client
        .get(format!("{base}/admin/providers"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let providers = body["providers"].as_array().unwrap();
    let zhipu = providers
        .iter()
        .find(|p| p["id"] == "zhipu")
        .unwrap()
        .clone();
    assert_eq!(zhipu["util_5h"], json!(12.0));
    assert_eq!(zhipu["util_7d"], json!(40.0));
    assert_eq!(zhipu["resets_at"], json!(1_758_800_000));
    // 智谱:裸 key 作 Authorization(magpie 实测口径)。
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("fixture-zhipu-key"),
        "Authorization = 裸 key"
    );
    // kimi 无 key → 不查,字段保持未知。
    let kimi = providers
        .iter()
        .find(|p| p["id"] == "kimi-code")
        .unwrap()
        .clone();
    assert_eq!(kimi["util_5h"], Value::Null);

    // plan 读数顺带落历史(咽喉共用)。
    let history: Value = client
        .get(format!("{base}/admin/quota-history?provider=zhipu"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = history["items"].as_array().unwrap();
    assert!(!items.is_empty(), "plan 读数入历史: {items:?}");
    let w5h = items.iter().find(|p| p["window"] == "5h").unwrap();
    let points = w5h["points"].as_array().unwrap();
    assert!(!points.is_empty());
    assert_eq!(points[0]["left"], json!(88.0));
    assert_eq!(points[0]["resets_at"], json!(1_758_800_000));

    server.abort();
}

#[tokio::test]
async fn plan_cache_limits_upstream_hits() {
    // providers 路由两次(缓存 180s)→ 假 upstream 只被打一次。
    let (stub, _seen, hits) = zhipu_stub().await;
    let state = wired_state(&stub, Arc::new(Mutex::new(None)));
    let (base, server) = serve(state).await;
    let client = client().await;
    for _ in 0..2 {
        let _: Value = client
            .get(format!("{base}/admin/providers"))
            .bearer_auth("test-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "缓存命中只打一次");
    server.abort();
}

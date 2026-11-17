//! §11 预算基线(变更记录 31 批 1 ②):回环转发延迟守预算。
//!
//! 不是基准框架——单断言防性能无声回退;CI 可跑即可,不上
//! Criterion。口径:本地回放上游 + 串行 /v1/messages,转发
//! p99 ≤ 30ms(§11,回环环境)。先预热(连接池/ JIT 路径)再计量。

// 回放 server 供本测试只用了 Sse 一条腿,其余 helper 保留给
// integration.rs 共用(同 support 目录,单编译单元按文件裁剪)。
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;

use gateway::{app, state::GatewayState};
use manifest::schema::Manifest;
use support::replay::{spawn, Step};

/// 语料:translate 回归集 tool_call 流(与 F- 用例同源,不自造)。
const BODY: &str = include_str!("../../translate/tests/fixtures/chat_tool_call.sse");

const WARMUP: usize = 10;
const MEASURED: usize = 100;
/// §11 预算:转发 p99 上限(毫秒,回环)。
const P99_BUDGET_MS: u128 = 30;

#[tokio::test]
async fn forward_p99_within_budget() {
    let steps = vec![
        Step::Sse {
            body: BODY.to_string(),
        };
        WARMUP + MEASURED
    ];
    let (_replay, base, _upstream) = spawn(steps).await;

    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "id": "prov-perf", "version": "1",
        "provider": { "base": base, "auth": "bearer" },
        "free_models": ["glm-4.5-flash"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["127.0.0.1"], "store_key": false }
    }))
    .expect("preset json");
    let state = std::sync::Arc::new(GatewayState::from_manifests(
        BTreeMap::from([("prov-perf".to_string(), manifest)]),
        "aubridge",
    ));
    let gw_app = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, gw_app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let url = format!("http://{gw_addr}/v1/messages");
    let body = r#"{"model":"glm-4.5-flash","max_tokens":100,
        "messages":[{"role":"user","content":"weather?"}]}"#;

    let request = || {
        let client = &client;
        let url = &url;
        async move {
            let t = std::time::Instant::now();
            let resp = client
                .post(url.clone())
                .header("authorization", "Bearer aubridge")
                .body(body)
                .send()
                .await
                .expect("gateway request");
            assert_eq!(resp.status(), 200);
            let text = resp.text().await.unwrap();
            (t.elapsed().as_millis(), text)
        }
    };

    // 预热:首请求建池/建流路径不计入计量。
    for _ in 0..WARMUP {
        let (_, text) = request().await;
        assert!(text.contains("message_stop"));
    }
    let mut times = Vec::with_capacity(MEASURED);
    for _ in 0..MEASURED {
        let (ms, text) = request().await;
        assert!(text.contains("message_stop"));
        times.push(ms);
    }
    times.sort_unstable();
    // nearest-rank p99:ceil(0.99 * 100) = 99 → 0 起第 98 位。
    let p99 = times[MEASURED - 2];
    assert!(
        p99 <= P99_BUDGET_MS,
        "转发 p99 = {p99}ms 超预算 {P99_BUDGET_MS}ms(§11;样本 p50={}ms max={}ms)",
        times[MEASURED / 2],
        times[MEASURED - 1]
    );

    server.abort();
}

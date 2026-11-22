//! 订阅额度查询(设计文档 §8.2):Claude `/api/oauth/usage` 与 Codex
//! `chatgpt.com/backend-api/wham/usage` 响应解析。
//!
//! 两条路:主动查询(此模块解析 `five_hour{utilization, resets_at}` /
//! `seven_day` 窗口;Codex wham 的 `rate_limit` 双窗口)与被动采样
//! (`anthropic-ratelimit-unified-*` 头,gateway 侧)。纯函数解析可测;
//! HTTP 拉取由 gateway/registry 注入(带缓存 3min/负缓存 1min/单飞/
//! jitter 纪律,§8.2)。

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// OpenRouter's key limit is a USD budget. Free-model request counts are
/// separate and are learned from the provider's limit responses.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct OpenRouterKeyBudget {
    pub limit: Option<f64>,
    pub limit_remaining: Option<f64>,
    pub limit_reset: Option<String>,
    pub is_free_tier: Option<bool>,
}

#[derive(Deserialize)]
struct OpenRouterKeyResponse {
    data: OpenRouterKeyBudget,
}

struct CachedBudget {
    key_hash: [u8; 32],
    expires: tokio::time::Instant,
    budget: Option<OpenRouterKeyBudget>,
}

const NEGATIVE_TTL_SECS: u64 = 60;

pub struct OpenRouterKeyFetcher {
    endpoint: String,
    cache: tokio::sync::Mutex<Option<CachedBudget>>,
}

impl OpenRouterKeyFetcher {
    pub const ENDPOINT: &'static str = "https://openrouter.ai/api/v1/key";

    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            cache: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn fetch(&self, client: &reqwest::Client, key: &str) -> Option<OpenRouterKeyBudget> {
        let key_hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.as_ref().filter(|entry| {
            entry.key_hash == key_hash && tokio::time::Instant::now() < entry.expires
        }) {
            return entry.budget.clone();
        }
        let budget = self.fetch_once(client, key).await;
        let ttl = Duration::from_secs(if budget.is_some() {
            900
        } else {
            NEGATIVE_TTL_SECS
        });
        *cache = Some(CachedBudget {
            key_hash,
            expires: tokio::time::Instant::now() + ttl,
            budget: budget.clone(),
        });
        budget
    }

    async fn fetch_once(&self, client: &reqwest::Client, key: &str) -> Option<OpenRouterKeyBudget> {
        let response = client
            .get(&self.endpoint)
            .bearer_auth(key)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        Some(response.json::<OpenRouterKeyResponse>().await.ok()?.data)
    }
}

/// Claude usage API 响应。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClaudeUsage {
    #[serde(rename = "five_hour", default)]
    pub five_hour: Option<UsageWindow>,
    #[serde(rename = "seven_day", default)]
    pub seven_day: Option<UsageWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UsageWindow {
    /// 0.0 – 1.0 利用率。
    #[serde(default)]
    pub utilization: Option<f64>,
    /// ISO8601 重置时刻。
    #[serde(default, rename = "resets_at")]
    pub resets_at: String,
}

/// 被动采样:`anthropic-ratelimit-unified-5h-*` 头 → 窗口状态
/// (gateway 每响应调用,零成本覆盖 100% 请求,§8.2)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PassiveWindow {
    pub utilization: Option<f64>,
    /// 秒级 unix 或 ISO8601(上游各异,原样保留由调用方归一)。
    pub resets_at: Option<String>,
}

/// 从响应头提取 5h 窗口(头名小写化后匹配)。
pub fn sample_passive_5h(headers: &[(String, String)]) -> PassiveWindow {
    let get = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    PassiveWindow {
        utilization: get("anthropic-ratelimit-unified-5h-utilization").and_then(|v| v.parse().ok()),
        resets_at: get("anthropic-ratelimit-unified-5h-reset"),
    }
}

/// Codex/ChatGPT `/backend-api/wham/usage` 响应(sub2api
/// `openai_quota_service.go` 口径;serde 从宽——未知字段忽略,
/// 上游对无配额的槽显式回 `null` 窗口)。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CodexWhamUsage {
    #[serde(default, rename = "rate_limit")]
    pub rate_limit: Option<WhamRateLimit>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WhamRateLimit {
    /// Missing spans keep the historical primary=7d compatibility mapping.
    #[serde(default, rename = "primary_window")]
    pub primary_window: Option<WhamWindow>,
    /// Missing spans keep the historical secondary=5h compatibility mapping.
    #[serde(default, rename = "secondary_window")]
    pub secondary_window: Option<WhamWindow>,
}

/// 单窗口:used_percent 为百分比 0–100,reset_at 为 unix 秒。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WhamWindow {
    #[serde(default)]
    pub limit_window_seconds: Option<u64>,
    #[serde(default, rename = "used_percent")]
    pub used_percent: Option<f64>,
    #[serde(default, rename = "reset_at")]
    pub reset_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_usage_windows() {
        let u: ClaudeUsage = serde_json::from_str(
            r#"{"five_hour":{"utilization":0.42,"resets_at":"2026-11-03T12:00:00Z"},
                "seven_day":{"utilization":0.15,"resets_at":"2026-11-07T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(u.five_hour.unwrap().utilization, Some(0.42));
        assert_eq!(u.seven_day.unwrap().resets_at, "2026-11-07T00:00:00Z");
    }

    #[test]
    fn passive_header_sampling() {
        let headers = vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.67".into(),
            ),
            (
                "Anthropic-Ratelimit-Unified-5h-Reset".into(),
                "1730635200".into(),
            ),
        ];
        let w = sample_passive_5h(&headers);
        assert_eq!(w.utilization, Some(0.67));
        assert_eq!(w.resets_at.as_deref(), Some("1730635200"));
    }

    #[test]
    fn parses_wham_usage_windows() {
        let u: CodexWhamUsage = serde_json::from_str(
            r#"{"user_id":"u1","rate_limit":{
                "allowed":true,"limit_reached":false,
                "primary_window":{"used_percent":31.5,"limit_window_seconds":604800,
                    "reset_after_seconds":3600,"reset_at":1762300000},
                "secondary_window":{"used_percent":74.0,"limit_window_seconds":18000,
                    "reset_after_seconds":900,"reset_at":1762205400}},
                "credits":{"has_credits":false,"unlimited":false}}"#,
        )
        .unwrap();
        let rl = u.rate_limit.unwrap();
        let p = rl.primary_window.unwrap();
        let s = rl.secondary_window.unwrap();
        assert_eq!(p.used_percent, Some(31.5));
        assert_eq!(p.reset_at, Some(1762300000));
        assert_eq!(s.used_percent, Some(74.0));
    }

    #[test]
    fn wham_null_and_missing_windows_are_unknown() {
        // 上游对无配额槽显式回 null;整体缺 rate_limit 也是未知,
        // 不臆断(§8.2:查不到的源按推断处理)。
        let u: CodexWhamUsage =
            serde_json::from_str(r#"{"rate_limit":{"primary_window":null}}"#).unwrap();
        assert!(u.rate_limit.unwrap().primary_window.is_none());
        let u: CodexWhamUsage = serde_json::from_str("{}").unwrap();
        assert!(u.rate_limit.is_none());
    }
}

// ---------------------------------------------------------------------------
// HTTP 拉取(§8.2 纪律:缓存 3 分钟、负缓存 1 分钟、单飞、jitter)
// ---------------------------------------------------------------------------

use std::time::Duration;

use crate::quota::{response_text, QuotaCache, QuotaRead};
use manifest::quota::QuotaIssue;

pub struct UsageFetcher {
    endpoint: String,
    cache: QuotaCache<ClaudeUsage>,
}

impl UsageFetcher {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            cache: QuotaCache::new(),
        }
    }

    pub async fn fetch(&self, client: &reqwest::Client, token: &str) -> Option<ClaudeUsage> {
        self.read(client, token).await.value
    }

    pub async fn read(&self, client: &reqwest::Client, token: &str) -> QuotaRead<ClaudeUsage> {
        self.cache
            .read(token, || self.fetch_once(client, token))
            .await
    }

    async fn fetch_once(
        &self,
        client: &reqwest::Client,
        token: &str,
    ) -> Result<ClaudeUsage, QuotaIssue> {
        let body = response_text(client.get(&self.endpoint).bearer_auth(token)).await?;
        let usage: ClaudeUsage =
            serde_json::from_str(&body).map_err(|_| QuotaIssue::InvalidResponse)?;
        if usage
            .five_hour
            .as_ref()
            .and_then(|w| w.utilization)
            .is_none()
            && usage
                .seven_day
                .as_ref()
                .and_then(|w| w.utilization)
                .is_none()
        {
            return Err(QuotaIssue::Unsupported);
        }
        Ok(usage)
    }
}

pub struct WhamFetcher {
    endpoint: String,
    cache: QuotaCache<CodexWhamUsage>,
}

impl WhamFetcher {
    pub const ENDPOINT: &'static str = "https://chatgpt.com/backend-api/wham/usage";

    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            cache: QuotaCache::new(),
        }
    }

    pub async fn fetch(&self, client: &reqwest::Client, token: &str) -> Option<CodexWhamUsage> {
        self.read(client, token).await.value
    }

    pub async fn read(&self, client: &reqwest::Client, token: &str) -> QuotaRead<CodexWhamUsage> {
        self.cache
            .read(token, || self.fetch_once(client, token))
            .await
    }

    async fn fetch_once(
        &self,
        client: &reqwest::Client,
        token: &str,
    ) -> Result<CodexWhamUsage, QuotaIssue> {
        let body = response_text(client.get(&self.endpoint).bearer_auth(token)).await?;
        let usage: CodexWhamUsage =
            serde_json::from_str(&body).map_err(|_| QuotaIssue::InvalidResponse)?;
        if usage.rate_limit.as_ref().is_none_or(|limits| {
            limits
                .primary_window
                .as_ref()
                .and_then(|w| w.used_percent)
                .is_none()
                && limits
                    .secondary_window
                    .as_ref()
                    .and_then(|w| w.used_percent)
                    .is_none()
        }) {
            return Err(QuotaIssue::Unsupported);
        }
        Ok(usage)
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;

    #[tokio::test]
    async fn fetches_and_caches_from_test_server() {
        // 测试服务器回 usage JSON;请求计数验证缓存(只打一次)。
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let app = axum::Router::new().route(
            "/api/oauth/usage",
            axum::routing::get(move || {
                let hits = hits2.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "five_hour": {"utilization": 0.5, "resets_at": "2026-11-03T12:00:00Z"}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let fetcher = UsageFetcher::new(format!("http://{addr}/api/oauth/usage"));
        let client = reqwest::Client::new();
        let u = fetcher.fetch(&client, "tok").await.expect("usage");
        assert_eq!(u.five_hour.unwrap().utilization, Some(0.5));
        // 缓存命中:第二次不打服务器。
        fetcher.fetch(&client, "tok").await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failure_is_negative_cached() {
        let app = axum::Router::new().route(
            "/api/oauth/usage",
            axum::routing::get(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let fetcher = UsageFetcher::new(format!("http://{addr}/api/oauth/usage"));
        let client = reqwest::Client::new();
        assert!(fetcher.fetch(&client, "tok").await.is_none());
        assert!(
            fetcher.fetch(&client, "tok").await.is_none(),
            "负缓存内不再打服务器"
        );
    }

    #[tokio::test]
    async fn wham_fetches_and_caches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let app = axum::Router::new().route(
            "/backend-api/wham/usage",
            axum::routing::get(move || {
                let hits = hits2.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
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
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let fetcher = WhamFetcher::new(format!("http://{addr}/backend-api/wham/usage"));
        let client = reqwest::Client::new();
        let u = fetcher.fetch(&client, "tok").await.expect("wham");
        let rl = u.rate_limit.expect("rate_limit");
        assert_eq!(
            rl.secondary_window.unwrap().used_percent,
            Some(60.0),
            "secondary = 5h 窗口"
        );
        fetcher.fetch(&client, "tok").await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "缓存命中只打一次");
    }
}

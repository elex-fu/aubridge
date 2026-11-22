//! Credential-scoped positive/negative caching and real concurrent HTTP reads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::{routing::get, Json, Router};
use credentials::planquota::{PlanAuth, PlanFetcher};
use credentials::usage::{UsageFetcher, WhamFetcher};
use serde_json::json;

enum Fetcher {
    Plan(PlanFetcher),
    Claude(UsageFetcher),
    Wham(WhamFetcher),
}

impl Fetcher {
    fn new(kind: &str, url: &str) -> Self {
        match kind {
            "plan" => Self::Plan(PlanFetcher::new(url, PlanAuth::RawKey)),
            "claude" => Self::Claude(UsageFetcher::new(url)),
            "wham" => Self::Wham(WhamFetcher::new(url)),
            _ => unreachable!(),
        }
    }

    async fn used(&self, client: &reqwest::Client, key: &str) -> Option<f64> {
        match self {
            Self::Plan(fetcher) => fetcher.fetch(client, key).await?.w5h,
            Self::Claude(fetcher) => fetcher.fetch(client, key).await?.five_hour?.utilization,
            Self::Wham(fetcher) => {
                fetcher
                    .fetch(client, key)
                    .await?
                    .rate_limit?
                    .secondary_window?
                    .used_percent
            }
        }
    }

    async fn metadata(
        &self,
        client: &reqwest::Client,
        key: &str,
    ) -> (
        credentials::quota::ObservationStamp,
        Option<manifest::quota::QuotaIssue>,
    ) {
        match self {
            Self::Plan(fetcher) => {
                let read = fetcher.read(client, key).await;
                (read.stamp, read.issue)
            }
            Self::Claude(fetcher) => {
                let read = fetcher.read(client, key).await;
                (read.stamp, read.issue)
            }
            Self::Wham(fetcher) => {
                let read = fetcher.read(client, key).await;
                (read.stamp, read.issue)
            }
        }
    }
}

async fn stub() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/quota",
        get(move |headers: HeaderMap| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                // Keep overlapping reads in flight; the assertion concerns their
                // final call count, not a timing or performance threshold.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let key = headers["authorization"].to_str().unwrap();
                // A quota-shaped body is not a reading on an HTTP error.
                let status = if key.ends_with("missing") {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::OK
                };
                let used = if key.ends_with("key-b") { 77 } else { 11 };
                (status, Json(json!({
                    "data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,
                        "percentage":used,"nextResetTime":1800000000000_i64}]},
                    "five_hour":{"utilization":used,"resets_at":"2027-01-15T08:00:00Z"},
                    "rate_limit":{"secondary_window":{"used_percent":used,"reset_at":1800000000}}
                })))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls, server)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn all_quota_fetchers_merge_concurrent_reads() {
    for kind in ["plan", "claude", "wham"] {
        let (url, calls, server) = stub().await;
        let fetcher = Fetcher::new(kind, &url);
        let client = client();
        let (first, second) = tokio::join!(
            fetcher.used(&client, "synthetic-key-a"),
            fetcher.used(&client, "synthetic-key-a")
        );
        assert_eq!(first, Some(11.0), "{kind}");
        assert_eq!(second, Some(11.0), "{kind}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{kind}: duplicate upstream reads"
        );
        server.abort();
    }
}

#[tokio::test]
async fn all_quota_fetchers_isolate_changed_credentials() {
    for kind in ["plan", "claude", "wham"] {
        let (url, calls, server) = stub().await;
        let fetcher = Fetcher::new(kind, &url);
        let client = client();
        assert_eq!(fetcher.used(&client, "synthetic-key-a").await, Some(11.0));
        assert_eq!(
            fetcher.used(&client, "synthetic-key-b").await,
            Some(77.0),
            "{kind}: a changed credential reused the previous reading"
        );
        assert_eq!(fetcher.used(&client, "synthetic-key-b").await, Some(77.0));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{kind}");
        server.abort();
    }
}

#[tokio::test]
async fn all_quota_fetchers_isolate_and_merge_negative_reads() {
    for kind in ["plan", "claude", "wham"] {
        let (url, calls, server) = stub().await;
        let fetcher = Fetcher::new(kind, &url);
        let client = client();
        let (first, second) = tokio::join!(
            fetcher.used(&client, "synthetic-missing"),
            fetcher.used(&client, "synthetic-missing")
        );
        assert!(first.is_none() && second.is_none(), "{kind}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{kind}: duplicate failed reads"
        );
        assert_eq!(
            fetcher.used(&client, "synthetic-key-b").await,
            Some(77.0),
            "{kind}: a previous credential's failure blocked the new credential"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{kind}");
        server.abort();
    }
}

#[tokio::test]
async fn cached_reads_preserve_their_original_observation_and_failure_category() {
    for kind in ["plan", "claude", "wham"] {
        let (url, calls, server) = stub().await;
        let fetcher = Fetcher::new(kind, &url);
        let client = client();
        let first = fetcher.metadata(&client, "synthetic-key-a").await;
        assert_eq!(first.1, None);
        assert_eq!(fetcher.metadata(&client, "synthetic-key-a").await, first);
        let failed = fetcher.metadata(&client, "synthetic-missing").await;
        assert_eq!(failed.1, Some(manifest::quota::QuotaIssue::Authentication));
        assert!(failed.0.sequence > first.0.sequence);
        assert_eq!(fetcher.metadata(&client, "synthetic-missing").await, failed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
}

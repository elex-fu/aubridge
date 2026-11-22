//! Credential-scoped reads retain the original observation time and failure kind.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use manifest::quota::QuotaIssue;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationStamp {
    pub at: u64,
    pub sequence: u64,
}

pub fn observation_stamp() -> ObservationStamp {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    ObservationStamp {
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        sequence: NEXT.fetch_add(1, Ordering::Relaxed),
    }
}

/// An irreversible, namespaced local identity. Never send it upstream or in views.
pub fn subject_tag(namespace: &str, identity: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(namespace.as_bytes());
    hash.update([0]);
    hash.update(identity.as_bytes());
    format!("{:x}", hash.finalize())
}

#[derive(Debug, Clone)]
pub struct QuotaRead<T> {
    pub value: Option<T>,
    pub stamp: ObservationStamp,
    pub issue: Option<QuotaIssue>,
}

struct Cached<T> {
    identity: [u8; 32],
    expires: tokio::time::Instant,
    read: QuotaRead<T>,
}

pub(crate) struct QuotaCache<T> {
    cached: tokio::sync::Mutex<Option<Cached<T>>>,
}

impl<T: Clone> QuotaCache<T> {
    pub(crate) fn new() -> Self {
        Self {
            cached: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) async fn read<F>(&self, credential: &str, fetch: impl FnOnce() -> F) -> QuotaRead<T>
    where
        F: Future<Output = Result<T, QuotaIssue>>,
    {
        let identity: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
        let mut cached = self.cached.lock().await;
        if let Some(entry) = cached.as_ref().filter(|entry| {
            entry.identity == identity && tokio::time::Instant::now() < entry.expires
        }) {
            return entry.read.clone();
        }
        let stamp = observation_stamp();
        let read = match fetch().await {
            Ok(value) => QuotaRead {
                value: Some(value),
                stamp,
                issue: None,
            },
            Err(issue) => QuotaRead {
                value: None,
                stamp,
                issue: Some(issue),
            },
        };
        let ttl = if read.value.is_some() { 180 } else { 60 };
        *cached = Some(Cached {
            identity,
            expires: tokio::time::Instant::now() + Duration::from_secs(ttl),
            read: read.clone(),
        });
        read
    }
}

pub(crate) async fn response_text(request: reqwest::RequestBuilder) -> Result<String, QuotaIssue> {
    let issue = |error: reqwest::Error| {
        if error.is_timeout() {
            QuotaIssue::Timeout
        } else {
            QuotaIssue::Network
        }
    };
    let mut response = request
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(issue)?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err(QuotaIssue::Authentication),
        429 => return Err(QuotaIssue::RateLimited),
        _ => return Err(QuotaIssue::Upstream),
    }
    const LIMIT: usize = 256 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(QuotaIssue::InvalidResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(issue)? {
        if body.len() + chunk.len() > LIMIT {
            return Err(QuotaIssue::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| QuotaIssue::InvalidResponse)
}

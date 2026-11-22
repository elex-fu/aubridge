//! Local correlation only: body text and credentials never enter diagnostics.
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::state::GatewayState;

pub(crate) const REQUEST_ID: &str = "x-aubridge-request-id";
pub(crate) const REQUEST_PATH: &str = "x-aubridge-request-path";
pub(crate) const REQUEST_BYTES: &str = "x-aubridge-request-bytes";
pub(crate) const RECORDED: &str = "x-aubridge-recorded";

pub(crate) fn recorded(mut response: Response<Body>) -> Response<Body> {
    response
        .headers_mut()
        .insert(RECORDED, HeaderValue::from_static("1"));
    response
}

pub(crate) struct Diagnostic {
    id: String,
    header: HeaderValue,
    path: String,
    agent: String,
    identity: crate::session_identity::Identity,
    ts: u64,
    started: Instant,
    pub bytes: Option<u64>,
}

impl Diagnostic {
    pub(crate) fn new(
        state: &GatewayState,
        path: &str,
        headers: &HeaderMap,
    ) -> Result<Self, axum::http::header::InvalidHeaderValue> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let id = format!(
            "req_{:x}_{:x}_{:x}",
            now.as_nanos(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let header = HeaderValue::from_str(&id)?;
        let prefix = format!("{}-", state.token_key());
        let agent = crate::security::caller_key(headers)
            .and_then(|key| key.strip_prefix(&prefix).map(str::to_owned))
            .unwrap_or_else(|| "-".into());
        Ok(Self {
            id,
            header,
            path: path.into(),
            agent,
            identity: crate::session_identity::extract(headers, None),
            ts: now.as_secs(),
            started: Instant::now(),
            bytes: None,
        })
    }

    pub(crate) fn attach(
        &self,
        headers: &mut HeaderMap,
    ) -> Result<(), axum::http::header::InvalidHeaderValue> {
        headers.insert(REQUEST_ID, self.header.clone());
        headers.insert(REQUEST_PATH, HeaderValue::from_str(&self.path)?);
        if let Some(bytes) = self.bytes {
            headers.insert(REQUEST_BYTES, HeaderValue::from_str(&bytes.to_string())?);
        }
        Ok(())
    }

    pub(crate) fn decorate(&self, mut response: Response<Body>) -> Response<Body> {
        response
            .headers_mut()
            .insert(REQUEST_ID, self.header.clone());
        response
    }

    pub(crate) fn reject(
        &self,
        state: &GatewayState,
        response: Response<Body>,
        outcome: &str,
    ) -> Response<Body> {
        if self.path == "/v1/messages/count_tokens" || self.path.ends_with(":countTokens") {
            return self.decorate(response);
        }
        state.record_request(ledger::CompletedRequest {
            decision: ledger::DecisionRecord {
                session_id: self.identity.session.clone(),
                parent_session_id: self.identity.parent.clone(),
                purpose: self.identity.purpose.clone(),
                ts: self.ts,
                agent: self.agent.clone(),
                request_id: Some(self.id.clone()),
                request_path: Some(self.path.clone()),
                request_bytes: self.bytes,
                duration_ms: Some(self.started.elapsed().as_millis() as u64),
                outcome: outcome.into(),
                attempts: "[]".into(),
                ..Default::default()
            },
            usage: Vec::new(),
        });
        self.decorate(response)
    }
}

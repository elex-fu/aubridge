//! Ingress bounds are independent of upstream timing and SSE frame limits.
use std::io::Read;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use futures_core::Stream;
use futures_util::StreamExt;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, Semaphore};

use super::GatewayState;

pub(crate) const BODY_LIMIT: usize = 32 * 1024 * 1024;
const MEMORY_BUDGET: usize = 256 * 1024 * 1024;
const QUANTUM: usize = 64 * 1024;
const AMPLIFICATION: usize = 4;
const MAX_REQUESTS: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Admission {
    memory: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    activities: Arc<tokio::sync::RwLock<()>>,
    maintenance: Arc<AtomicBool>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            memory: Arc::new(Semaphore::new(MEMORY_BUDGET / QUANTUM)),
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
            activities: Arc::new(tokio::sync::RwLock::new(())),
            maintenance: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn charge(bytes: usize) -> usize {
    bytes.max(1).div_ceil(QUANTUM) * AMPLIFICATION
}

struct Lease {
    memory: OwnedSemaphorePermit,
    _request: OwnedSemaphorePermit,
}

/// Held across an explicit multi-step CLI job or parked subscription run.
pub struct UpdateActivityLease {
    _activity: OwnedRwLockReadGuard<()>,
}

impl UpdateActivityLease {
    pub(crate) fn acquire(gate: Arc<tokio::sync::RwLock<()>>) -> Result<Self, &'static str> {
        gate.try_read_owned()
            .map(|activity| Self {
                _activity: activity,
            })
            .map_err(|_| "update-maintenance-active")
    }
}

/// Exclusive idle admission, kept alive until helper handoff or a recoverable error.
pub struct UpdateMaintenanceLease {
    _activity: OwnedRwLockWriteGuard<()>,
    requests: Option<OwnedSemaphorePermit>,
    active: Arc<AtomicBool>,
}

impl Drop for UpdateMaintenanceLease {
    fn drop(&mut self) {
        drop(self.requests.take());
        self.active.store(false, Ordering::SeqCst);
    }
}

impl Admission {
    pub(super) fn activity_gate(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.activities.clone()
    }

    async fn acquire(&self, bytes: usize) -> Result<Lease, Issue> {
        tokio::time::timeout(ADMISSION_TIMEOUT, async {
            let request = self
                .requests
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Issue::Busy)?;
            let memory = self
                .memory
                .clone()
                .acquire_many_owned(charge(bytes) as u32)
                .await
                .map_err(|_| Issue::Busy)?;
            Ok(Lease {
                memory,
                _request: request,
            })
        })
        .await
        .map_err(|_| Issue::Busy)?
    }

    pub(super) fn has_active_inference(&self) -> bool {
        !self.maintenance.load(Ordering::SeqCst) && self.requests.available_permits() < MAX_REQUESTS
    }

    pub(super) fn view(&self) -> serde_json::Value {
        let maintenance = self.maintenance.load(Ordering::SeqCst);
        serde_json::json!({"wire_body_bytes":BODY_LIMIT,"decoded_body_bytes":BODY_LIMIT,
            "body_read_timeout_secs":READ_TIMEOUT.as_secs(),"admission_timeout_secs":ADMISSION_TIMEOUT.as_secs(),
            "max_requests":MAX_REQUESTS,"active_requests":if maintenance {0}else{MAX_REQUESTS-self.requests.available_permits()},
            "maintenance":maintenance,
            "memory_budget_bytes":MEMORY_BUDGET,"reserved_bytes":(MEMORY_BUDGET/QUANTUM-self.memory.available_permits())*QUANTUM,
            "reservation_multiplier":AMPLIFICATION,"encodings":["identity","gzip","zstd"]})
    }
}

impl GatewayState {
    /// No queued wait: a busy job keeps running and the user may retry updating later.
    pub fn begin_update_activity(&self) -> Result<UpdateActivityLease, &'static str> {
        UpdateActivityLease::acquire(self.admission.activity_gate())
    }

    pub fn try_update_maintenance(&self) -> Result<UpdateMaintenanceLease, &'static str> {
        let activity = self
            .admission
            .activities
            .clone()
            .try_write_owned()
            .map_err(|_| "update-busy")?;
        let requests = self
            .admission
            .requests
            .clone()
            .try_acquire_many_owned(MAX_REQUESTS as u32)
            .map_err(|_| "update-busy")?;
        self.admission.maintenance.store(true, Ordering::SeqCst);
        Ok(UpdateMaintenanceLease {
            _activity: activity,
            requests: Some(requests),
            active: self.admission.maintenance.clone(),
        })
    }

    pub(crate) fn request_limits(&self) -> serde_json::Value {
        self.admission.view()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
    Zstd,
}

fn encoding(headers: &HeaderMap) -> Result<Encoding, Issue> {
    let mut values = headers.get_all(header::CONTENT_ENCODING).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(Issue::UnsupportedEncoding);
    }
    match value
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| Issue::UnsupportedEncoding)?
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "identity" => Ok(Encoding::Identity),
        "gzip" | "x-gzip" => Ok(Encoding::Gzip),
        "zstd" => Ok(Encoding::Zstd),
        _ => Err(Issue::UnsupportedEncoding),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Issue {
    TooLarge,
    BadBody,
    BadEncoding,
    UnsupportedEncoding,
    Timeout,
    Busy,
    Authentication,
    Paused,
}

impl Issue {
    fn outcome(self) -> &'static str {
        match self {
            Self::TooLarge => "request-too-large",
            Self::BadBody => "invalid-body",
            Self::BadEncoding => "invalid-encoding",
            Self::UnsupportedEncoding => "unsupported-encoding",
            Self::Timeout => "body-timeout",
            Self::Busy => "gateway-busy",
            Self::Authentication => "unauthorized",
            Self::Paused => "paused",
        }
    }
    fn response(self, openai: bool, google: bool) -> Response<Body> {
        let (status, kind, code, message) = match self {
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request_too_large",
                "Encoded and decoded request bodies must each fit within 32 MiB",
            ),
            Self::BadBody => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request_body",
                "Request body could not be read",
            ),
            Self::BadEncoding => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_content_encoding",
                "Invalid compressed body or zstd window exceeds 32 MiB",
            ),
            Self::UnsupportedEncoding => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_request_error",
                "unsupported_content_encoding",
                "Send a single identity, gzip or zstd Content-Encoding",
            ),
            Self::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                "invalid_request_error",
                "request_body_timeout",
                "Request body read and decode exceeded 60 seconds",
            ),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "gateway_capacity_exceeded",
                "Gateway request capacity is busy; retry after one second",
            ),
            Self::Authentication => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "authentication_error",
                "invalid api key",
            ),
            Self::Paused => (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "gateway_paused",
                "网关已暂停",
            ),
        };
        let mut response = if google {
            crate::gemini::error_response(status, message)
        } else if openai {
            crate::chat::error_response(status, message, kind, Some(code))
        } else {
            (status,axum::Json(serde_json::json!({"type":"error","error":{"type":kind,"code":code,"message":message}}))).into_response()
        };
        if self == Self::Busy {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response.headers_mut().insert(
            header::CONNECTION,
            axum::http::HeaderValue::from_static("close"),
        );
        response
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn read_wire(body: Body) -> Result<Vec<u8>, Issue> {
    let mut source = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|_| Issue::BadBody)?;
        if chunk.len() > BODY_LIMIT.saturating_sub(bytes.len()) {
            return Err(Issue::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn decode(wire: Vec<u8>, encoding: Encoding, cancel: &AtomicBool) -> Result<Vec<u8>, Issue> {
    let mut decoder: Box<dyn Read> = match encoding {
        Encoding::Identity => return Ok(wire),
        Encoding::Gzip => Box::new(flate2::read::MultiGzDecoder::new(wire.as_slice())),
        Encoding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(wire.as_slice())
                .map_err(|_| Issue::BadEncoding)?;
            decoder.window_log_max(25).map_err(|_| Issue::BadEncoding)?;
            Box::new(decoder)
        }
    };
    let mut output = Vec::new();
    let mut chunk = [0_u8; 32 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Issue::BadBody);
        }
        let count = decoder.read(&mut chunk).map_err(|_| Issue::BadEncoding)?;
        if count == 0 {
            return Ok(output);
        }
        if count > BODY_LIMIT.saturating_sub(output.len()) {
            return Err(Issue::TooLarge);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

struct HeldStream {
    source: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    lease: Option<Arc<Lease>>,
}
impl Stream for HeldStream {
    type Item = Result<Bytes, axum::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.source.as_mut().poll_next(cx);
        if matches!(result, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            self.source = Box::pin(futures_util::stream::empty());
            self.lease.take();
        }
        result
    }
}

pub(crate) async fn limit(
    State(state): State<Arc<GatewayState>>,
    request: Request,
    next: Next,
) -> Response<Body> {
    let path = request.uri().path();
    let google = path.starts_with("/v1beta/models/");
    if request.method() != axum::http::Method::POST
        || !(google
            || matches!(
                path,
                "/v1/messages"
                    | "/v1/messages/count_tokens"
                    | "/v1/chat/completions"
                    | "/v1/responses"
                    | "/v1/responses/compact"
            ))
    {
        return next.run(request).await;
    }
    let openai = matches!(
        path,
        "/v1/chat/completions" | "/v1/responses" | "/v1/responses/compact"
    );
    if !state.is_valid_key(crate::security::caller_key(request.headers()).as_deref()) {
        return Issue::Authentication.response(openai, google);
    }
    if !state.is_serving() {
        return Issue::Paused.response(openai, google);
    }
    let Ok(mut diagnostic) = crate::diagnostics::Diagnostic::new(&state, path, request.headers())
    else {
        return crate::error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "request diagnostic unavailable",
        );
    };
    let encoding = match encoding(request.headers()) {
        Ok(encoding) => encoding,
        Err(issue) => {
            return diagnostic.reject(&state, issue.response(openai, google), issue.outcome())
        }
    };
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|size| size > BODY_LIMIT as u64) {
        return diagnostic.reject(
            &state,
            Issue::TooLarge.response(openai, google),
            Issue::TooLarge.outcome(),
        );
    }
    let reserved = if encoding == Encoding::Identity {
        declared.map_or(BODY_LIMIT, |size| size as usize)
    } else {
        BODY_LIMIT
    };
    let mut lease = match state.admission.acquire(reserved).await {
        Ok(lease) => Arc::new(lease),
        Err(issue) => {
            return diagnostic.reject(&state, issue.response(openai, google), issue.outcome())
        }
    };
    let (mut parts, body) = request.into_parts();
    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let decoded = tokio::time::timeout(READ_TIMEOUT, async {
        let wire = read_wire(body).await?;
        if declared.is_some_and(|size| size != wire.len() as u64) {
            return Err(Issue::BadBody);
        }
        if encoding == Encoding::Identity {
            return Ok(wire);
        }
        let cancelled = cancel.0.clone();
        let worker_lease = lease.clone();
        tokio::task::spawn_blocking(move || {
            let _lease = worker_lease;
            decode(wire, encoding, &cancelled)
        })
        .await
        .map_err(|_| Issue::BadEncoding)?
    })
    .await
    .unwrap_or(Err(Issue::Timeout));
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(issue) => {
            return diagnostic.reject(&state, issue.response(openai, google), issue.outcome())
        }
    };
    diagnostic.bytes = Some(decoded.len() as u64);
    if std::str::from_utf8(&decoded).is_err() {
        return diagnostic.reject(
            &state,
            Issue::BadBody.response(openai, google),
            Issue::BadBody.outcome(),
        );
    }
    if let Some(lease) = Arc::get_mut(&mut lease) {
        let excess = lease
            .memory
            .num_permits()
            .saturating_sub(charge(decoded.len()));
        drop(lease.memory.split(excess));
    }
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.remove(header::CONTENT_LENGTH);
    if diagnostic.attach(&mut parts.headers).is_err() {
        return diagnostic.reject(
            &state,
            crate::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "request diagnostic unavailable",
            ),
            "diagnostic-error",
        );
    }
    let mut response = next
        .run(Request::from_parts(parts, Body::from(decoded)))
        .await;
    let recorded = response
        .headers_mut()
        .remove(crate::diagnostics::RECORDED)
        .is_some();
    let response = if !recorded
        && (response.status().is_client_error() || response.status().is_server_error())
    {
        diagnostic.reject(&state, response, "request-rejected")
    } else {
        response
    };
    let (parts, body) = response.into_parts();
    diagnostic.decorate(Response::from_parts(
        parts,
        Body::from_stream(HeldStream {
            source: Box::pin(body.into_data_stream()),
            lease: Some(lease),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn update_maintenance_preserves_streams_and_excludes_multi_step_jobs() {
        let state = GatewayState::from_manifests(Default::default(), "test");
        let stream = state.admission.acquire(1).await.unwrap();
        assert!(state.try_update_maintenance().is_err());
        assert_eq!(
            state.admission.requests.available_permits(),
            MAX_REQUESTS - 1
        );
        drop(stream);
        let job = state.begin_update_activity().unwrap();
        assert!(state.try_update_maintenance().is_err());
        assert!(state.is_serving());
        drop(job);
        let maintenance = state.try_update_maintenance().unwrap();
        assert!(state.begin_update_activity().is_err());
        assert!(state.try_update_maintenance().is_err());
        assert_eq!(state.admission.acquire(1).await.err(), Some(Issue::Busy));
        assert!(state.is_serving());
        drop(maintenance);
        assert_eq!(state.admission.requests.available_permits(), MAX_REQUESTS);
        assert!(state.begin_update_activity().is_ok());
        assert!(state.admission.acquire(1).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn admission_deadline_and_cancellation_return_both_budgets() {
        let admission = Arc::new(Admission::default());
        let mut leases = Vec::new();
        for _ in 0..MAX_REQUESTS {
            leases.push(admission.acquire(1).await.unwrap());
        }
        assert_eq!(admission.acquire(1).await.err(), Some(Issue::Busy));
        drop(leases);
        let lease = admission.acquire(BODY_LIMIT).await.unwrap();
        assert_eq!(lease.memory.num_permits(), charge(BODY_LIMIT));
        let second = admission.acquire(BODY_LIMIT).await.unwrap();
        let mut waiting = Box::pin(admission.acquire(BODY_LIMIT));
        let polled =
            std::future::poll_fn(|cx| Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx)))
                .await;
        assert!(polled.is_pending());
        assert_eq!(admission.requests.available_permits(), MAX_REQUESTS - 3);
        drop(waiting);
        assert_eq!(admission.requests.available_permits(), MAX_REQUESTS - 2);
        drop((lease, second));
        assert_eq!(
            admission.memory.available_permits(),
            MEMORY_BUDGET / QUANTUM
        );
        assert_eq!(admission.requests.available_permits(), MAX_REQUESTS);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_has_an_absolute_read_deadline() {
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
        assert!(tokio::time::timeout(READ_TIMEOUT, read_wire(body))
            .await
            .is_err());
    }

    #[test]
    fn codecs_bound_output_validate_checksums_and_honor_cancellation() {
        use std::io::Write;
        let cancel = AtomicBool::new(false);
        let original = b"a valid decoded request";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(original).unwrap();
        let gzip = gzip.finish().unwrap();
        let zstd = zstd::stream::encode_all(original.as_slice(), 1).unwrap();
        for (encoding, wire) in [(Encoding::Gzip, gzip), (Encoding::Zstd, zstd)] {
            assert_eq!(decode(wire.clone(), encoding, &cancel).unwrap(), original);
            assert_eq!(
                decode(wire[..wire.len() - 3].to_vec(), encoding, &cancel),
                Err(Issue::BadEncoding)
            );
            cancel.store(true, Ordering::Relaxed);
            assert_eq!(decode(wire, encoding, &cancel), Err(Issue::BadBody));
            cancel.store(false, Ordering::Relaxed);
        }
        let too_large = zstd::stream::encode_all(vec![0_u8; BODY_LIMIT + 1].as_slice(), 1).unwrap();
        assert_eq!(
            decode(too_large, Encoding::Zstd, &cancel),
            Err(Issue::TooLarge)
        );
    }
}

#[cfg(test)]
mod analytics_activity_tests {
    use super::*;

    #[tokio::test]
    async fn inference_admission_blocks_comparison_but_idle_maintenance_does_not() {
        let state = GatewayState::from_manifests(Default::default(), "analytics-active")
            .with_ledger(ledger::Ledger::open(":memory:").unwrap());
        state.analytics_retention_since.store(0, Ordering::SeqCst);
        assert!(!state.admission.has_active_inference());
        let inference = state.admission.acquire(1).await.unwrap();
        let report = serde_json::to_value(
            state
                .analytics_report("2020-01-01", "2020-01-02", true)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            report["comparison"]["requests_change"]["reason"],
            "pending_settlement"
        );
        drop(inference);
        assert!(!state.admission.has_active_inference());
        let maintenance = state.try_update_maintenance().unwrap();
        assert!(!state.admission.has_active_inference());
        drop(maintenance);
    }
}

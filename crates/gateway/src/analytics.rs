//! D59: bounded, local-only analytics; never delegates to dashboard sampling.
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{rejection::QueryRejection, Query};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use crate::{error_response, security, state::GatewayState};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalyticsQuery {
    start: String,
    end: String,
    compare: Option<String>,
}

pub(crate) async fn read(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    query: Result<Query<AnalyticsQuery>, QueryRejection>,
) -> Response<Body> {
    if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(query)) = query else {
        return error(ledger::AnalyticsError::Invalid);
    };
    if !matches!(query.compare.as_deref(), None | Some("previous")) {
        return error(ledger::AnalyticsError::Invalid);
    }
    match tokio::task::spawn_blocking(move || {
        state.analytics_report(&query.start, &query.end, query.compare.is_some())
    })
    .await
    {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(failure)) => error(failure),
        Err(_) => error(ledger::AnalyticsError::Storage),
    }
}

pub(crate) fn error(failure: ledger::AnalyticsError) -> Response<Body> {
    match failure {
        ledger::AnalyticsError::Invalid => error_response(
            StatusCode::BAD_REQUEST,
            "invalid analytics interval or query",
        ),
        ledger::AnalyticsError::TooLarge => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "analytics interval exceeds row limit; choose a shorter interval",
        ),
        ledger::AnalyticsError::Storage => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "local analytics unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_have_distinct_safe_statuses() {
        assert_eq!(
            error(ledger::AnalyticsError::Invalid).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            error(ledger::AnalyticsError::TooLarge).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            error(ledger::AnalyticsError::Storage).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

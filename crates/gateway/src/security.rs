//! Loopback HTTP is reachable from browsers. Only the bundled desktop origin
//! can make cross-origin requests, and it still needs the route's credential.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, uri::Authority, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::GatewayState;

pub(crate) const DESKTOP_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// 提取调用方 key:`Authorization: Bearer <k>` 或 `x-api-key: <k>`。
pub fn caller_key(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(k) = auth.strip_prefix("Bearer ") {
            return Some(k.to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|k| k.to_string())
}

pub async fn guard(State(state): State<Arc<GatewayState>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let origin = headers.get(header::ORIGIN).cloned();
    let fetch_site = headers.get("sec-fetch-site");
    let local_host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<Authority>().ok())
        .is_some_and(|host| matches!(host.host(), "127.0.0.1" | "localhost" | "[::1]"));
    if !local_host {
        return deny(StatusCode::MISDIRECTED_REQUEST, "host not local");
    }
    if origin.as_ref().is_some_and(|value| {
        !value
            .to_str()
            .is_ok_and(|origin| state.allows_origin(origin))
    }) || (origin.is_none() && fetch_site.is_some_and(|site| site == "cross-site"))
    {
        return deny(StatusCode::FORBIDDEN, "origin not allowed");
    }

    let mut response = if req.method() == Method::OPTIONS && origin.is_some() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, DELETE"),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static("authorization, content-type, x-api-key"),
        );
        response
    } else {
        next.run(req).await
    };
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response.headers_mut().insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            header::HeaderValue::from_static("x-aubridge-provider"),
        );
        response
            .headers_mut()
            .append(header::VARY, header::HeaderValue::from_static("Origin"));
    }
    response
}

fn deny(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": "auth", "message": msg}});
    (status, axum::Json(body)).into_response()
}

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

pub async fn guard(
    State(state): State<Arc<GatewayState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let google =
        req.uri().path() == "/v1beta/models" || req.uri().path().starts_with("/v1beta/models/");
    let headers = req.headers();
    let origin = headers.get(header::ORIGIN).cloned();
    let fetch_site = headers.get("sec-fetch-site");
    let local_host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<Authority>().ok())
        .is_some_and(|host| matches!(host.host(), "127.0.0.1" | "localhost" | "[::1]"));
    if !local_host {
        return if google {
            crate::gemini::error_response(StatusCode::MISDIRECTED_REQUEST, "host not local")
        } else {
            deny(StatusCode::MISDIRECTED_REQUEST, "host not local")
        };
    }
    if origin.as_ref().is_some_and(|value| {
        !value
            .to_str()
            .is_ok_and(|origin| state.allows_origin(origin))
    }) || (origin.is_none() && fetch_site.is_some_and(|site| site == "cross-site"))
    {
        return if google {
            crate::gemini::error_response(StatusCode::FORBIDDEN, "origin not allowed")
        } else {
            deny(StatusCode::FORBIDDEN, "origin not allowed")
        };
    }

    let mut response = if req.method() == Method::OPTIONS && origin.is_some() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, DELETE"),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static(
                "authorization, content-type, x-api-key, x-goog-api-key",
            ),
        );
        response
    } else {
        if google && normalize_google(&mut req).is_err() {
            return crate::gemini::error_response(
                StatusCode::UNAUTHORIZED,
                "Missing or conflicting gateway keys",
            );
        }
        next.run(req).await
    };
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response.headers_mut().insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            header::HeaderValue::from_static("x-aubridge-provider, x-aubridge-request-id"),
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

// Normalize only Google ingress, before diagnostics and admission see credentials.
fn normalize_google(req: &mut Request) -> Result<(), ()> {
    let mut keys = Vec::new();
    for name in ["x-goog-api-key", "x-api-key", "authorization"] {
        for value in req.headers().get_all(name).iter() {
            let text = value.to_str().map_err(|_| ())?;
            let key = if name == "authorization" {
                text.strip_prefix("Bearer ").ok_or(())?
            } else {
                text
            };
            if key.is_empty() {
                return Err(());
            }
            keys.push(key.to_owned());
        }
    }
    let parsed = reqwest::Url::parse(&format!("http://localhost{}", req.uri())).map_err(|_| ())?;
    for (key, value) in parsed.query_pairs() {
        if key == "key" {
            keys.push(value.into_owned());
        }
    }
    let key = keys.first().ok_or(())?.clone();
    if keys.iter().any(|other| other != &key) {
        return Err(());
    }
    req.headers_mut().remove("x-goog-api-key");
    req.headers_mut().remove("x-api-key");
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {key}").parse().map_err(|_| ())?,
    );
    let mut clean = parsed.clone();
    clean.set_query(None);
    {
        let mut pairs = clean.query_pairs_mut();
        for (key, value) in parsed.query_pairs() {
            if key != "key" {
                pairs.append_pair(&key, &value);
            }
        }
    }
    let uri = match clean.query() {
        Some(q) if !q.is_empty() => format!("{}?{q}", req.uri().path()),
        _ => req.uri().path().to_owned(),
    };
    *req.uri_mut() = uri.parse().map_err(|_| ())?;
    Ok(())
}

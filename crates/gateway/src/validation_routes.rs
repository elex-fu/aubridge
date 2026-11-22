//! Admin-only discovery and bounded CLI file-validation job endpoints.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use crate::{agent_validation, error_response, security, state::GatewayState};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCheckRequest {
    path: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnvPreviewRequest {
    project_handle: String,
    #[serde(default = "default_env_operation")]
    operation: String,
}

fn default_env_operation() -> String {
    "add".into()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnvApplyRequest {
    preview_handle: String,
    confirmed: bool,
}

fn project_env_error(reason: &str) -> Response<Body> {
    let status = match reason {
        "project_env_invalid_operation" | "project_env_confirmation_required" => {
            StatusCode::BAD_REQUEST
        }
        "project_env_conflict"
        | "project_env_changed"
        | "project_env_preview_expired"
        | "project_env_preview_unknown"
        | "project_env_capacity"
        | "project_handle_unknown"
        | "project_handle_expired"
        | "project_handle_mismatch"
        | "project_changed"
        | "project_configuration_unreadable"
        | "project_context_unknown"
        | "project_context_conflict"
        | "update-maintenance-active" => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, reason)
}

pub(crate) async fn project_env_preview(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let request: ProjectEnvPreviewRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid project env request"),
    };
    match tokio::task::spawn_blocking(
        move || -> Result<crate::agent_project::EnvPreviewResponse, String> {
            crate::agent_project::env_preview(&state, &request.project_handle, &request.operation)
        },
    )
    .await
    {
        Ok(Ok(preview)) => Json(preview).into_response(),
        Ok(Err(reason)) => project_env_error(&reason),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "project_env_io"),
    }
}

pub(crate) async fn project_env_apply(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let request: ProjectEnvApplyRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid project env request"),
    };
    match tokio::task::spawn_blocking(move || {
        crate::agent_project::env_apply(&state, &request.preview_handle, request.confirmed)
    })
    .await
    {
        Ok(Ok(check)) => Json(check).into_response(),
        Ok(Err(reason)) => project_env_error(&reason),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "project_env_io"),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectStartRequest {
    scope: String,
    project_handle: String,
    #[serde(default)]
    allow_project_temp_file: bool,
}

pub(crate) async fn project_check(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let request: ProjectCheckRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid project check request"),
    };
    if !std::path::Path::new(&request.path).is_absolute() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_project_directory");
    }
    match tokio::task::spawn_blocking(move || {
        crate::agent_project::check(&state, &id, &request.path)
    })
    .await
    {
        Ok(Ok(check)) => Json(check).into_response(),
        Ok(Err(reason)) => error_response(
            if reason == "project_check_capacity" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            &reason,
        ),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "project_check_unavailable",
        ),
    }
}

fn authorized(state: &GatewayState, headers: &HeaderMap) -> bool {
    state.is_valid_admin_key(security::caller_key(headers).as_deref())
}

pub(crate) async fn discover(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match agent_validation::discover(&state, &id).await {
        Ok(discovery) => Json(serde_json::json!({"discovery":discovery})).into_response(),
        Err(reason) => error_response(StatusCode::BAD_REQUEST, &reason),
    }
}

pub(crate) async fn start(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let private = body.trim().is_empty()
        || serde_json::from_str::<serde_json::Value>(&body)
            .is_ok_and(|value| value.as_object().is_some_and(|object| object.is_empty()));
    let result = if private {
        agent_validation::start(state, id)
    } else {
        let request: ProjectStartRequest = match serde_json::from_str(&body) {
            Ok(request) => request,
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid project validation request",
                )
            }
        };
        if request.scope != "project" || request.project_handle.is_empty() {
            return error_response(StatusCode::BAD_REQUEST, "invalid project validation scope");
        }
        agent_validation::start_project(
            state,
            id,
            request.project_handle,
            request.allow_project_temp_file,
        )
    };
    match result {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(reason) => error_response(
            if reason == "validation_busy"
                || reason == "update-maintenance-active"
                || reason.starts_with("project_")
            {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            &reason,
        ),
    }
}

pub(crate) fn job(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    cancel: bool,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let job = if cancel {
        agent_validation::cancel(&state, &id)
    } else {
        agent_validation::get(&state, &id)
    };
    match job {
        Some(job) => Json(job).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "validation_job_not_found"),
    }
}

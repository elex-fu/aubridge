//! Local admin configuration for explicit user comparisons.
use super::*;
use manifest::model::BenchmarkMapping;

#[derive(serde::Deserialize)]
struct Edit {
    expected: Vec<BenchmarkMapping>,
    mappings: Vec<BenchmarkMapping>,
}

pub(crate) async fn read(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {
        return unauthorized();
    }
    match state.pricing_mappings() {
        Ok(mappings) => Json(
            serde_json::json!({"mappings":mappings,"currency":"USD","unit":"per-million-tokens"}),
        )
        .into_response(),
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, &e),
    }
}

pub(crate) async fn write(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {
        return unauthorized();
    }
    let edit: Edit = match serde_json::from_str(&body) {
        Ok(edit) => edit,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid pricing edit"),
    };
    match state.set_pricing_mappings(&edit.expected, edit.mappings) {
        Ok(()) => Json(serde_json::json!({"saved":true})).into_response(),
        Err(e) => error_response(
            if e.starts_with("pricing changed") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            &e,
        ),
    }
}

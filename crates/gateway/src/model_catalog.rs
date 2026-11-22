//! Read-only model IDs from a saved custom Chat connection.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use manifest::schema::{AuthKind, WireProtocol};
use serde_json::Value;

use crate::route_plan::Credential;
use crate::state::GatewayState;

const BODY_LIMIT: usize = 2 * 1024 * 1024;
const MODEL_LIMIT: usize = 1000;
const TIMEOUT: Duration = Duration::from_secs(10);

enum Issue {
    AdminRequired,
    UnknownProvider,
    UnsupportedProvider,
    MissingCredential,
    ProviderChanged,
    InvalidProviderUrl,
    AuthenticationFailed,
    UpstreamHttp,
    Network,
    Timeout,
    InvalidDirectory,
    TooLarge,
}

impl Issue {
    fn response(self) -> Response<Body> {
        let (status, code, error) = match self {
            Self::AdminRequired => (StatusCode::UNAUTHORIZED, "admin-required", "需要管理员凭据"),
            Self::UnknownProvider => (StatusCode::NOT_FOUND, "unknown-provider", "来源不存在"),
            Self::UnsupportedProvider => (
                StatusCode::BAD_REQUEST,
                "unsupported-provider",
                "仅支持已保存的自定义 Chat 来源",
            ),
            Self::MissingCredential => (
                StatusCode::BAD_REQUEST,
                "missing-credential",
                "请先保存来源凭据",
            ),
            Self::ProviderChanged => (
                StatusCode::CONFLICT,
                "provider-changed",
                "来源连接已变化，请重新读取",
            ),
            Self::InvalidProviderUrl => (
                StatusCode::BAD_REQUEST,
                "invalid-provider-url",
                "来源地址无效",
            ),
            Self::AuthenticationFailed => (
                StatusCode::BAD_GATEWAY,
                "authentication-failed",
                "来源拒绝了已保存的凭据",
            ),
            Self::UpstreamHttp => (
                StatusCode::BAD_GATEWAY,
                "upstream-http",
                "来源未返回成功的模型目录",
            ),
            Self::Network => (
                StatusCode::BAD_GATEWAY,
                "network-error",
                "无法读取来源模型目录",
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "读取来源模型目录超时",
            ),
            Self::InvalidDirectory => (
                StatusCode::BAD_GATEWAY,
                "invalid-directory",
                "来源返回了无效的模型目录",
            ),
            Self::TooLarge => (
                StatusCode::BAD_GATEWAY,
                "catalog-too-large",
                "来源模型目录超过读取上限",
            ),
        };
        (
            status,
            [("cache-control", "no-store")],
            Json(serde_json::json!({"code": code, "error": error})),
        )
            .into_response()
    }
}

pub(crate) async fn route(state: &GatewayState, headers: &HeaderMap, id: &str) -> Response<Body> {
    let key = crate::security::caller_key(headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return Issue::AdminRequired.response();
    }
    match discover(state, id).await {
        Ok(models) => (
            [("cache-control", "no-store")],
            Json(serde_json::json!({"models": models})),
        )
            .into_response(),
        Err(issue) => issue.response(),
    }
}

async fn discover(state: &GatewayState, id: &str) -> Result<Vec<String>, Issue> {
    let manifest = state.manifest(id).ok_or(Issue::UnknownProvider)?;
    if !state.is_custom_provider(id) || manifest.provider.protocol != WireProtocol::Chat {
        return Err(Issue::UnsupportedProvider);
    }
    let (credential, version) =
        state
            .dispatch_credential(&manifest)
            .map_err(|reason| match reason {
                "missing-credential" => Issue::MissingCredential,
                crate::route_plan::INVALID_PROVIDER_URL => Issue::InvalidProviderUrl,
                _ => Issue::ProviderChanged,
            })?;
    let mut request = state.http().get(format!(
        "{}/models",
        manifest.provider.base.trim_end_matches('/')
    ));
    request = match credential {
        Credential::Key(key) => match manifest.provider.auth {
            AuthKind::ClaudeCli => request,
            AuthKind::Bearer => request.bearer_auth(key),
            AuthKind::ApiKey => request.header("x-api-key", key),
        },
        Credential::Local => request,
        Credential::ClaudeCli | Credential::ChatGpt => return Err(Issue::UnsupportedProvider),
    };
    let result = tokio::time::timeout(TIMEOUT, async {
        let response = request.send().await.map_err(|_| Issue::Network)?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(Issue::AuthenticationFailed)
            }
            _ => return Err(Issue::UpstreamHttp),
        }
        let bytes = crate::stream::read_limited(response, BODY_LIMIT)
            .await
            .map_err(|reason| {
                if reason == "reply-too-large" {
                    Issue::TooLarge
                } else {
                    Issue::Network
                }
            })?;
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| Issue::InvalidDirectory)?;
        let entries = payload["data"].as_array().ok_or(Issue::InvalidDirectory)?;
        let mut models = BTreeSet::new();
        for entry in entries {
            let Some(id) = entry["id"].as_str().map(str::trim).filter(|id| {
                !id.is_empty()
                    && id.len() <= 512
                    && !id.contains(|ch: char| ch.is_control() || ch == ',')
            }) else {
                continue;
            };
            models.insert(id.to_owned());
            if models.len() > MODEL_LIMIT {
                return Err(Issue::TooLarge);
            }
        }
        if !entries.is_empty() && models.is_empty() {
            return Err(Issue::InvalidDirectory);
        }
        Ok(models.into_iter().collect())
    })
    .await
    .unwrap_or(Err(Issue::Timeout));
    let mut current = false;
    state.update_current_connection(id, Some(&version), || current = true);
    if !current {
        return Err(Issue::ProviderChanged);
    }
    result
}

//! gateway —— axum 网关(设计文档 §6/§8,M1 第一版)。
//!
//! 组装 L0/L1:安全中间件(§6 边界清单)→ 模型解析 → router 候选
//! 循环 → 上游 Chat 转发(translate 接线)→ 错误分类回写状态。
//! holdWriter 语义:首字节发给 agent 后不再换源,后续失败按 F-5
//! 补 error 终止(magpie `holdWriter`)。
//!
//! 依赖方向纪律(crate-layout §1):本 crate 是 L2 组装层,可依赖
//! 一切 L0/L1;接口不泄漏 reqwest 类型。

mod agent_project;
mod agent_test;
mod agent_validation;
mod analytics;
#[cfg(target_os = "macos")]
pub mod app_update;
#[cfg(target_os = "macos")]
pub mod app_update_helper;
#[cfg(target_os = "macos")]
pub mod app_update_package;
pub mod build_info;
mod catalog;
mod chat;
pub mod chatgpt;
mod claude_cli;
pub mod cli;
mod compaction;
mod diagnostics;
mod gemini;
mod model_catalog;
mod native_sessions;
mod openrouter;
mod pricing;
mod responses;
mod route_plan;
pub mod runtime;
pub mod security;
mod session_identity;
pub mod state;
pub(crate) use state::usage_policy;
mod stream;
pub mod upstream;
mod validation_routes;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{rejection::QueryRejection, DefaultBodyLimit, Query, RawQuery};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use manifest::schema::Manifest;
use proto::protocol::{InboundContent, InboundMessage, MessagesRequest};
use state::GatewayState;

/// 网关构建参数。
pub struct GatewayConfig {
    /// 本机 token(设计文档 §6:`aubridge`,每 agent `aubridge-<agent>`)。
    pub token: String,
    /// 允许的 Origin(空 = 一律要求 key)。
    pub origin_allowlist: Vec<String>,
}

/// "自动(aubridge)" 内置虚拟模型字面 id(ui-design §6.1,变更记录
/// 32-②):agent 请求此模型名时,网关不按模型过滤候选(全部健康
/// 启用源两梯队),按候选源把请求 model 改写成其首个声明模型。
/// 名字写死,不进配置(正确行为写死,反配置爆炸)。
pub const AUTO_MODEL: &str = "aubridge";

/// 组装网关 Router。状态由调用方持有(Arc 共享)。
pub fn app(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/admin/pricing", {
            let read = state.clone(); let write = state.clone();
            get(move |headers: HeaderMap| pricing::read(read, headers))
                .post(move |headers: HeaderMap, body: String| pricing::write(write, headers, body))
        })
        .route("/admin/analytics", {
            let state = state.clone();
            get(move |headers: HeaderMap, query: Result<Query<analytics::AnalyticsQuery>, QueryRejection>| {
                analytics::read(state, headers, query)
            })
        })
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin/build", {
            let state = state.clone();
            get(move |headers: HeaderMap| async move {
                if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {
                    return unauthorized();
                }
                Json(build_info::current()).into_response()
            })
        })
        .route("/admin/work-config", {
            let state = state.clone();
            get(move |headers: HeaderMap| {
                state::work_config_backup::route(state.clone(), headers, "status".into(), String::new())
            })
        })
        .route("/admin/work-config/{action}", {
            let state = state.clone();
            post(move |axum::extract::Path(action): axum::extract::Path<String>, headers: HeaderMap, body: String| {
                state::work_config_backup::route(state.clone(), headers, action, body)
            })
        })
        .route("/admin/native-sessions", {
            let state = state.clone();
            get(move |headers: HeaderMap| native_sessions::list(state.clone(), headers))
        })
        .route("/admin/native-sessions/resume", {
            let state = state.clone();
            post(move |headers: HeaderMap, body: String| native_sessions::resume(state.clone(), headers, body))
        })
        .route("/v1beta/models", {
            let state=state.clone();
            get(move |headers:HeaderMap| async move {gemini::models(state,headers)})
        })
        .route("/v1beta/models/{*model_action}", {
            let state=state.clone();
            post(move |axum::extract::Path(path):axum::extract::Path<String>, headers:HeaderMap, body:String| async move {
                gemini::route(state,headers,path,body).await
            })
        })
        .route("/v1/models", {
            let state = state.clone();
            get(move |headers: HeaderMap| async move { models_route(state.clone(), headers).await })
        })
        .route(
            "/v1/messages",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    messages_route(state.clone(), headers, body, false).await
                }).layer(DefaultBodyLimit::max(state::request_body::BODY_LIMIT))
            },
        )
        .route(
            "/v1/messages/count_tokens",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    messages_route(state.clone(), headers, body, true).await
                }).layer(DefaultBodyLimit::max(state::request_body::BODY_LIMIT))
            },
        )
        .route(
            "/v1/chat/completions",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    chat_completions_route(state.clone(), headers, body).await
                }).layer(DefaultBodyLimit::max(state::request_body::BODY_LIMIT))
            },
        )
        .route(
            "/v1/responses",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    responses_route(state.clone(), headers, body).await
                }).layer(DefaultBodyLimit::max(state::request_body::BODY_LIMIT))
            },
        )
        .route("/v1/responses/compact", {
            let state = state.clone();
            post(move |headers: HeaderMap, body: String| async move {
                compaction::route(state, headers, body).await
            }).layer(DefaultBodyLimit::max(state::request_body::BODY_LIMIT))
        })
        .route(
            "/admin/dashboard",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    dashboard_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/quota-history",
            {
                let state = state.clone();
                get(move |headers: HeaderMap,
                          query: Result<Query<QuotaHistoryQuery>, QueryRejection>| async move {
                    quota_history_route(state.clone(), headers, query).await
                })
            },
        )
        .route(
            "/admin/plugins",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    plugins_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/plugins/auto",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    plugin_auto_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/plugins/{id}/enable",
            {
                let state = state.clone();
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          body: String| async move {
                        plugin_enable_route(state.clone(), headers, id, body).await
                    },
                )
            },
        )
        .route(
            "/admin/settings",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    settings_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/settings",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    settings_set_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/settings/restore",
            {
                let state = state.clone();
                post(move |headers: HeaderMap| async move {
                    settings_restore_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/agents",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    agents_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/agents/{id}/disconnect-preview",
            {
                let state = state.clone();
                get(move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    agent_disconnect_preview_route(state.clone(), headers, id).await
                })
            },
        )
        .route(
            "/admin/agents/{id}/enable",
            {
                let state = state.clone();
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          body: String| async move {
                        agent_enable_route(state.clone(), headers, id, body).await
                    },
                )
            },
        )
        .route(
            "/admin/agents/{id}/model",
            {
                let state = state.clone();
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          body: String| async move {
                        agent_model_route(state.clone(), headers, id, body).await
                    },
                )
            },
        )
        .route(
            "/admin/agents/{id}/config",
            {
                let state = state.clone();
                get(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap| async move {
                        agent_config_route(state.clone(), headers, id).await
                    },
                )
            },
        )
        .route(
            "/admin/agents/{id}/config",
            {
                let state = state.clone();
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap,
                          body: String| async move {
                        agent_config_set_route(state.clone(), headers, id, body).await
                    },
                )
            },
        )
        .route(
            "/admin/agents/{id}/test",
            {
                let state = state.clone();
                post(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap| async move {
                        agent_test_route(state.clone(), headers, id).await
                    },
                )
            },
        )
        .route("/admin/agents/{id}/discovery", {
            let state = state.clone();
            post(move |axum::extract::Path(id): axum::extract::Path<String>, headers: HeaderMap| {
                validation_routes::discover(state.clone(), headers, id)
            })
        })
        .route("/admin/agents/{id}/project-check", {
            let state = state.clone();
            post(move |axum::extract::Path(id): axum::extract::Path<String>, headers: HeaderMap, body: String| {
                validation_routes::project_check(state.clone(), headers, id, body)
            })
        })
        .route("/admin/agents/gemini/project-env/preview", {
            let state = state.clone();
            post(move |headers: HeaderMap, body: String| {
                validation_routes::project_env_preview(state.clone(), headers, body)
            })
        })
        .route("/admin/agents/gemini/project-env/apply", {
            let state = state.clone();
            post(move |headers: HeaderMap, body: String| {
                validation_routes::project_env_apply(state.clone(), headers, body)
            })
        })
        .route("/admin/agents/{id}/validation-jobs", {
            let state = state.clone();
            post(move |axum::extract::Path(id): axum::extract::Path<String>, headers: HeaderMap, body: String| {
                validation_routes::start(state.clone(), headers, id, body)
            })
        })
        .route("/admin/agent-validation-jobs/{job_id}", {
            let read = state.clone();
            let cancel = state.clone();
            get(move |axum::extract::Path(id): axum::extract::Path<String>, headers: HeaderMap| async move {
                validation_routes::job(read.clone(), headers, id, false)
            })
            .delete(move |axum::extract::Path(id): axum::extract::Path<String>, headers: HeaderMap| async move {
                validation_routes::job(cancel.clone(), headers, id, true)
            })
        })
        .route(
            "/admin/providers",
            {
                let state = state.clone();
                get(move |headers: HeaderMap, RawQuery(query): RawQuery| async move {
                    providers_route(state.clone(), headers, query).await
                })
            },
        )
        .route(
            "/admin/providers",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    provider_add_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/providers/{id}",
            {
                let state = state.clone();
                delete(
                    move |axum::extract::Path(id): axum::extract::Path<String>,
                          headers: HeaderMap| async move {
                        provider_delete_route(state.clone(), headers, id).await
                    },
                )
            },
        )
        .route(
            "/admin/providers/{id}/model-catalog",
            {
                let state = state.clone();
                get(move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    model_catalog::route(&state, &headers, &id).await
                })
            },
        )
        .route(
            "/admin/providers/{id}/catalog",
            {
                let read=state.clone();
                let refresh=state.clone();
                get(move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    state::directory::route(&read,&headers,&id,false).await
                }).post(move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    state::directory::route(&refresh,&headers,&id,true).await
                })
            },
        )
        .route(
            "/admin/providers/{id}/models",
            {
                let state = state.clone();
                post(
                    move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>, body: String| async move {
                        provider_models_route(state, headers, id, body).await
                    },
                )
            },
        )
        .route("/admin/providers/{id}/catalog-selection", {
            let state=state.clone();
            post(move |headers:HeaderMap,axum::extract::Path(id):axum::extract::Path<String>,body:String| async move {
                if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {return unauthorized();}
                #[derive(serde::Deserialize)] struct Selection {models:Vec<String>}
                let Ok(selection)=serde_json::from_str::<Selection>(&body) else {return error_response(StatusCode::BAD_REQUEST,"models must be a list of strings");};
                match state.select_directory_models(&id,&selection.models) {
                    Ok(())=>sync_opencode_models(&state,serde_json::json!({"id":id,"models":selection.models})).into_response(),
                    Err(error)=>error_response(StatusCode::BAD_REQUEST,&error),
                }
            })
        })
        .route(
            "/admin/providers/{id}/pause",
            {
                let state = state.clone();
                post(
                    move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>, body: String| async move {
                        provider_pause_route(state.clone(), headers, id, body).await
                    },
                )
            },
        )
        .route(
            "/admin/providers/{id}/resume-routing",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| async move {
                    provider_resume_routing_route(state, headers, id).await
                })
            },
        )
        .route(
            "/admin/providers/{id}/check",
            {
                let state = state.clone();
                post(
                    move |headers: HeaderMap,
                          axum::extract::Path(id): axum::extract::Path<String>,
                          query: Result<Query<ProviderCheckQuery>, QueryRejection>| async move {
                        provider_check_route(state.clone(), headers, id, query).await
                    },
                )
            },
        )
        .route(
            "/admin/providers/openrouter/catalog",
            {
                let read = state.clone();
                let refresh = state.clone();
                get(move |headers: HeaderMap| async move {
                    state::directory::route(&read, &headers, "openrouter", false).await
                }).post(move |headers: HeaderMap| async move {
                    openrouter_catalog_refresh_route(refresh.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/import/cc-switch",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    ccswitch_import_get_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/import/cc-switch",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    ccswitch_import_post_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/keys",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    key_set_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/keys",
            {
                let state = state.clone();
                delete(move |headers: HeaderMap, body: String| async move {
                    key_clear_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/model-groups",
            {
                let state = state.clone();
                get(move |headers: HeaderMap| async move {
                    model_groups_route(state.clone(), headers).await
                })
            },
        )
        .route(
            "/admin/model-groups",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    model_groups_set_route(state.clone(), headers, body).await
                })
            },
        )
        .route(
            "/admin/model-groups/{name}",
            {
                let state = state.clone();
                delete(
                    move |axum::extract::Path(name): axum::extract::Path<String>,
                          headers: HeaderMap| async move {
                        model_group_delete_route(state.clone(), headers, name).await
                    },
                )
            },
        )
        .route(
            "/admin/routing/preview",
            {
                let state = state.clone();
                get(
                    move |headers: HeaderMap, query: Result<Query<RoutingPreviewQuery>, QueryRejection>| async move {
                        routing_preview_route(&state, &headers, query)
                    },
                )
            },
        )
        .route("/admin/gateway/sessions", {
            let state = state.clone();
            get(move |headers: HeaderMap, query: Result<Query<GatewayHistoryQuery>, QueryRejection>| async move {
                gateway_sessions_route(state, headers, query).await
            })
        })
        .route(
            "/admin/gateway/history",
            {
                let state = state.clone();
                delete(move |headers: HeaderMap| async move { gateway_history_clear_route(state, headers).await })
            },
        )
        .route(
            "/admin/gateway",
            {
                let state = state.clone();
                get(
                    move |headers: HeaderMap, query: Result<Query<GatewayHistoryQuery>, QueryRejection>| async move {
                        gateway_route(state.clone(), headers, query).await
                    },
                )
            },
        )
        .route(
            "/admin/gateway",
            {
                let state = state.clone();
                post(move |headers: HeaderMap, body: String| async move {
                    gateway_serving_route(state.clone(), headers, body).await
                })
            },
        )
        .layer(axum::middleware::from_fn_with_state(state.clone(), state::request_body::limit))
        .layer(axum::middleware::from_fn_with_state(state, security::guard))
}

async fn models_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return unauthorized();
    }
    let models: Vec<String> = state.models();
    let facts = state.model_facts();
    Json(serde_json::json!({
        "object": "list",
        "data": models.iter().map(|m| serde_json::json!({
            "id": m, "object": "model", "owned_by": "aubridge", "capabilities": facts.get(m)
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

async fn messages_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
    count: bool,
) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return unauthorized();
    }
    // 软开关 off(变更记录 32-④):503 显式拒绝,进程不退——admin
    // 面照常可用,可再开;不裸断连(与 F-5 同精神,网关侧口径)。
    if !state.is_serving() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "网关已暂停");
    }
    let req: MessagesRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    if count {
        upstream::count_tokens(&state, &req, &headers, &body).await
    } else {
        upstream::forward(state, &req, &headers, &body).await
    }
}

async fn chat_completions_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return chat::error_response(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid gateway key",
            "authentication_error",
            Some("invalid_api_key"),
        );
    }
    if !state.is_serving() {
        return chat::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Gateway is paused",
            "server_error",
            Some("gateway_paused"),
        );
    }
    let input = match translate::chat_input::ChatInput::parse(&body) {
        Ok(input) => input,
        Err(message) => {
            return chat::error_response(
                StatusCode::BAD_REQUEST,
                message,
                "invalid_request_error",
                None,
            )
        }
    };
    upstream::forward_chat(state, &input, &headers).await
}

async fn responses_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !state.is_valid_key(security::caller_key(&headers).as_deref()) {
        return chat::error_response(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid gateway key",
            "authentication_error",
            Some("invalid_api_key"),
        );
    }
    if !state.is_serving() {
        return chat::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Gateway is paused",
            "server_error",
            Some("gateway_paused"),
        );
    }
    let input = match translate::responses_input::ResponsesInput::parse(&body) {
        Ok(input) => input,
        Err(message) => {
            return chat::error_response(
                StatusCode::BAD_REQUEST,
                message,
                "invalid_request_error",
                Some("invalid_request"),
            )
        }
    };
    upstream::forward_responses(state, &input, &headers).await
}

/// 仪表盘(§8.3):月度省 $X + 功臣榜 + 分享卡片 + 源状态快照。
/// admin token 独立校验(§6:与推理 key 空间隔离)。
async fn dashboard_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let value = state.monthly_value();
    let monthly_savings_status = if value.is_some() {
        "available"
    } else {
        "unavailable"
    };
    let total = value.as_ref().and_then(|value| value.api_equivalent_usd);
    let top_providers=value.as_ref().map(|value|value.providers.iter().filter_map(|provider|
        provider.api_equivalent_usd.map(|amount|serde_json::json!({"provider":provider.provider,"benchmark_cost_usd":amount,
            "api_equivalent_usd":amount,"records":provider.records,"priced_records":provider.priced_records}))).collect::<Vec<_>>());
    let card=value.as_ref().and_then(|value|value.api_equivalent_usd.map(|total|format!(
        "本月 API 等值估算 ${total:.2} · 已计价 {}/{} 条用量记录\n缓存读取 {} token · 缓存未报告 {} 条\n对标口径：按已知 token 和价格估算；缓存单价缺失按普通输入估算；未扣订阅费，不代表现金节省。",
        value.priced_records,value.records,value.cache_read_tokens,value.cache_unknown_records)));
    // codex wham 主动查询(§8.2 第二数据源):查得到就并入快照
    // (缓存 3min,频繁刷新不放大外呼)。
    state.sample_wham_usage().await;
    // key plan 配额查询(quota-plan-design §3.3;同款触发语义)。
    state.sample_plan_quotas().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let providers: Vec<_> = state
        .snapshot()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "kind": p.kind,
                "models": p.models,
                "remaining": p.remaining,
                "resets_at": p.resets_at,
                "resting_until": p.rest_until,
                "resting": p.is_resting(now),
                "sunk": p.sunk,
                "healthy": p.healthy,
                "enabled": p.enabled,
                "paused": state.provider_paused(&p.id),
                "ready": state.manifest(&p.id)
                    .is_some_and(|manifest| route_plan::credential(&state, &manifest).is_some()),
                // 熔断 open 期(half-open 已过期 = false)。
                "breaker_open": p.is_breaker_open(now),
                // 窗口利用率(§8.2,百分比;null = 未知)。
                "util_5h": p.util_5h,
                "util_7d": p.util_7d,
                "quota": p.quota.view(now),
            })
        })
        .collect();
    Json(serde_json::json!({
        "monthly_savings_status": monthly_savings_status,
        "monthly_savings_usd": total,
        "value_summary": value,
        "top_providers": top_providers,
        "share_card": card,
        "providers": providers,
        // 最近决策(变更记录 31 批 2 ⑤):投影与代理页历史请求表
        // 共用(decision_row)。
        "recent_decisions": state.recent_decisions().map(|rows| rows.iter().map(decision_row).collect::<Vec<_>>()),
    }))
    .into_response()
}

/// 插件页数据(§9):扫描目录 + enabled 状态 + 校验失败清单。
async fn plugins_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Some(dir) = state.plugins_dir() else {
        return Json(serde_json::json!({
            "dir": null, "plugins": [], "invalid": {}, "auto_enable": false
        }))
        .into_response();
    };
    let scan = registry::plugins::scan_dir(dir);
    let enabled = registry::plugins::EnabledState::new(dir.join("enabled.json"));
    let plugins: Vec<_> = scan
        .plugins
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "version": m.version,
                "kind": m.kind,
                "models": m.free_models,
                "base": m.provider.base,
                "enabled": enabled.is_enabled(&m.id)
                    || (enabled.auto_enable() && !enabled.is_disabled(&m.id)),
            })
        })
        .collect();
    Json(serde_json::json!({
        "dir": dir.display().to_string(),
        "plugins": plugins,
        "invalid": scan.invalid,
        "auto_enable": enabled.auto_enable(),
    }))
    .into_response()
}

/// "自动开启新插件"独立设置(§9:默认关;开启后新装插件自动启用)。
fn sync_opencode_models(
    state: &GatewayState,
    mut receipt: serde_json::Value,
) -> Json<serde_json::Value> {
    let issue = state.repair_opencode().err().map(|error| error.to_string());
    receipt["opencode_issue"] = serde_json::json!(issue);
    Json(receipt)
}

async fn plugin_auto_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Auto {
        on: bool,
    }
    let auto: Auto = match serde_json::from_str(&body) {
        Ok(a) => a,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    let Some(dir) = state.plugins_dir().cloned() else {
        return error_response(StatusCode::BAD_REQUEST, "插件目录未配置");
    };
    match registry::plugins::EnabledState::new(dir.join("enabled.json")).set_auto_enable(auto.on) {
        Ok(()) => {
            // auto 开启时,把当前未开启的合法插件一并并入运行态
            // (不写逐个开关记录,新插件语义由 auto 标志承载)。
            let merged = if auto.on {
                state.merge_scanned_plugins().unwrap_or(0)
            } else {
                0
            };
            sync_opencode_models(
                &state,
                serde_json::json!({"auto_enable": auto.on, "merged": merged}),
            )
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        )
            .into_response(),
    }
}

/// 插件开关(§9:默认全关,显式开启才进候选)。
async fn plugin_enable_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Toggle {
        on: bool,
    }
    let toggle: Toggle = match serde_json::from_str(&body) {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.set_plugin_enabled(&id, toggle.on) {
        Ok(()) => sync_opencode_models(&state, serde_json::json!({"id": id, "enabled": toggle.on}))
            .into_response(),
        Err(errs) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": errs.join("; ")})),
        )
            .into_response(),
    }
}

/// 风险披露文案(§7:"同 token 双客户端并发使用可能触发厂商异常
/// 检测",开关旁明示)。单点常量,UI 原样展示。
const SHARE_LOGIN_RISK: &str = "开启后 aubridge 会把刷新得到的 token 写回 CLI 凭据文件;\
同 token 双客户端并发使用可能触发厂商异常检测。";

/// 设置页数据(§7 写回 opt-in):开关状态 + stash 备份元信息 +
/// 风险披露。
async fn settings_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let stash = state.stash_meta().map(|m| {
        serde_json::json!({
            "mtime": m.mtime,
            "sha256": m.sha256,
            "backed_up_at": m.backed_up_at,
        })
    });
    let login = state.chatgpt_login_metadata();
    let cost_policy = state.cost_policy();
    Json(serde_json::json!({
        "share_login": state.share_login(),
        "cost_intent": cost_policy.cost_intent,
        "cost_policy_issue": cost_policy.configuration_issue,
        "subscription_authorizations": cost_policy.subscription_authorizations,
        "chatgpt_login_source": login.source,
        "chatgpt_login_status": login.status,
        "chatgpt_login_path": login.path,
        "wired": state.settings_wired(),
        "stash": stash,
        "risk": SHARE_LOGIN_RISK,
    }))
    .into_response()
}

/// 切换"与 CLI 共享登录"(§7):开启时先 stash 备份,失败返回错误
/// 且开关不动(不开无备份的写回窗口)。
async fn settings_set_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "bad settings request"),
    };
    if req.get("cost_intent").is_some() || req.get("subscription_authorizations").is_some() {
        if req
            .get("cost_intent")
            .is_none_or(serde_json::Value::is_null)
            || req.as_object().is_none_or(|object| {
                object
                    .keys()
                    .any(|key| key != "cost_intent" && key != "subscription_authorizations")
            })
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "choose one explicit cost policy operation",
            );
        }
        let policy: usage_policy::UsagePolicy = match serde_json::from_value(req) {
            Ok(policy) => policy,
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid cost policy"),
        };
        return match state.set_cost_policy(policy) {
            Ok(()) => Json(state.cost_policy()).into_response(),
            Err(error) => error_response(StatusCode::BAD_REQUEST, &error),
        };
    }
    let operation = match (req.get("share_login"), req.get("chatgpt_login_source")) {
        (Some(share), None) => match share.as_bool() {
            Some(on) => (
                state.set_share_login(on),
                serde_json::json!({"share_login": on}),
            ),
            None => return error_response(StatusCode::BAD_REQUEST, "share_login must be boolean"),
        },
        (None, Some(source)) => {
            let Some(source) = source.as_str().and_then(chatgpt::LoginSource::parse) else {
                return error_response(StatusCode::BAD_REQUEST, "invalid ChatGPT login source");
            };
            (
                state.set_chatgpt_login_source(source),
                serde_json::json!({"chatgpt_login_source": source.as_str()}),
            )
        }
        _ => return error_response(StatusCode::BAD_REQUEST, "choose one settings operation"),
    };
    match operation.0 {
        Ok(()) => Json(operation.1).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// 一键还原(§7):stash 备份原子写回 auth.json,内存凭据缓存失效
/// 下次重读盘。不动开关状态。
async fn settings_restore_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.restore_stash() {
        Ok(()) => Json(serde_json::json!({"restored": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

fn unauthorized() -> Response<Body> {
    error_response(StatusCode::UNAUTHORIZED, "missing or invalid aubridge key")
}

/// agents 页数据(§8.6,变更记录 30):注入路径 → 注册表全量状态;
/// 未注入 → 空表 + configured:false(smoke 自检壳/测试默认态,
/// 与设置页"未配置"同口径)。
async fn agents_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let agents = state.agents_overview().unwrap_or_default();
    Json(serde_json::json!({
        "configured": state.agents_configured(),
        "agents": agents,
        // 模型下拉数据源(编辑弹层):自动别名置首 + 网关聚合模型。
        "models": state.models(),
    }))
    .into_response()
}

/// agent 接线开关(变更记录 30):on = 接线(幂等),off = 断开
/// (按 stash 原样还原)。失败 400 带原因,UI setErr 不回滚
/// (与插件页同模式,数据重载揭示真实状态)。
async fn agent_enable_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Toggle {
        on: bool,
        preview: Option<agents::DisconnectPreview>,
    }
    let toggle: Toggle = match serde_json::from_str(&body) {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.set_agent_wired(&id, toggle.on, toggle.preview.as_ref()) {
        Ok(()) => Json(serde_json::json!({"id": id, "wired": toggle.on})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn agent_disconnect_preview_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.agent_disconnect_preview(&id) {
        Ok(preview) => Json(preview).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":error})),
        )
            .into_response(),
    }
}

/// 设置 agent 使用的模型(变更记录 32-②):写该 agent 配置文件的
/// 模型字段(Claude Code settings.json / codex config.toml;opencode
/// 在自身 UI 选)。模型合法性 = 自动别名或网关已知模型。
async fn agent_model_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Set {
        model: String,
    }
    let req: Set = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.set_agent_model(&id, &req.model) {
        Ok(()) => Json(serde_json::json!({"id": id, "model": req.model})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// Agent 高级配置(Claude Code settings.json / Codex profile)。
/// Admin-only;密钥在读取时打码,保存用 revision 防止旧快照覆盖。
async fn agent_config_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.agent_config(&id) {
        Ok(config) => Json(config).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn agent_config_set_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    if body.len() > 1_048_576 {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "配置内容过大");
    }
    match state.set_agent_config(&id, &body) {
        Ok(config) => Json(config).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

/// 固定命令的 Agent 连通性测试(不接受前端参数)。
async fn agent_test_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match agent_test::run(&state, &id).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

/// 模型页分组表(ui-design §6.3,变更记录 32-③):未接线 → 400
/// 与其他设置路由同口径;自动虚拟模型由 UI 内置渲染不在表里。
async fn model_groups_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.model_groups_overview() {
        Some(groups) => Json(serde_json::json!({ "groups": groups })).into_response(),
        None => error_response(StatusCode::BAD_REQUEST, "设置存储未接线"),
    }
}

/// 添加/更新分组(upsert by name,变更记录 32-③):校验失败 400
/// 带原因;成功回分组本体(成员/规则即存即生效,免重启)。
async fn model_groups_set_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let g: credentials::settings::ModelGroup = match serde_json::from_str(&body) {
        Ok(g) => g,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.upsert_model_group(&g) {
        Ok(()) => sync_opencode_models(&state, serde_json::json!(g)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// 删除分组(变更记录 32-③):不存在/未接线 400。
async fn model_group_delete_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    name: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.remove_model_group(&name) {
        Ok(()) => {
            sync_opencode_models(&state, serde_json::json!({"deleted": name})).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RoutingPreviewQuery {
    model: String,
    #[serde(default)]
    images: bool,
    #[serde(default)]
    tools: bool,
}

fn routing_preview_route(
    state: &GatewayState,
    headers: &HeaderMap,
    query: Result<Query<RoutingPreviewQuery>, QueryRejection>,
) -> Response<Body> {
    let key = security::caller_key(headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(RoutingPreviewQuery {
        model,
        tools,
        images,
    })) = query
    else {
        return error_response(StatusCode::BAD_REQUEST, "invalid routing preview query");
    };
    let request = MessagesRequest {
        model,
        messages: vec![InboundMessage {
            role: "user".into(),
            content: if images {
                InboundContent::Blocks(vec![proto::protocol::InboundBlock::Image {
                    source: proto::protocol::MediaSource::Url {
                        url: "https://example.invalid/preview.png".into(),
                    },
                }])
            } else {
                InboundContent::Text("Reply with OK.".into())
            },
        }],
        system: None,
        max_tokens: 64,
        tools: None,
        tool_choice: None,
        stream: None,
        metadata: None,
        thinking: None,
        output_config: None,
    };
    // Ignore session headers and leave the current round-robin turn available.
    let plan = route_plan::plan(
        state,
        &request,
        &HeaderMap::new(),
        upstream::unix_now(),
        false,
        tools,
        |manifest, model| {
            let wire = match manifest.provider.protocol {
                manifest::schema::WireProtocol::Anthropic => translate::media::Wire::Messages,
                manifest::schema::WireProtocol::Chat => translate::media::Wire::Chat,
                manifest::schema::WireProtocol::Responses => translate::media::Wire::Responses,
            };
            if let Err(reason) =
                translate::media::check(&request, wire, wire == translate::media::Wire::Messages)
            {
                return Some(route_plan::Block::new(reason, None));
            }
            (openrouter::applies(manifest) && !catalog::free_id(model))
                .then(|| route_plan::Block::new(openrouter::RESTRICTED, None))
        },
    );
    if !plan.saved_group {
        return error_response(StatusCode::NOT_FOUND, "saved model group not found");
    }
    let candidates: Vec<_> = plan
        .ordered
        .into_iter()
        .map(|candidate| {
            serde_json::json!({
                "id": candidate.manifest.id,
                "model": candidate.model,
                "kind": candidate.manifest.kind,
            })
        })
        .collect();
    (
        [("cache-control", "no-store")],
        Json(serde_json::json!({
            "model": request.model,
            "serving": state.is_serving(),
            "candidates": candidates,
            "skipped": plan.skipped,
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct GatewayHistoryQuery {
    start: Option<String>,
    end: Option<String>,
    session_id: Option<String>,
    request_id: Option<String>,
    agent: Option<String>,
    provider: Option<String>,
    related_provider: Option<String>,
    outcome: Option<String>,
    kind: Option<String>,
    model: Option<String>,
    before: Option<String>,
}

fn history_cursor(value: &str) -> Option<(u64, i64)> {
    let (ts, rowid) = value.split_once(':')?;
    let ts = u64::try_from(ts.parse::<i64>().ok()?).ok()?;
    let rowid = rowid.parse::<i64>().ok()?;
    (rowid > 0).then_some((ts, rowid))
}

async fn gateway_sessions_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    query: Result<Query<GatewayHistoryQuery>, QueryRejection>,
) -> Response<Body> {
    if !state.is_valid_admin_key(security::caller_key(&headers).as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "invalid session query");
    };
    let before = match query.before.as_deref() {
        Some(value) => match history_cursor(value) {
            Some(cursor) => Some(cursor),
            None => return error_response(StatusCode::BAD_REQUEST, "invalid session cursor"),
        },
        None => None,
    };
    match state.session_page(query.agent.as_deref(),query.session_id.as_deref(),before) {
        Some((sessions,next))=>Json(serde_json::json!({"history_status":"available","sessions":sessions,"next":next.map(|(ts,rowid)|format!("{ts}:{rowid}"))})).into_response(),
        None=>error_response(StatusCode::SERVICE_UNAVAILABLE,"session history unavailable"),
    }
}

/// 代理页的软开关、回环接入方式及本地历史请求。
async fn gateway_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    query: Result<Query<GatewayHistoryQuery>, QueryRejection>,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "invalid history query");
    };
    let outcome = query.outcome.as_deref();
    if !matches!(outcome, None | Some("ok" | "failed" | "cancelled")) {
        return error_response(StatusCode::BAD_REQUEST, "invalid history outcome");
    }
    let kind = query.kind.as_deref();
    if !matches!(kind, None | Some("free_tier" | "subscription")) {
        return error_response(StatusCode::BAD_REQUEST, "invalid history kind");
    }
    let before = if let Some(value) = &query.before {
        let Some(cursor) = history_cursor(value) else {
            return error_response(StatusCode::BAD_REQUEST, "invalid history cursor");
        };
        Some(cursor)
    } else {
        None
    };
    let ranged = match (&query.start, &query.end) {
        (None, None) => false,
        (Some(_), Some(_)) => true,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "start and end must be supplied together",
            )
        }
    };
    let page_state = state.clone();
    let read_page = move || {
        let outcome = query.outcome.as_deref();
        let kind = query.kind.as_deref();
        let filter = ledger::DecisionFilter {
            request_id: query.request_id.as_deref(),
            session_id: query.session_id.as_deref(),
            agent: query
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            provider: query
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            related_provider: query
                .related_provider
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            outcome,
            kind,
            model: query
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            before,
        };
        if ranged {
            page_state.analytics_decision_page(
                query.start.as_deref().unwrap_or_default(),
                query.end.as_deref().unwrap_or_default(),
                filter,
            )
        } else {
            page_state
                .decision_page(filter)
                .ok_or(ledger::AnalyticsError::Storage)
        }
    };
    let page = if ranged {
        match tokio::task::spawn_blocking(read_page).await {
            Ok(Ok(page)) => Some(page),
            Ok(Err(error)) => return analytics::error(error),
            Err(_) => return analytics::error(ledger::AnalyticsError::Storage),
        }
    } else {
        read_page().ok()
    };
    let (history_status, requests, next) = match page {
        Some(page) => (
            "available",
            Some(page.rows.iter().map(decision_row).collect::<Vec<_>>()),
            page.next.map(|(ts, rowid)| format!("{ts}:{rowid}")),
        ),
        None => ("unavailable", None, None),
    };
    Json(serde_json::json!({
        "serving": state.is_serving(),
        "endpoint": format!("http://127.0.0.1:{}", state.port()),
        "admin_token": state.admin_token(),
        "history_status": history_status,
        "request_limits": state.request_limits(),
        "requests": requests,
        "next": next,
    }))
    .into_response()
}

async fn gateway_history_clear_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.clear_request_history() {
        Ok(deleted) => Json(serde_json::json!({"deleted":deleted})).into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// 软开关切换(变更记录 32-④):off 时推理路径 503,进程不退;
/// 不持久化(重启回默认开)。
async fn gateway_serving_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Set {
        serving: bool,
    }
    let req: Set = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    state.set_serving(req.serving);
    Json(serde_json::json!({"serving": req.serving})).into_response()
}

/// 历史请求行投影(仪表盘与代理页共用):attempts 落库为紧凑 JSON
/// 字符串,这里解析回数组给 UI(坏行容错为原样字符串)。
fn decision_row(d: &ledger::DecisionRecord) -> serde_json::Value {
    let usage = ledger::UsageRecord {
        model: d.served_model.as_ref().unwrap_or(&d.model).clone(),
        input_tokens: d.input_tokens,
        output_tokens: d.output_tokens,
        cache_read_tokens: d.cache_read_tokens,
        cache_write_tokens: d.cache_write_tokens,
        input_includes_cache: d.input_includes_cache,
        provider: d.provider.clone(),
        price_snapshot: d.price_snapshot.clone(),
        benchmark_snapshot: d.benchmark_snapshot.clone(),
        accounting: if d.outcome == "ok" {
            ledger::Accounting::Reported
        } else {
            ledger::Accounting::Partial
        },
        ..Default::default()
    };
    serde_json::json!({
        "ts": d.ts,
        "model": d.model,
        "agent": d.agent,
        "provider": d.provider,
        "served_model": d.served_model,
        "input_tokens": d.input_tokens,
        "output_tokens": d.output_tokens,
        "first_content_ms": d.first_content_ms,
        "duration_ms": d.duration_ms,
        "attempts": serde_json::from_str::<serde_json::Value>(&d.attempts)
            .unwrap_or(serde_json::Value::String(d.attempts.clone())),
        "skipped": d.skipped.as_deref().and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .unwrap_or_else(|| serde_json::json!([])),
        "outcome": d.outcome,
        "session_id":d.session_id,"parent_session_id":d.parent_session_id,"purpose":d.purpose,
        "request_id":d.request_id,"request_path":d.request_path,"request_bytes":d.request_bytes,
        "cache_read_tokens":d.cache_read_tokens,"cache_write_tokens":d.cache_write_tokens,
        "input_includes_cache":d.input_includes_cache,"price_snapshot":d.price_snapshot,"estimate":usage.estimate(),
        "benchmark_snapshot":d.benchmark_snapshot,"benchmark_estimate":usage.benchmark_estimate(),
    })
}

/// Provider 页数据(变更记录 32-①):源行清单(只回 key 状态和来源)。
async fn providers_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    query: Option<String>,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    // key plan 配额查询(quota-plan-design §3.3;wham 同构触发,
    // 缓存 180s 保证频繁刷新不放大外呼)。
    let sample = !query
        .as_deref()
        .is_some_and(|query| query.split('&').any(|field| field == "sample=0"));
    let budget = if sample {
        state.sample_plan_quotas().await;
        state.openrouter_key_budget().await
    } else {
        // No cached-only key-budget accessor exists; absence stays unknown, never triggers HTTP.
        None
    };
    Json(serde_json::json!({
        "providers": state.providers_overview(),
        "openrouter_key_budget": budget,
        "openrouter_catalog": state.openrouter_catalog(),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct QuotaHistoryQuery {
    provider: Option<String>,
    days: Option<String>,
}

/// 配额窗口历史(Provider 页曲线,quota-plan-design §3.4):
/// `?provider=&days=`,days 缺省 7、非法/超 45 → 45(保留上限口径)。
async fn quota_history_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    query: Result<Query<QuotaHistoryQuery>, QueryRejection>,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "invalid quota query");
    };
    let days = query
        .days
        .as_deref()
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| *d > 0.0 && *d <= 45.0)
        .unwrap_or(7.0);
    let since = crate::upstream::unix_now().saturating_sub((days * 86_400.0) as u64);
    let provider = query
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // 按 (provider, window) 分组(升序),窗口内 at 升序——UI 直接
    // 画线,不再二次分组。
    let mut grouped: BTreeMap<(String, String, bool), Vec<serde_json::Value>> = BTreeMap::new();
    let points = match state.quota_history_result(provider, since) {
        Ok(points) => points,
        Err(message) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, message),
    };
    for p in points {
        grouped
            .entry((p.provider, p.window, p.legacy))
            .or_default()
            .push(serde_json::json!({
                "at": p.at,
                "left": p.left,
                "resets_at": p.resets_at,
            }));
    }
    let items: Vec<serde_json::Value> = grouped
        .into_iter()
        .map(|((provider, window, legacy), points)| {
            serde_json::json!({ "provider": provider, "window": window, "legacy":legacy, "points": points })
        })
        .collect();
    Json(serde_json::json!({ "items": items })).into_response()
}

async fn openrouter_catalog_refresh_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.refresh_openrouter_catalog().await {
        Ok(view) => sync_opencode_models(&state, serde_json::json!(view)).into_response(),
        Err(catalog::RefreshError::Unavailable) => {
            error_response(StatusCode::NOT_FOUND, "OpenRouter 目录未配置")
        }
        Err(catalog::RefreshError::Busy) => {
            error_response(StatusCode::CONFLICT, "OpenRouter 目录正在刷新")
        }
    }
}

#[derive(serde::Deserialize)]
struct ProviderCheckQuery {
    model: Option<String>,
}

async fn provider_check_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    query: Result<Query<ProviderCheckQuery>, QueryRejection>,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "invalid check query");
    };
    let _activity = match state.begin_update_activity() {
        Ok(activity) => activity,
        Err(reason) => return error_response(StatusCode::CONFLICT, reason),
    };
    match upstream::check_provider(&state, &id, query.model.as_deref()).await {
        Some(result) => Json(result).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "unknown provider"),
    }
}

async fn provider_pause_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Pause {
        paused: bool,
    }
    let Ok(request) = serde_json::from_str::<Pause>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "paused must be a boolean");
    };
    match state.set_provider_paused(&id, request.paused) {
        Ok(()) => {
            if id == crate::claude_cli::ID && !request.paused {
                state.refresh_claude_cli().await;
            }
            Json(serde_json::json!({"paused": request.paused})).into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, &error),
    }
}

async fn provider_resume_routing_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.resume_provider_routing(&id) {
        Ok(true) => Json(serde_json::json!({"routing_disabled":false})).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown provider"),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// 添加自定义源(变更记录 32-①):CustomProvider 字段 + 可选 key
/// (ui-design §6.2 卡片流:base 和 key 同一步粘贴)。响应绝不回显
/// key,只回 id。
async fn provider_add_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Add {
        #[serde(flatten)]
        provider: credentials::settings::CustomProvider,
        #[serde(default)]
        key: Option<String>,
    }
    let req: Add = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.add_custom_provider(&req.provider) {
        Ok(()) => {
            // key 与源同一步保存;源已落库、key 失败时如实分开报。
            if let Some(k) = req.key.as_deref().filter(|k| !k.is_empty()) {
                if let Err(e) = state.set_provider_key(&req.provider.id, k) {
                    return (
                        StatusCode::BAD_REQUEST,
                        sync_opencode_models(
                            &state,
                            serde_json::json!({
                                "error": format!("源已添加,但 key 保存失败: {e}")
                            }),
                        ),
                    )
                        .into_response();
                }
            }
            sync_opencode_models(
                &state,
                serde_json::json!({"id": req.provider.id, "created": true}),
            )
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn provider_models_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Edit {
        models: Vec<String>,
        kind: Option<manifest::schema::ProviderKind>,
        base: Option<String>,
    }
    let Ok(request) = serde_json::from_str::<Edit>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "models must be a list of strings, kind must be free_tier or subscription, and base must be a string if provided",
        );
    };
    match state.edit_custom_provider(&id, &request.models, request.kind, request.base.as_deref()) {
        Ok(models) => sync_opencode_models(&state, serde_json::json!({"id":id,"models":models}))
            .into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, &error),
    }
}

/// 删除自定义源(preset/插件不可经此删,错误 400)。
async fn provider_delete_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    id: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match state.remove_custom_provider(&id) {
        Ok(()) => sync_opencode_models(&state, serde_json::json!({"id": id, "deleted": true}))
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// cc-switch 导入候选列表(cc-switch-import-design §4.3):
/// 用户点开才读(不自动发现),key 永不回显(与 provider key 红线
/// 同口径)。db 未接线/缺失 → 空列表。
async fn ccswitch_import_get_route(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let items = match state.cc_switch_db() {
        Some(db) => match credentials::ccswitch::read_candidates(db) {
            Ok(items) => items,
            Err(error) => {
                return error_response(StatusCode::BAD_REQUEST, &format!("{error}"));
            }
        },
        None => Vec::new(),
    };
    let items: Vec<serde_json::Value> = items
        .into_iter()
        .map(|candidate| {
            let mut row = serde_json::json!({
                "reference": candidate.reference,
                "name": candidate.name,
                "source": candidate.source,
                "skip": candidate.skip,
            });
            if let Some(provider) = candidate.provider {
                row["provider"] = serde_json::json!({
                    "id": provider.id,
                    "base": provider.base,
                    "protocol": provider.protocol,
                    "auth": provider.auth,
                    "kind": provider.kind,
                    "models": provider.models,
                });
            }
            row
        })
        .collect();
    Json(serde_json::json!({ "items": items })).into_response()
}

/// 勾选即新增(D1):对每个 ref 重读 db 重解析(D7:不信任 GET 快照),
/// 逐行 `add_custom_provider` + `set_provider_key`。顺序:先加源再写
/// key——key 失败时源以 missing-credential 状态可见,UI KeyForm 可补救。
async fn ccswitch_import_post_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Import {
        refs: Vec<String>,
    }
    let req: Import = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    let items = match state.cc_switch_db() {
        Some(db) => match credentials::ccswitch::read_candidates(db) {
            Ok(items) => items,
            Err(error) => {
                return error_response(StatusCode::BAD_REQUEST, &format!("{error}"));
            }
        },
        None => Vec::new(),
    };
    let mut added: Vec<String> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();
    for reference in req.refs {
        let Some(candidate) = items.iter().find(|c| c.reference == reference) else {
            failed.push(serde_json::json!({
                "reference": reference,
                "error": "未找到该配置行(cc-switch 配置可能已修改),请刷新后重选",
            }));
            continue;
        };
        let Some(provider) = &candidate.provider else {
            failed.push(serde_json::json!({
                "reference": reference,
                "error": candidate.skip.clone().unwrap_or_else(|| "不可导入".into()),
            }));
            continue;
        };
        let custom = credentials::settings::CustomProvider {
            id: provider.id.clone(),
            base: provider.base.clone(),
            models: provider.models.clone(),
            auth: provider.auth.clone(),
            protocol: provider.protocol.clone(),
            kind: provider.kind.clone(),
        };
        if let Err(error) = state.add_custom_provider(&custom) {
            failed.push(serde_json::json!({
                "reference": reference,
                "error": error,
            }));
            continue;
        }
        if let Err(error) = state.set_provider_key(&provider.id, &provider.key) {
            // 源已加入,key 失败可经 Provider 页 KeyForm 补救(设计
            // §4.3 顺序决策),按 failed 报告但写明状态。
            failed.push(serde_json::json!({
                "reference": reference,
                "error": format!("源 {} 已加入,但 key 写入失败: {error}", provider.id),
            }));
            continue;
        }
        added.push(provider.id.clone());
    }
    Json(serde_json::json!({ "added": added, "failed": failed })).into_response()
}

/// 保存源 key(变更记录 32-①):keyring + 缓存双写,保存即生效。
/// 响应只回布尔回执。
async fn key_set_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Set {
        id: String,
        key: String,
    }
    let req: Set = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.set_provider_key(&req.id, &req.key) {
        Ok(()) => Json(serde_json::json!({"id": req.id, "set": true})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// 清除源 key(幂等)。
async fn key_clear_route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    let key = security::caller_key(&headers);
    if !state.is_valid_admin_key(key.as_deref()) {
        return error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    #[derive(serde::Deserialize)]
    struct Clear {
        id: String,
    }
    let req: Clear = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad request: {e}")),
    };
    match state.clear_provider_key(&req.id) {
        Ok(()) => Json(serde_json::json!({"id": req.id, "set": false})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response<Body> {
    let body = serde_json::json!({"type": "error", "error": {"type": "auth", "message": msg}});
    (status, Json(body)).into_response()
}

/// 启动网关(绑死回环,§6;tokio 由调用方提供)。
/// 端口回退(§4):首选端口被占用时交还 OS 随机分配(port 0);
/// 绑定成功后先回调 `on_bound`(传实际端口,调用方持久化/同步 wiring),
/// 再进入服务循环。挂了 ledger 时起周期 flush 任务(§8.1/§8.4
/// write-behind:5s 合并落盘,脏集为空时是空操作)。
pub async fn serve(
    state: Arc<GatewayState>,
    port: u16,
    on_bound: impl FnOnce(u16),
) -> std::io::Result<()> {
    let listener = bind_listener(&state, port).await?;
    on_bound(state.port());
    serve_listener(state, listener).await
}

pub(crate) async fn bind_listener(
    state: &GatewayState,
    port: u16,
) -> std::io::Result<tokio::net::TcpListener> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("aubridge: 端口 {port} 被占用,回退随机端口(§4)");
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
        }
        Err(e) => return Err(e),
    };
    let actual = listener.local_addr()?.port();
    // 端口回写(变更记录 30 ②):接线 base_url 用实际端口
    // (§4 回退时不能写 3427 死值)。
    state.set_port(actual);
    Ok(listener)
}

pub(crate) async fn serve_listener(
    state: Arc<GatewayState>,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    let app = app(state.clone());
    serve_with_ledger_flush(&state, axum::serve(listener, app)).await
}

pub(crate) async fn serve_with_ledger_flush(
    state: &GatewayState,
    server: impl std::future::IntoFuture<Output = std::io::Result<()>>,
) -> std::io::Result<()> {
    tokio::select! {
        res = server => res,
        () = async {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(ROUTER_FLUSH_SECS));
            loop {
                tick.tick().await;
                state.flush_router_state();
            }
        } => Ok(()),
        () = async {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(state::quota::POLL_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                state.sample_quotas().await;
            }
        } => Ok(()),
    }
}

/// write-behind flush 周期(§8.1/§8.4;本地 SQLite,5s 足够——
/// 丢失窗口内的状态重启后靠被动采样/重新探测恢复)。
const ROUTER_FLUSH_SECS: u64 = 5;

/// 便捷构造:内置 preset + 指定本机 token。
pub fn default_state(token: &str) -> Arc<GatewayState> {
    let presets = registry::presets();
    let manifests: BTreeMap<String, Manifest> =
        presets.iter().map(|m| (m.id.clone(), m.clone())).collect();
    Arc::new(GatewayState::from_manifests(manifests, token))
}

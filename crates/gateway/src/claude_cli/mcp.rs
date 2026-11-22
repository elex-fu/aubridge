//! Each run exposes exactly the caller's tools over authenticated loopback MCP.
use super::run::{private_file, random, Run};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::Arc};
#[derive(Clone)]
struct Mcp {
    run: Arc<Run>,
    token: String,
    tools: BTreeMap<String, (String, Value)>,
}
pub(super) struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(super) async fn start(
    run: Arc<Run>,
    dir: &Path,
    tools: BTreeMap<String, (String, Value)>,
) -> Result<Server, &'static str> {
    let token = random()?;
    let path = format!("/{}", random()?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "claude-mcp-bind")?;
    let address = listener.local_addr().map_err(|_| "claude-mcp-bind")?;
    let config = json!({"mcpServers":{"aubridge":{"type":"http", "url":format!("http://{address}{path}"), "headers":{"Authorization":format!("Bearer {token}")}}}});
    private_file(&dir.join("mcp.json"), config.to_string().as_bytes())?;
    let app = Router::new()
        .route(&path, post(rpc))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(Mcp { run, token, tools });
    Ok(Server(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })))
}
async fn rpc(State(state): State<Mcp>, headers: HeaderMap, Json(request): Json<Value>) -> Response {
    if state.run.finished()
        || headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {}", state.token))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match request["method"].as_str() {
        Some("initialize") => Ok(
            json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{}}, "serverInfo":{"name":"aubridge", "version":"1"}}),
        ),
        Some("ping") => Ok(json!({})),
        Some("tools/list") => {
            Ok(json!({"tools":state.tools.values().map(|(_, t)| t).collect::<Vec<_>>()}))
        }
        Some("tools/call") => {
            let params = &request["params"];
            if !params["name"]
                .as_str()
                .is_some_and(|name| state.tools.contains_key(name))
            {
                Err("unoffered tool")
            } else if let Some(native) = params["_meta"]["claudecode/toolUseId"]
                .as_str()
                .filter(|id| !id.is_empty())
            {
                state.run.callback(native.to_owned()).await
            } else {
                Err("missing native toolUseId")
            }
        }
        _ => Err("method not found"),
    };
    let response = match result {
        Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
        Err(message) => {
            json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32000, "message":message}})
        }
    };
    Json(response).into_response()
}

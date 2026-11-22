use super::*;
use agents::readiness::{self, Discovery};
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::time::Duration;

fn binary_identity(agent: &str) -> Result<String, String> {
    let name = readiness::program_name(agent).ok_or("unknown_agent")?;
    let path = readiness::resolve_program(agent, name)?;
    let meta = std::fs::metadata(&path).map_err(|_| "cli_unavailable")?;
    Ok(format!(
        "{}:{:?}:{}:{:?}",
        path.display(),
        std::fs::canonicalize(&path).ok(),
        meta.len(),
        meta.modified()
    ))
}
pub(super) fn identity(state: &GatewayState, agent: &str) -> Result<String, String> {
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    binary_identity(agent)?.hash(&mut h);
    let files = match agent {
        "claude-code" => vec![&paths.claude_settings],
        "codex" => vec![&paths.codex_config],
        "gemini" => vec![&paths.gemini_settings, &paths.gemini_env],
        "opencode" => vec![&paths.opencode_config],
        _ => return Err("unknown_agent".into()),
    };
    for path in files {
        path.hash(&mut h);
        std::fs::read(path).ok().hash(&mut h);
    }
    if agent == "codex" {
        std::fs::read(paths.codex_config.with_file_name("aubridge.config.toml"))
            .ok()
            .hash(&mut h);
    }
    state.port().hash(&mut h);
    state.validation_routing_identity().hash(&mut h);
    serde_json::to_string(&crate::build_info::current())
        .unwrap_or_default()
        .hash(&mut h);
    Ok(format!("{:016x}", h.finish()))
}
fn cache_key(agent: &str) -> String {
    binary_identity(agent).unwrap_or_else(|error| format!("unresolved:{error}"))
}
fn cache(state: &GatewayState, agent: &str, value: &Discovery) {
    if let Ok(mut cache) = state.agent_validation.discovery.lock() {
        cache.insert(agent.into(), (cache_key(agent), value.clone()));
    }
}
pub(crate) fn cached_discovery(state: &GatewayState, agent: &str) -> Discovery {
    let identity = cache_key(agent);
    state
        .agent_validation
        .discovery
        .lock()
        .ok()
        .and_then(|cache| {
            cache
                .get(agent)
                .filter(|(key, d)| {
                    key == &identity
                        && d.observed_at
                            .is_some_and(|at| now().saturating_sub(at) < 300)
                })
                .map(|(_, d)| d.clone())
        })
        .unwrap_or_default()
}
pub(crate) async fn discover(state: &GatewayState, agent: &str) -> Result<Discovery, String> {
    let _activity = state.begin_update_activity().map_err(str::to_owned)?;
    let name = readiness::program_name(agent).ok_or("unknown_agent")?;
    let mut result = Discovery {
        observed_at: Some(now()),
        ..Default::default()
    };
    let program = match readiness::resolve_program(agent, name) {
        Ok(path) => path,
        Err(error) => {
            result.status = if error.contains("不可执行") {
                "not_executable"
            } else {
                "not_found"
            }
            .into();
            cache(state, agent, &result);
            return Ok(result);
        }
    };
    let env_key = match agent {
        "claude-code" => "AUBRIDGE_CLAUDE_BIN",
        "codex" => "AUBRIDGE_CODEX_BIN",
        "gemini" => "AUBRIDGE_GEMINI_BIN",
        _ => "AUBRIDGE_OPENCODE_BIN",
    };
    result.origin = Some(
        if std::env::var_os(env_key).is_some() {
            "override"
        } else if std::env::var_os("PATH").is_some_and(|p| {
            std::env::split_paths(&p).any(|d| {
                std::path::absolute(d.join(name)).is_ok_and(|candidate| candidate == program)
            })
        }) {
            "path"
        } else {
            "known_location"
        }
        .into(),
    );
    let mut command = tokio::process::Command::new(&program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (_sender, receiver) = tokio::sync::watch::channel(false);
    let output = super::process::execute(
        command,
        receiver,
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await;
    result.version = output
        .ok()
        .and_then(|out| readiness::version_from_output(&String::from_utf8_lossy(&out)));
    result.status = match result.version.as_deref() {
        None => "version_unknown",
        Some(v) if readiness::version_supported(agent, v) => "ready",
        Some(_) => "version_unsupported",
    }
    .into();
    cache(state, agent, &result);
    Ok(result)
}

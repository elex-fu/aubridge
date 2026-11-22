//! Native session handles authorize one rechecked, fixed official-CLI launch.
use crate::state::GatewayState;
use agents::native_sessions::{self as native, NativeAgent, NativeRecord};
use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct Store {
    entries: Mutex<BTreeMap<String, Candidate>>,
}
struct Candidate {
    record: NativeRecord,
    identity: String,
    issued_at: u64,
}
const OPENCODE_XDG: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];
#[derive(Debug)]
struct OpenCodeProfile {
    database: PathBuf,
    home: PathBuf,
    xdg: [Option<PathBuf>; 4],
}
impl OpenCodeProfile {
    fn checked(
        database: &Path,
        home: Option<OsString>,
        xdg: [Option<OsString>; 4],
    ) -> Result<Self, String> {
        fn absolute(value: OsString) -> Result<PathBuf, String> {
            let text = value.to_str().ok_or("unsafe_launch_value")?;
            if !native::safe_value(text) || !Path::new(text).is_absolute() {
                return Err("unsafe_launch_value".into());
            }
            Ok(PathBuf::from(value))
        }
        let home = absolute(home.ok_or("wiring_unavailable")?)?;
        let mut result: [Option<PathBuf>; 4] = Default::default();
        for (target, value) in result.iter_mut().zip(xdg) {
            *target = value.map(absolute).transpose()?;
        }
        Ok(Self {
            database: database.to_path_buf(),
            home,
            xdg: result,
        })
    }
    fn current(database: &Path) -> Result<Self, String> {
        Self::checked(
            database,
            std::env::var_os("HOME"),
            OPENCODE_XDG.map(std::env::var_os),
        )
    }
}
#[derive(Serialize)]
struct Summary {
    handle: String,
    agent: String,
    project_alias: String,
    updated_at: u64,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}
#[derive(Serialize)]
struct AgentSummary {
    agent: String,
    state: String,
    reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    handle: String,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn authorized(state: &GatewayState, headers: &HeaderMap) -> bool {
    state.is_valid_admin_key(crate::security::caller_key(headers).as_deref())
}
fn random() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "random_unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn identity(state: &GatewayState, record: &NativeRecord) -> Result<String, String> {
    let agent = record.metadata.agent.id();
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let program = agents::readiness::resolve_program(
        agent,
        agents::readiness::program_name(agent).ok_or("unknown_agent")?,
    )
    .map_err(|_| "cli_unavailable")?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let canonical = std::fs::canonicalize(program).map_err(|_| "cli_unavailable")?;
    let m = std::fs::metadata(&canonical).map_err(|_| "cli_unavailable")?;
    canonical.hash(&mut h);
    m.len().hash(&mut h);
    m.modified().ok().hash(&mut h);
    for file in [
        &paths.claude_settings,
        &paths.codex_config,
        &paths.gemini_settings,
        &paths.gemini_env,
        &paths.opencode_config,
    ] {
        file.hash(&mut h);
        std::fs::read(file).ok().hash(&mut h);
    }
    std::fs::read(paths.codex_config.with_file_name("aubridge.config.toml"))
        .ok()
        .hash(&mut h);
    state.validation_routing_identity().hash(&mut h);
    if record.metadata.agent == NativeAgent::OpenCode {
        std::env::var_os("HOME").hash(&mut h);
        for name in OPENCODE_XDG {
            std::env::var_os(name).hash(&mut h);
        }
    }
    header_check(state, record)?.hash(&mut h);
    Ok(format!("{:016x}", h.finish()))
}
fn header_check(state: &GatewayState, record: &NativeRecord) -> Result<Option<u64>, String> {
    if record.metadata.agent != NativeAgent::Gemini {
        return Ok(None);
    }
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let project = agents::project_check::check(paths, "gemini", &record.canonical_cwd)
        .map_err(|_| agents::native_sessions::gemini_headers::CONFLICT)?;
    let inherited = std::env::var_os("GEMINI_CLI_CUSTOM_HEADERS");
    agents::native_sessions::gemini_headers::check(
        paths,
        &project,
        &record.metadata.native_id,
        inherited.as_deref(),
    )
    .map(Some)
    .map_err(|_| agents::native_sessions::gemini_headers::CONFLICT.into())
}
fn wired(state: &GatewayState, agent: &str) -> bool {
    state.agents_paths().is_some_and(|paths| {
        agents::wirings(paths, state.port(), vec![])
            .iter()
            .any(|w| w.id() == agent && w.wired())
    })
}
fn project_check(state: &GatewayState, record: &NativeRecord) -> Result<(), String> {
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let check =
        agents::project_check::check(paths, record.metadata.agent.id(), &record.canonical_cwd)
            .map_err(|_| "project_context_unreadable")?;
    if !check.can_launch() || check.canonical_path != record.canonical_cwd {
        return Err("project_context_conflict".into());
    }
    Ok(())
}
fn sealed_allowed(state: &GatewayState, record: &NativeRecord) -> Result<(), native::NativeIssue> {
    let mut expected = None;
    record.check_sealed(|item| {
        let owner = state
            .history_owner(&serde_json::json!({"input":[item]}))
            .map_err(|_| native::NativeIssue::OpaqueUnverified)?
            .ok_or(native::NativeIssue::OpaqueUnverified)?;
        if expected.as_ref().is_some_and(|old| old != &owner) {
            return Err(native::NativeIssue::OpaqueUnverified);
        }
        let parts: Vec<String> =
            serde_json::from_str(&owner).map_err(|_| native::NativeIssue::OpaqueUnverified)?;
        if parts.len() != 3 {
            return Err(native::NativeIssue::OpaqueUnverified);
        }
        let manifest = state
            .manifest(&parts[0])
            .ok_or(native::NativeIssue::OpaqueUnverified)?;
        if manifest.provider.base != parts[1]
            || state.current_compaction_subject(&manifest).as_deref() != Some(parts[2].as_str())
        {
            return Err(native::NativeIssue::OpaqueUnverified);
        }
        expected = Some(owner);
        Ok(())
    })
}
fn roots(state: &GatewayState) -> Vec<(NativeAgent, PathBuf)> {
    let Some(p) = state.agents_paths() else {
        return vec![];
    };
    let mut roots = vec![];
    if let Some(home) = p.codex_config.parent() {
        roots.push((NativeAgent::Codex, home.join("sessions")));
        roots.push((NativeAgent::Codex, home.join("archived_sessions")));
    }
    if let Some(home) = p.claude_settings.parent() {
        roots.push((NativeAgent::Claude, home.join("projects")));
    }
    roots
}
fn scans(state: &GatewayState, agent: NativeAgent) -> Vec<native::NativeScan> {
    let Some(paths) = state.agents_paths() else {
        return vec![];
    };
    match agent {
        NativeAgent::Codex | NativeAgent::Claude => roots(state)
            .into_iter()
            .filter(|(id, _)| *id == agent)
            .map(|(_, root)| native::scan_jsonl(agent, &root))
            .collect(),
        NativeAgent::Gemini => paths
            .gemini_settings
            .parent()
            .map(|home| vec![native::scan_gemini(home)])
            .unwrap_or_default(),
        NativeAgent::OpenCode => {
            let database = std::env::var_os("OPENCODE_DB")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("XDG_DATA_HOME")
                        .map(PathBuf::from)
                        .map(|p| p.join("opencode/opencode.db"))
                })
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|p| p.join(".local/share/opencode/opencode.db"))
                });
            database
                .filter(|p| p.is_absolute())
                .and_then(|p| p.parent().map(|parent| native::scan_opencode(parent, &p)))
                .map(|scan| vec![scan])
                .unwrap_or_default()
        }
    }
}
pub(crate) async fn list(state: Arc<GatewayState>, headers: HeaderMap) -> Response<Body> {
    if !authorized(&state, &headers) {
        return crate::error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    match tokio::task::spawn_blocking(move || collect(&state)).await {
        Ok(Ok(value)) => ([("cache-control", "no-store")], Json(value)).into_response(),
        _ => crate::error_response(StatusCode::SERVICE_UNAVAILABLE, "session_store_unavailable"),
    }
}
fn collect(state: &GatewayState) -> Result<serde_json::Value, String> {
    let mut sessions = vec![];
    let mut agents = vec![];
    state
        .native_sessions
        .entries
        .lock()
        .map_err(|_| "session_store_unavailable")?
        .retain(|_, c| now().saturating_sub(c.issued_at) < 300);
    for agent in [
        NativeAgent::Codex,
        NativeAgent::Claude,
        NativeAgent::OpenCode,
        NativeAgent::Gemini,
    ] {
        let discovery = crate::agent_validation::cached_discovery(state, agent.id());
        let reason = if !cfg!(target_os = "macos") {
            Some("platform_unsupported")
        } else if !wired(state, agent.id()) {
            Some("not_wired")
        } else if discovery.status != "ready" {
            Some("cli_check_required")
        } else {
            None
        };
        agents.push(AgentSummary {
            agent: agent.id().into(),
            state: if reason.is_some() {
                "read_only"
            } else {
                "ready"
            }
            .into(),
            reason: reason.map(str::to_owned),
        });
        for scan in scans(state, agent) {
            for record in scan.records {
                if sessions.len() >= 200 {
                    break;
                }
                let reason = reason.or(if project_check(state, &record).is_err() {
                    Some("project_context_conflict")
                } else if record.metadata.contains_opaque && sealed_allowed(state, &record).is_err()
                {
                    Some("opaque_unverified")
                } else if header_check(state, &record).is_err() {
                    Some(agents::native_sessions::gemini_headers::CONFLICT)
                } else {
                    None
                });
                let handle = random()?;
                let summary = Summary {
                    handle: handle.clone(),
                    agent: agent.id().into(),
                    project_alias: record.project_alias(),
                    updated_at: record.updated_at,
                    state: if reason.is_none() {
                        "ready"
                    } else {
                        "read_only"
                    }
                    .into(),
                    reason: reason.map(str::to_owned),
                };
                if reason.is_none() {
                    if let Ok(identity) = identity(state, &record) {
                        state
                            .native_sessions
                            .entries
                            .lock()
                            .map_err(|_| "session_store_unavailable")?
                            .insert(
                                handle,
                                Candidate {
                                    record,
                                    identity,
                                    issued_at: now(),
                                },
                            );
                    }
                }
                sessions.push(summary);
            }
            for issue in scan.issues {
                if let Some(a) = agents.last_mut() {
                    if a.reason.is_none() {
                        a.reason = Some(issue.code().into());
                    }
                }
            }
        }
    }
    let mut entries = state
        .native_sessions
        .entries
        .lock()
        .map_err(|_| "session_store_unavailable")?;
    while entries.len() > 400 {
        let Some(key) = entries.keys().next().cloned() else {
            break;
        };
        entries.remove(&key);
    }
    Ok(serde_json::json!({"sessions":sessions,"agents":agents}))
}
pub(crate) async fn resume(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !authorized(&state, &headers) {
        return crate::error_response(StatusCode::UNAUTHORIZED, "admin key required");
    }
    let request = match serde_json::from_str::<Resume>(&body) {
        Ok(r) => r,
        Err(_) => return crate::error_response(StatusCode::BAD_REQUEST, "handle_only"),
    };
    let result = launch(&state, &request.handle).await;
    match result {
        Ok((agent, project)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"state":"launched","agent":agent,"project_alias":project})),
        )
            .into_response(),
        Err(reason) => crate::error_response(StatusCode::CONFLICT, &reason),
    }
}
async fn launch(state: &GatewayState, handle: &str) -> Result<(String, String), String> {
    if !cfg!(target_os = "macos") {
        return Err("platform_unsupported".into());
    }
    if handle.len() != 64 || !handle.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid_handle".into());
    }
    let _activity = state.begin_update_activity().map_err(str::to_owned)?;
    let candidate = state
        .native_sessions
        .entries
        .lock()
        .map_err(|_| "session_store_unavailable")?
        .remove(handle)
        .ok_or("session_handle_expired")?;
    if now().saturating_sub(candidate.issued_at) >= 300 {
        return Err("session_handle_expired".into());
    }
    let agent = candidate.record.metadata.agent.id();
    let discovery = crate::agent_validation::discover(state, agent).await?;
    if discovery.status != "ready" {
        return Err(discovery.status);
    }
    if identity(state, &candidate.record)? != candidate.identity {
        return Err("connection_changed".into());
    }
    if !wired(state, agent) {
        return Err("not_wired".into());
    }
    candidate.record.recheck().map_err(|e| e.code())?;
    project_check(state, &candidate.record)?;
    if candidate.record.metadata.contains_opaque {
        sealed_allowed(state, &candidate.record).map_err(|e| e.code())?;
    }
    header_check(state, &candidate.record)?;
    let program = agents::readiness::resolve_program(
        agent,
        agents::readiness::program_name(agent).ok_or("unknown_agent")?,
    )
    .map_err(|_| "cli_unavailable")?;
    let program = std::fs::canonicalize(program).map_err(|_| "cli_unavailable")?;
    let id = &candidate.record.metadata.native_id;
    let cwd = candidate
        .record
        .canonical_cwd
        .to_str()
        .ok_or("unsafe_launch_value")?;
    let args = resume_args(candidate.record.metadata.agent, id, cwd);
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let home = match candidate.record.metadata.agent {
        NativeAgent::Codex => paths.codex_config.parent().map(|p| ("CODEX_HOME", p)),
        NativeAgent::Claude => paths
            .claude_settings
            .parent()
            .map(|p| ("CLAUDE_CONFIG_DIR", p)),
        NativeAgent::Gemini => paths
            .gemini_settings
            .parent()
            .and_then(Path::parent)
            .map(|p| ("GEMINI_CLI_HOME", p)),
        NativeAgent::OpenCode => Some(("OPENCODE_CONFIG", paths.opencode_config.as_path())),
    }
    .ok_or("wiring_unavailable")?;
    let opencode_profile = if agent == "opencode" {
        Some(OpenCodeProfile::current(
            candidate.record.database_path().ok_or("record_changed")?,
        )?)
    } else {
        None
    };
    launch_terminal(
        &program,
        &candidate.record.canonical_cwd,
        &args,
        agent,
        home,
        opencode_profile.as_ref(),
        (agent == "gemini").then_some(id.as_str()),
    )
    .await?;
    Ok((agent.into(), candidate.record.project_alias()))
}
fn resume_args<'a>(agent: NativeAgent, id: &'a str, cwd: &'a str) -> Vec<&'a str> {
    match agent {
        NativeAgent::Codex => vec!["--profile", "aubridge", "resume", id],
        NativeAgent::Claude => vec!["--resume", id],
        NativeAgent::OpenCode => vec!["--standalone", "--session", id, cwd],
        NativeAgent::Gemini => vec!["--resume", id],
    }
}
fn quote(value: &str) -> Result<String, String> {
    if !native::safe_value(value) {
        return Err("unsafe_launch_value".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}
fn terminal_script(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    agent: &str,
    home: (&str, &Path),
    opencode_profile: Option<&OpenCodeProfile>,
    native_id: Option<&str>,
) -> Result<String, String> {
    let mut script = String::from("#!/bin/sh\nset +x\nset -eu\n");
    // A fixed POSIX interpreter avoids depending on the user's interactive shell.
    script.push_str("rm -f -- \"$0\"\nrmdir -- \"$(dirname -- \"$0\")\" 2>/dev/null || :\n");
    if agent == "gemini" {
        let header = agents::native_sessions::gemini_headers::expected(
            native_id.ok_or("invalid_session_id")?,
        )
        .map_err(|_| "invalid_session_id")?;
        script.push_str("if [ -n \"${GEMINI_CLI_CUSTOM_HEADERS-}\" ] && [ \"${GEMINI_CLI_CUSTOM_HEADERS-}\" != ");
        script.push_str(&quote(&header)?);
        script.push_str(" ]; then\n  printf '%s\\n' 'gemini_custom_headers_conflict: 自定义请求头需核对，请核对终端环境后在 App 刷新重试。' >&2\n  exit 78\nfi\nexport GEMINI_CLI_CUSTOM_HEADERS=");
        script.push_str(&quote(&header)?);
        script.push('\n');
    }
    script.push_str("cd -- ");
    script.push_str(&quote(cwd.to_str().ok_or("unsafe_launch_value")?)?);
    script.push('\n');
    let clear: &[&str] = match agent {
        "claude-code" => crate::agent_test::CLAUDE_CLEAR_ENV,
        "gemini" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GEMINI_BASE_URL",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GEMINI_CLI_USE_WORKLOAD_IDENTITY",
            "GEMINI_CLI_HOME",
        ],
        "opencode" => &[
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_CONFIG_DIR",
            "OPENCODE_CONFIG",
        ],
        _ => &[
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_API_KEY",
            "CODEX_PROFILE",
        ],
    };
    for key in clear {
        script.push_str("unset ");
        script.push_str(key);
        script.push('\n');
    }
    script.push_str("export ");
    script.push_str(home.0);
    script.push('=');
    script.push_str(&quote(home.1.to_str().ok_or("unsafe_launch_value")?)?);
    if agent == "gemini" {
        script.push_str("\nexport HOME=");
        script.push_str(&quote(home.1.to_str().ok_or("unsafe_launch_value")?)?);
    }
    if agent == "opencode" {
        let profile = opencode_profile.ok_or("wiring_unavailable")?;
        script.push_str("\nexport HOME=");
        script.push_str(&quote(profile.home.to_str().ok_or("unsafe_launch_value")?)?);
        for (name, value) in OPENCODE_XDG.iter().zip(&profile.xdg) {
            script.push('\n');
            if let Some(path) = value {
                script.push_str("export ");
                script.push_str(name);
                script.push('=');
                script.push_str(&quote(path.to_str().ok_or("unsafe_launch_value")?)?);
            } else {
                script.push_str("unset ");
                script.push_str(name);
            }
        }
        script.push_str("\nexport OPENCODE_DB=");
        script.push_str(&quote(
            profile.database.to_str().ok_or("unsafe_launch_value")?,
        )?);
    }
    script.push_str("\nexec ");
    script.push_str(&quote(program.to_str().ok_or("unsafe_launch_value")?)?);
    for arg in args {
        script.push(' ');
        script.push_str(&quote(arg)?);
    }
    script.push('\n');
    Ok(script)
}
async fn launch_terminal(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    agent: &str,
    home: (&str, &Path),
    opencode_profile: Option<&OpenCodeProfile>,
    native_id: Option<&str>,
) -> Result<(), String> {
    let script = terminal_script(program, cwd, args, agent, home, opencode_profile, native_id)?;
    let dir = std::env::temp_dir().join(format!("aubridge-resume-{}", random()?));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir).map_err(|_| "terminal_launch_failed")?;
    let file = dir.join("resume.command");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o700);
    }
    {
        use std::io::Write;
        opts.open(&file)
            .and_then(|mut f| f.write_all(script.as_bytes()))
            .map_err(|_| "terminal_launch_failed")?;
    }
    let mut command = tokio::process::Command::new("/usr/bin/open");
    command
        .args(["-a", "Terminal"])
        .arg(&file)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), command.status()).await;
    if !matches!(status,Ok(Ok(status)) if status.success()) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("terminal_launch_failed".into());
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        let _ = std::fs::remove_dir_all(dir);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_cannot_supply_paths_or_commands() {
        assert!(serde_json::from_str::<Resume>(r#"{"handle":"x","cwd":"/tmp"}"#).is_err());
        assert!(serde_json::from_str::<Resume>(r#"{"handle":"x","argv":["echo"]}"#).is_err());
        assert!(serde_json::from_str::<Resume>(
            r#"{"handle":"x","headers":{"x-session-id":"caller-choice"}}"#
        )
        .is_err());
    }
    #[test]
    fn opencode_native_resume_uses_standalone_exact_id_and_original_cwd() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../agents/tests/fixtures/native-opencode-2.2.5-argv.json"
        ))
        .unwrap();
        assert_eq!(
            agents::readiness::supported_version("opencode"),
            fixture["version"].as_str()
        );
        let id = fixture["native_id"].as_str().unwrap();
        let cwd = fixture["canonical_cwd"].as_str().unwrap();
        let expected = fixture["resume_args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(resume_args(NativeAgent::OpenCode, id, cwd), expected);
        assert!(serde_json::from_value::<Resume>(serde_json::json!({"handle":"opaque","native_id":id,"headers":{"thread-id":"untrusted"}})).is_err());
    }
    #[test]
    fn opencode_profile_rejects_missing_or_unsafe_explicit_app_paths() {
        let base = std::env::temp_dir().join("opencode-profile-boundary");
        let database = base.join("opencode.db");
        let home = base.join("home").into_os_string();
        assert_eq!(
            OpenCodeProfile::checked(&database, None, Default::default()).unwrap_err(),
            "wiring_unavailable"
        );
        for bad in [
            OsString::from(""),
            OsString::from("relative"),
            OsString::from("/tmp/line\nbreak"),
            OsString::from(format!("/{}", "x".repeat(4096))),
        ] {
            assert_eq!(
                OpenCodeProfile::checked(&database, Some(bad.clone()), Default::default())
                    .unwrap_err(),
                "unsafe_launch_value"
            );
            assert_eq!(
                OpenCodeProfile::checked(
                    &database,
                    Some(home.clone()),
                    [Some(bad), None, None, None]
                )
                .unwrap_err(),
                "unsafe_launch_value"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = OsString::from_vec(vec![b'/', 0xff]);
            assert_eq!(
                OpenCodeProfile::checked(&database, Some(bad.clone()), Default::default())
                    .unwrap_err(),
                "unsafe_launch_value"
            );
            assert_eq!(
                OpenCodeProfile::checked(
                    &database,
                    Some(home.clone()),
                    [None, Some(bad), None, None]
                )
                .unwrap_err(),
                "unsafe_launch_value"
            );
        }
        OpenCodeProfile::checked(&database, Some(home), Default::default()).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn opencode_rendered_command_binds_app_profile_and_clears_terminal_xdg() {
        use std::os::unix::fs::PermissionsExt;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../agents/tests/fixtures/native-opencode-2.2.5-argv.json"
        ))
        .unwrap();
        let root =
            std::env::temp_dir().join(format!("aubridge-opencode-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("original-project");
        std::fs::create_dir_all(&cwd).unwrap();
        let app_home = root.join("app-home");
        let app_data = root.join("app-data");
        let app_state = root.join("app-state");
        let database = root.join("pinned/opencode.db");
        let config = root.join("pinned/opencode.json");
        let profile = OpenCodeProfile::checked(
            &database,
            Some(app_home.clone().into_os_string()),
            [
                None,
                Some(app_data.clone().into_os_string()),
                None,
                Some(app_state.clone().into_os_string()),
            ],
        )
        .unwrap();
        let program = root.join("fixture-cli");
        std::fs::write(&program,"#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"${XDG_CONFIG_HOME-UNSET}\" \"${XDG_DATA_HOME-UNSET}\" \"${XDG_CACHE_HOME-UNSET}\" \"${XDG_STATE_HOME-UNSET}\" \"$OPENCODE_DB\" \"$OPENCODE_CONFIG\" \"$@\" > received-profile\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = fixture["native_id"].as_str().unwrap();
        let cwd_text = cwd.to_str().unwrap();
        let args = resume_args(NativeAgent::OpenCode, id, cwd_text);
        let script = terminal_script(
            &program,
            &cwd,
            &args,
            "opencode",
            ("OPENCODE_CONFIG", &config),
            Some(&profile),
            None,
        )
        .unwrap();
        let command_file = root.join("resume.command");
        std::fs::write(&command_file, script).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg(&command_file)
            .env("HOME", root.join("wrong-terminal-home"));
        for name in OPENCODE_XDG {
            command.env(name, root.join("wrong-terminal-xdg"));
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert!(!command_file.exists());
        let actual = std::fs::read_to_string(cwd.join("received-profile")).unwrap();
        let expected = [
            app_home.to_str().unwrap(),
            "UNSET",
            app_data.to_str().unwrap(),
            "UNSET",
            app_state.to_str().unwrap(),
            database.to_str().unwrap(),
            config.to_str().unwrap(),
            "--standalone",
            "--session",
            id,
            cwd_text,
        ];
        assert_eq!(actual.lines().collect::<Vec<_>>(), expected);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn posix_values_remain_literal_and_controls_are_rejected() {
        assert_eq!(quote("a'$(touch x); b").unwrap(), "'a'\\''$(touch x); b'");
        assert!(quote("a\nb").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn gemini_terminal_guard_blocks_conflicts_without_spawning_or_disclosing_headers() {
        use std::os::unix::fs::PermissionsExt;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../agents/tests/fixtures/gemini-session-correlation.json"
        ))
        .unwrap();
        let id = fixture["native_id"].as_str().unwrap();
        let expected = fixture["header"].as_str().unwrap();
        let root =
            std::env::temp_dir().join(format!("aubridge-native-header-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("home/work");
        std::fs::create_dir_all(&cwd).unwrap();
        let program = root.join("fixture-cli");
        std::fs::write(
            &program,
            "#!/bin/sh\nprintf '%s' \"$GEMINI_CLI_CUSTOM_HEADERS\" > received-header\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = terminal_script(
            &program,
            &cwd,
            &["--resume", id],
            "gemini",
            ("GEMINI_CLI_HOME", &root.join("home")),
            None,
            Some(id),
        )
        .unwrap();
        let secret = "authorization: fixture-secret-must-not-leak";
        assert!(!script.contains(secret));
        assert!(script.contains(expected));
        for inherited in [
            Some(secret),
            Some("thread-id: unrelated"),
            None,
            Some(expected),
        ] {
            let file = root.join("resume.command");
            std::fs::write(&file, &script).unwrap();
            let mut command = std::process::Command::new("/bin/sh");
            command.arg(&file).env_remove("GEMINI_CLI_CUSTOM_HEADERS");
            if let Some(value) = inherited {
                command.env("GEMINI_CLI_CUSTOM_HEADERS", value);
            }
            let output = command.output().unwrap();
            assert!(
                !file.exists(),
                "both conflict and success must remove the private command file"
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("fixture-secret-must-not-leak")
            );
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("fixture-secret-must-not-leak")
            );
            if inherited.is_some_and(|value| value != expected) {
                assert_eq!(output.status.code(), Some(78));
                assert!(
                    !cwd.join("received-header").exists(),
                    "conflict must start zero CLI processes"
                );
                assert!(String::from_utf8_lossy(&output.stderr)
                    .contains("gemini_custom_headers_conflict"));
            } else {
                assert!(output.status.success());
                assert_eq!(
                    std::fs::read_to_string(cwd.join("received-header")).unwrap(),
                    expected
                );
                std::fs::remove_file(cwd.join("received-header")).unwrap();
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn known_chatgpt_checkpoint_uses_compaction_subject_and_rebind_is_rejected() {
        let root =
            std::env::temp_dir().join(format!("aubridge-native-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let auth = root.join("auth.json");
        std::fs::write(
            &auth,
            r#"{"tokens":{"access_token":"fixture-access","account_id":"fixture-account"}}"#,
        )
        .unwrap();
        let manifest = registry::presets()
            .into_iter()
            .find(|m| m.id == "chatgpt")
            .unwrap();
        let state = GatewayState::from_manifests(
            BTreeMap::from([("chatgpt".into(), manifest.clone())]),
            "fixture",
        )
        .with_codex_channel(crate::chatgpt::ChatGptChannel::new(
            auth.clone(),
            root.join("config.toml"),
        ));
        let compaction_subject = state.current_compaction_subject(&manifest).unwrap();
        assert_ne!(
            Some(compaction_subject.clone()),
            state.current_quota_subject(&manifest)
        );
        let owner = serde_json::json!([manifest.id, manifest.provider.base, compaction_subject])
            .to_string();
        let item = serde_json::json!({"type":"compaction","encrypted_content":"fixture-opaque"});
        state.save_checkpoint(&item, &owner, 0).unwrap();
        let id = "c18365a3-0000-4000-8000-000000000001";
        let file = root.join(format!("{id}.jsonl"));
        std::fs::write(
            &file,
            format!(
                "{}\n{}",
                serde_json::json!({"sessionId":id,"cwd":root,"isSidechain":false}),
                item
            ),
        )
        .unwrap();
        let record = native::read_jsonl(NativeAgent::Claude, &root, &file).unwrap();
        assert!(sealed_allowed(&state, &record).is_ok());
        std::fs::write(&auth,r#"{"tokens":{"access_token":"fixture-other-access","account_id":"fixture-other-account"}}"#).unwrap();
        assert_eq!(
            sealed_allowed(&state, &record).unwrap_err(),
            native::NativeIssue::OpaqueUnverified
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

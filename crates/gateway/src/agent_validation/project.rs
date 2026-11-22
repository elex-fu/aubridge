//! Project jobs retain the selected cwd and its original environment/trust contract.
use super::*;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch(parent: &Path, id: &str) -> Result<Scratch, String> {
    let path = parent.join(format!(".cli-file-probe-{id}"));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&path)
        .map_err(|_| "project_probe_directory_failed")?;
    Ok(Scratch(path))
}
fn random() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "random_unavailable")?;
    Ok(bytes.iter().map(|v| format!("{v:02x}")).collect())
}
fn challenge_prompt(cwd: &Path, challenge: &Path, within_project: bool) -> Result<String, String> {
    let reference = if within_project {
        challenge
            .strip_prefix(cwd)
            .map_err(|_| "project_path_invalid")?
    } else {
        challenge
    };
    let path =
        serde_json::to_string(&reference.to_string_lossy()).map_err(|_| "project_path_invalid")?;
    Ok(format!("Use your file reading tool to read {path}. Reply with exactly its complete contents and nothing else. Do not guess or print other text."))
}
fn execution_context(agent: &str, paths: &agents::AgentsPaths, cwd: &Path) -> Result<(), String> {
    let keys: &[&str] = match agent {
        "gemini" => &[
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENAI_USE_VERTEXAI",
        ],
        "claude-code" => crate::agent_test::CLAUDE_CLEAR_ENV,
        "codex" => &["OPENAI_BASE_URL", "OPENAI_API_KEY"],
        "opencode" => &[
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_MODEL",
            "OPENAI_BASE_URL",
        ],
        _ => &[],
    };
    if keys
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Err("project_environment_override".into());
    }
    if agent == "gemini" {
        let project_settings = cwd.join(".gemini/settings.json");
        for file in [&paths.gemini_settings, &project_settings] {
            match std::fs::metadata(file) {
                Ok(meta) => {
                    if meta.len() > 1_048_576 {
                        return Err("project_context_conflict".into());
                    }
                    let bytes = std::fs::read(file).map_err(|_| "project_context_conflict")?;
                    let value: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(|_| "project_context_conflict")?;
                    if ["mcpServers", "hooks"].iter().any(|key| {
                        value.get(key).is_some_and(|v| {
                            v.as_object()
                                .map(|map| !map.is_empty())
                                .or_else(|| v.as_array().map(|items| !items.is_empty()))
                                .unwrap_or(!v.is_null())
                        })
                    }) {
                        return Err("project_execution_extensions".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("project_context_conflict".into()),
            }
        }
        if let Some(config) = paths.gemini_settings.parent() {
            match std::fs::read_dir(config.join("extensions")) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        return Err("project_execution_extensions".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("project_context_conflict".into()),
            }
        }
    }
    Ok(())
}
fn configure(
    state: &GatewayState,
    agent: &str,
    program: &Path,
    cwd: &Path,
    scratch: &Path,
    prompt: &str,
    session: &str,
) -> Result<tokio::process::Command, String> {
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let mut c = tokio::process::Command::new(program);
    c.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    match agent {
        "claude-code" => {
            // Override execution hooks only; retain actual user/project connection settings.
            let settings = scratch.join("claude-probe-settings.json");
            std::fs::write(&settings, b"{\"disableAllHooks\":true}")
                .map_err(|_| "isolated_config_failed")?;
            c.arg("--settings").arg(settings).args([
                "-p",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--output-format",
                "stream-json",
                "--verbose",
                "--session-id",
                session,
                "--permission-mode",
                "dontAsk",
                "--tools",
                "Read",
                "--allowedTools",
                "Read",
                "--disable-slash-commands",
                prompt,
            ]);
            if let Some(config) = paths.claude_settings.parent() {
                c.env("CLAUDE_CONFIG_DIR", config);
            }
        }
        "codex" => {
            c.args([
                "--profile",
                "aubridge",
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "-c",
                "mcp_servers={}",
                "--json",
                prompt,
            ]);
            if let Some(home) = paths.codex_config.parent() {
                c.env("CODEX_HOME", home);
            }
        }
        "opencode" => {
            let wiring = agents::opencode::OpenCode::new(
                paths.opencode_config.clone(),
                paths.state_dir.clone(),
                state.port(),
            );
            let model = wiring
                .current_model()
                .unwrap_or_else(|| crate::AUTO_MODEL.into());
            let config = agents::probe_config::opencode_provider(&paths.opencode_config, session)
                .map_err(|_| "isolated_config_failed")?;
            let config_path = scratch.join("opencode.json");
            std::fs::write(
                &config_path,
                serde_json::to_vec(&config).map_err(|_| "isolated_config_failed")?,
            )
            .map_err(|_| "isolated_config_failed")?;
            c.args([
                "--standalone",
                "run",
                "--format",
                "json",
                "--model",
                &format!("aubridge/{model}"),
                prompt,
            ]);
            c.env("OPENCODE_CONFIG", config_path)
                .env("XDG_CONFIG_HOME", scratch.join("config"))
                .env("XDG_DATA_HOME", scratch.join("data"))
                .env("XDG_STATE_HOME", scratch.join("state"))
                .env("XDG_CACHE_HOME", scratch.join("cache"));
        }
        "gemini" => {
            // Do not set HOME, trust, base URL, API key or ignoreLocalEnv for a project job.
            c.args([
                "--session-id",
                session,
                "--prompt",
                prompt,
                "--output-format",
                "stream-json",
                "--approval-mode",
                "default",
            ]);
            c.env(
                "GEMINI_CLI_CUSTOM_HEADERS",
                format!("x-session-id: {session}"),
            );
            if let Some(home) = paths.gemini_settings.parent().and_then(Path::parent) {
                c.env("GEMINI_CLI_HOME", home);
            }
        }
        _ => return Err("unknown_agent".into()),
    }
    Ok(c)
}
pub(super) async fn run(
    state: &GatewayState,
    agent: &str,
    id: &str,
    handle: &str,
    allow_temp: bool,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Evidence, String> {
    if *cancel.borrow() {
        return Err("cancelled".into());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    let (cwd, _alias) = crate::agent_project::resolve(state, agent, handle)?;
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    execution_context(agent, paths, &cwd)?;
    if !agents::wirings(paths, state.port(), vec![])
        .iter()
        .any(|w| w.id() == agent && w.wired())
    {
        return Err("not_wired".into());
    }
    let d = discover(state, agent).await?;
    if d.status != "ready" {
        return Err(d.status);
    }
    let initial = discovery::identity(state, agent)?;
    let temporary = std::env::temp_dir();
    let private = scratch(if allow_temp { &cwd } else { &temporary }, id)?;
    let answer = random()?;
    let challenge = private.0.join("challenge.txt");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        options
            .open(&challenge)
            .and_then(|mut file| file.write_all(answer.as_bytes()))
            .map_err(|_| "private_file_failed")?;
    }
    let challenge = std::fs::canonicalize(challenge).map_err(|_| "private_file_failed")?;
    let prompt = challenge_prompt(&cwd, &challenge, allow_temp)?;
    let raw = random()?;
    let session = format!(
        "{}-{}-4{}-8{}-{}",
        &raw[..8],
        &raw[8..12],
        &raw[13..16],
        &raw[17..20],
        &raw[20..32]
    );
    let program = agents::readiness::resolve_program(
        agent,
        agents::readiness::program_name(agent).ok_or("unknown_agent")?,
    )?;
    crate::agent_project::resolve(state, agent, handle)?;
    let command = configure(state, agent, &program, &cwd, &private.0, &prompt, &session)?;
    let output = super::process::execute(command, cancel.clone(), deadline).await?;
    let parsed = parser::parse(agent, &String::from_utf8_lossy(&output), &answer);
    if !(parsed.successful
        && parsed.tool_started
        && parsed.tool_result_matched
        && parsed.final_exact)
    {
        return Err(if allow_temp {
            "file_task_unverified"
        } else {
            "project_external_read_unverified"
        }
        .into());
    }
    let native = parsed.session.ok_or("session_unverified")?;
    loop {
        if *cancel.borrow() {
            return Err("cancelled".into());
        }
        crate::agent_project::resolve(state, agent, handle)?;
        if discovery::identity(state, agent)? != initial {
            return Err("identity_changed".into());
        }
        let mut evidence_session = native.clone();
        let mut requests = state.validation_session_evidence(agent, &evidence_session)?;
        if requests.is_empty() && agent == "opencode" {
            evidence_session = session.clone();
            requests = state.validation_session_evidence(agent, &evidence_session)?;
        }
        if requests.len() >= 2 {
            return Ok(Evidence {
                cli_version: d.version.ok_or("version_unknown")?,
                gateway_revision: crate::build_info::current()
                    .git_revision
                    .unwrap_or("unknown")
                    .into(),
                session_id: evidence_session,
                requests,
                tool_result_matched: true,
                final_exact: true,
            });
        }
        tokio::select! {_=cancel.changed()=>return Err("cancelled".into()),_=tokio::time::sleep_until(deadline)=>return Err("timed_out".into()),_=tokio::time::sleep(Duration::from_millis(200))=>{}}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consented_project_prompt_omits_user_project_name_and_uses_relative_file() {
        let cwd = Path::new("/work/Aubridge-private-project");
        let challenge = cwd.join(".cli-file-probe-job/challenge.txt");
        let prompt = challenge_prompt(cwd, &challenge, true).unwrap();
        assert!(prompt.contains(".cli-file-probe-job/challenge.txt"));
        assert!(!prompt.contains("/work") && !prompt.to_lowercase().contains("aubridge"));
        assert!(challenge_prompt(cwd, Path::new("/other/challenge.txt"), true).is_err());
    }
    #[test]
    fn external_challenge_prompt_keeps_the_actual_absolute_file_reference() {
        let prompt = challenge_prompt(
            Path::new("/work/project"),
            Path::new("/tmp/.cli-file-probe-job/challenge.txt"),
            false,
        )
        .unwrap();
        assert!(prompt.contains("/tmp/.cli-file-probe-job/challenge.txt"));
        assert!(!prompt.to_lowercase().contains("aubridge"));
    }
}

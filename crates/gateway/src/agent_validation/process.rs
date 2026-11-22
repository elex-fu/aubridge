use super::*;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
const LIMIT: u64 = 1_048_576;
const DEADLINE: Duration = Duration::from_secs(150);
const PROMPT: &str = "Use your file reading tool to read challenge.txt in the current directory. Reply with exactly the complete file contents and nothing else. Do not guess or print any other text.";
struct PrivateDir(PathBuf);
impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn private_dir(id: &str) -> Result<PrivateDir, String> {
    let path = std::env::temp_dir().join(format!("aubridge-file-validation-{id}"));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&path)
        .map_err(|_| "private_directory_failed")?;
    Ok(PrivateDir(path))
}
fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "random_unavailable")?;
    Ok(bytes.iter().map(|v| format!("{v:02x}")).collect())
}
fn session_id() -> Result<String, String> {
    let s = nonce()?;
    Ok(format!(
        "{}-{}-4{}-8{}-{}",
        &s[..8],
        &s[8..12],
        &s[13..16],
        &s[17..20],
        &s[20..32]
    ))
}
fn configure(
    state: &GatewayState,
    agent: &str,
    program: &Path,
    dir: &Path,
    session: &str,
) -> Result<tokio::process::Command, String> {
    let paths = state.agents_paths().ok_or("wiring_unavailable")?;
    let wiring = agents::wirings(paths, state.port(), vec![])
        .into_iter()
        .find(|w| w.id() == agent)
        .ok_or("unknown_agent")?;
    if !wiring.wired() {
        return Err("not_wired".into());
    }
    let mut c = tokio::process::Command::new(program);
    c.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    #[cfg(unix)]
    c.as_std_mut().process_group(0);
    match agent {
        "claude-code" => {
            for key in crate::agent_test::CLAUDE_CLEAR_ENV {
                c.env_remove(key);
            }
            let settings = dir.join("claude-settings.json");
            let config = agents::probe_config::claude_settings(&paths.claude_settings)
                .map_err(|_| "isolated_config_failed")?;
            std::fs::write(
                &settings,
                serde_json::to_vec(&config).map_err(|_| "isolated_config_failed")?,
            )
            .map_err(|_| "isolated_config_failed")?;
            c.arg("--settings")
                .arg(&settings)
                .args(["--setting-sources", ""]);
            c.args([
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
                "Read(./challenge.txt)",
                "--disable-slash-commands",
                PROMPT,
            ]);
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
                PROMPT,
            ]);
        }
        "opencode" => {
            let model = wiring
                .current_model()
                .unwrap_or_else(|| crate::AUTO_MODEL.into());
            c.args([
                "--standalone",
                "run",
                "--format",
                "json",
                "--model",
                &format!("aubridge/{model}"),
                PROMPT,
            ]);
            let config = agents::probe_config::opencode_provider(&paths.opencode_config, session)
                .map_err(|_| "isolated_config_failed")?;
            let config_path = dir.join("opencode.json");
            std::fs::write(
                &config_path,
                serde_json::to_vec(&config).map_err(|_| "isolated_config_failed")?,
            )
            .map_err(|_| "isolated_config_failed")?;
            c.env("OPENCODE_CONFIG", &config_path)
                .env("XDG_CONFIG_HOME", dir.join("config"))
                .env("XDG_DATA_HOME", dir.join("data"))
                .env("XDG_STATE_HOME", dir.join("state"))
                .env("XDG_CACHE_HOME", dir.join("cache"));
        }
        "gemini" => {
            let layout =
                agents::probe_config::gemini_layout(dir).map_err(|_| "private_directory_failed")?;
            let home = layout.home;
            c.current_dir(layout.work);
            let config = home.join(".gemini");
            let model = wiring.current_model().ok_or("managed_model_missing")?;
            let isolated = agents::gemini::GeminiCli::new(
                config.join("settings.json"),
                config.join(".env"),
                dir.join("wiring"),
                state.port(),
            );
            isolated.apply().map_err(|_| "isolated_config_failed")?;
            isolated
                .set_model(&model)
                .map_err(|_| "isolated_config_failed")?;
            for (key, _) in std::env::vars_os() {
                let upper = key.to_string_lossy().to_ascii_uppercase();
                if ["GEMINI", "GOOGLE", "VERTEX", "GCLOUD"]
                    .iter()
                    .any(|token| upper.contains(token))
                {
                    c.env_remove(key);
                }
            }
            let mut search = vec![];
            if let Some(parent) = program.parent() {
                search.push(parent.to_path_buf());
            }
            if let Some(home) = std::env::var_os("HOME") {
                search.push(PathBuf::from(home).join(".local/node/bin"));
            }
            if let Some(path) = std::env::var_os("PATH") {
                search.extend(std::env::split_paths(&path));
            }
            c.env(
                "PATH",
                std::env::join_paths(search).map_err(|_| "cli_path_invalid")?,
            );
            c.args([
                "--prompt",
                PROMPT,
                "--output-format",
                "stream-json",
                "--approval-mode",
                "default",
                "--session-id",
                session,
            ]);
            c.env("HOME", &home)
                .env("GEMINI_CLI_HOME", &home)
                .env("XDG_CONFIG_HOME", dir.join("config"))
                .env("XDG_DATA_HOME", dir.join("data"))
                .env("XDG_STATE_HOME", dir.join("state"))
                .env("XDG_CACHE_HOME", dir.join("cache"))
                .env("TMPDIR", dir)
                .env("CI", "true")
                .env("GEMINI_CLI_TRUST_WORKSPACE", "true")
                .env(
                    "GEMINI_CLI_CUSTOM_HEADERS",
                    format!("x-session-id: {session}"),
                );
        }
        _ => return Err("unknown_agent".into()),
    }
    Ok(c)
}
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
async fn read_bounded(stream: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    stream
        .take(LIMIT + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| "cli_output_failed")?;
    if output.len() as u64 > LIMIT {
        return Err("output_limit".into());
    }
    Ok(output)
}
async fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("/bin/kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}
async fn capture(
    mut command: tokio::process::Command,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<std::process::Output, String> {
    if *cancel.borrow() {
        return Err("cancelled".into());
    }
    #[cfg(unix)]
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().map_err(|_| "cli_spawn_failed")?;
    let _group = ProcessGroup(child.id());
    let stdout = child.stdout.take().ok_or("cli_stdout_missing")?;
    let stderr = child.stderr.take().ok_or("cli_stderr_missing")?;
    let collect = async {
        let (out, err) = tokio::try_join!(read_bounded(stdout), read_bounded(stderr))?;
        Ok((out, err))
    };
    let output = tokio::select! {
        _=cancel.changed()=>Err("cancelled".into()),
        _=tokio::time::sleep_until(deadline)=>Err("timed_out".into()),
        out=collect=>out,
    };
    let output = match output {
        Ok(out) => out,
        Err(reason) => {
            kill_group(&mut child).await;
            return Err(reason);
        }
    };
    let status = tokio::select! { _=cancel.changed()=>{kill_group(&mut child).await;return Err("cancelled".into());}, _=tokio::time::sleep_until(deadline)=>{kill_group(&mut child).await;return Err("timed_out".into());}, s=child.wait()=>s.map_err(|_|"cli_wait_failed")? };
    Ok(std::process::Output {
        status,
        stdout: output.0,
        stderr: output.1,
    })
}
pub(super) async fn execute(
    command: tokio::process::Command,
    cancel: tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, String> {
    let output = capture(command, cancel, deadline).await?;
    if !output.status.success() {
        return Err("cli_failed".into());
    }
    Ok(output.stdout)
}
struct CancelProbeOnDrop(tokio::sync::watch::Sender<bool>);
impl Drop for CancelProbeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}
/// The runner retains activity until cancellation has killed and reaped the process group.
pub(crate) async fn capture_for_probe(
    command: tokio::process::Command,
    activity: crate::state::UpdateActivityLease,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let _cancel = CancelProbeOnDrop(sender);
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::spawn(async move {
        let _activity = activity;
        capture(command, receiver, deadline).await
    })
    .await
    .map_err(|_| "cli_runner_failed".to_string())?
}
pub(super) async fn run(
    state: &GatewayState,
    job_id: &str,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Evidence, String> {
    let job = get(state, job_id).ok_or("job_missing")?;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    if *cancel.borrow() {
        return Err("cancelled".into());
    }
    let d = discover(state, &job.agent_id).await?;
    if *cancel.borrow() {
        return Err("cancelled".into());
    }
    if d.status != "ready" {
        return Err(d.status);
    }
    let initial = discovery::identity(state, &job.agent_id)?;
    let private = private_dir(job_id)?;
    let answer = nonce()?;
    let work = if job.agent_id == "gemini" {
        agents::probe_config::gemini_layout(&private.0)
            .map_err(|_| "private_directory_failed")?
            .work
    } else {
        private.0.clone()
    };
    let file = work.join("challenge.txt");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write;
        opts.open(&file)
            .and_then(|mut f| f.write_all(answer.as_bytes()))
            .map_err(|_| "private_file_failed")?;
    }
    let session = session_id()?;
    let program = agents::readiness::resolve_program(
        &job.agent_id,
        agents::readiness::program_name(&job.agent_id).ok_or("unknown_agent")?,
    )?;
    let command = configure(state, &job.agent_id, &program, &private.0, &session)?;
    let output = execute(command, cancel.clone(), deadline).await?;
    let parsed = parser::parse(&job.agent_id, &String::from_utf8_lossy(&output), &answer);
    if !(parsed.tool_started
        && parsed.tool_result_matched
        && parsed.final_exact
        && parsed.successful)
    {
        return Err("file_task_unverified".into());
    }
    let native_session = parsed.session.ok_or("session_unverified")?;
    loop {
        if *cancel.borrow() {
            return Err("cancelled".into());
        }
        if discovery::identity(state, &job.agent_id)? != initial {
            return Err("identity_changed".into());
        }
        let mut evidence_session = native_session.clone();
        let mut requests = state.validation_session_evidence(&job.agent_id, &evidence_session)?;
        if requests.is_empty() && job.agent_id == "opencode" {
            evidence_session = session.clone();
            requests = state.validation_session_evidence(&job.agent_id, &evidence_session)?;
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
        tokio::select! { _=cancel.changed()=>return Err("cancelled".into()), _=tokio::time::sleep_until(deadline)=>return Err("timed_out".into()), _=tokio::time::sleep(Duration::from_millis(200))=>{} }
    }
}
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn gemini_private_file_command_keeps_cwd_under_isolated_home() {
        let root = private_dir(&nonce().unwrap()).unwrap();
        let repository = root.0.join("repository");
        let job = repository.join("jobs/probe");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(repository.join(".env"), "UNRELATED_FIXTURE=yes\n").unwrap();
        let source = root.0.join("managed/.gemini");
        let paths = agents::AgentsPaths {
            claude_settings: root.0.join("claude/settings.json"),
            codex_config: root.0.join("codex/config.toml"),
            gemini_settings: source.join("settings.json"),
            gemini_env: source.join(".env"),
            opencode_config: root.0.join("opencode.json"),
            state_dir: root.0.join("managed-state"),
        };
        let managed = agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            3427,
        );
        managed.apply().unwrap();
        managed.set_model("gpt-6.1-sol").unwrap();
        let state = GatewayState::from_manifests(std::collections::BTreeMap::new(), "fixture")
            .with_agents(paths);
        let command = configure(
            &state,
            "gemini",
            Path::new("/fixture/gemini"),
            &job,
            "session-fixture",
        )
        .unwrap();
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(job.join("home/work").as_path())
        );
        let home = job.join("home");
        for name in ["HOME", "GEMINI_CLI_HOME"] {
            assert!(command
                .as_std()
                .get_envs()
                .any(|(key, value)| key == name && value == Some(home.as_os_str())));
        }
    }
    fn shell(script: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    }
    #[tokio::test]
    async fn cancellation_reaps_descendant_and_releases_next_execution() {
        let dir = private_dir(&nonce().unwrap()).unwrap();
        let pid_file = dir.0.join("descendant.pid");
        let mut command = shell("sleep 30 & printf '%s' \"$!\" > \"$1\"; wait");
        command.arg("probe").arg(&pid_file);
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(execute(
            command,
            receiver,
            tokio::time::Instant::now() + Duration::from_secs(5),
        ));
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sender.send(true).unwrap();
        assert_eq!(handle.await.unwrap().unwrap_err(), "cancelled");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let output = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                let status = String::from_utf8_lossy(&output.stdout);
                if status.trim().is_empty() || status.trim().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant remained running after cancellation");
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        assert_eq!(
            execute(
                shell("printf ok"),
                receiver,
                tokio::time::Instant::now() + Duration::from_secs(3)
            )
            .await
            .unwrap(),
            b"ok"
        );
    }
    #[tokio::test]
    async fn output_overflow_stops_a_process_before_its_deadline() {
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let result = execute(
            shell("yes overflow"),
            receiver,
            tokio::time::Instant::now() + Duration::from_secs(3),
        )
        .await;
        assert_eq!(result.unwrap_err(), "output_limit");
    }
    #[tokio::test]
    async fn deadline_stops_process_with_inherited_open_pipes() {
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let result = execute(
            shell("sleep 30 & wait"),
            receiver,
            tokio::time::Instant::now() + Duration::from_millis(50),
        )
        .await;
        assert_eq!(result.unwrap_err(), "timed_out");
    }
    async fn wait_pid(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(path) {
                    if let Ok(pid) = text.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn assert_descendant_stopped(pid: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let output = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                let status = String::from_utf8_lossy(&output.stdout);
                if status.trim().is_empty() || status.trim().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("probe descendant remained alive");
    }
    #[tokio::test]
    async fn legacy_probe_request_cancellation_retains_activity_until_group_cleanup() {
        let state = std::sync::Arc::new(GatewayState::from_manifests(
            std::collections::BTreeMap::new(),
            "test",
        ));
        let dir = private_dir(&nonce().unwrap()).unwrap();
        let path = dir.0.join("probe.pid");
        let mut command = shell("sleep 30 & printf '%s' \"$!\" > \"$1\"; wait");
        command.arg("probe").arg(&path);
        let activity = state.begin_update_activity().unwrap();
        let request = tokio::spawn(capture_for_probe(command, activity, Duration::from_secs(5)));
        let pid = wait_pid(&path).await;
        assert!(state.try_update_maintenance().is_err());
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(maintenance) = state.try_update_maintenance() {
                    drop(maintenance);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_descendant_stopped(pid).await;
        let output = capture_for_probe(
            shell("printf ok"),
            state.begin_update_activity().unwrap(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
    }
    #[tokio::test]
    async fn legacy_probe_timeout_reaps_descendant_before_releasing_activity() {
        let state = GatewayState::from_manifests(std::collections::BTreeMap::new(), "test");
        let dir = private_dir(&nonce().unwrap()).unwrap();
        let path = dir.0.join("probe.pid");
        let mut command = shell("sleep 30 & printf '%s' \"$!\" > \"$1\"; wait");
        command.arg("probe").arg(&path);
        let request = tokio::spawn(capture_for_probe(
            command,
            state.begin_update_activity().unwrap(),
            Duration::from_secs(3),
        ));
        let pid = wait_pid(&path).await;
        assert!(state.try_update_maintenance().is_err());
        assert_eq!(request.await.unwrap().unwrap_err(), "timed_out");
        assert_descendant_stopped(pid).await;
        assert!(state.try_update_maintenance().is_ok());
    }
}

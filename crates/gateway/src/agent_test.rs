//! Fixed non-interactive probes for wired agents.
//!
//! The prompt and arguments are constants: the admin API cannot inject
//! flags, a shell, or arbitrary working directories. Each probe runs in
//! a throwaway directory, has a hard timeout, and redacts configured
//! credentials from its output.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::process::Command;

use crate::state::GatewayState;

const PROMPT: &str = "只回复两个字：收到";
const TIMEOUT: Duration = Duration::from_secs(150);
const MAX_OUTPUT: usize = 16_000;

static TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

const CLAUDE_ARGS: &[&str] = &[
    "-p",
    "--strict-mcp-config",
    "--no-session-persistence",
    "--output-format",
    "text",
    PROMPT,
];
const CODEX_ARGS: &[&str] = &[
    "--profile",
    "aubridge",
    "exec",
    "--skip-git-repo-check",
    "--ephemeral",
    "--sandbox",
    "read-only",
    "--color",
    "never",
    PROMPT,
];
pub(crate) const CLAUDE_CLEAR_ENV: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

#[derive(Debug, Serialize)]
pub struct AgentTestResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub message: String,
}

pub(crate) async fn run(state: &GatewayState, id: &str) -> Result<AgentTestResult, String> {
    let _activity = state.begin_update_activity().map_err(str::to_owned)?;
    let paths = state
        .agents_paths()
        .ok_or_else(|| "agent 接线未配置".to_string())?;
    let (name, args, clear_env) = match id {
        "claude-code" => ("claude", CLAUDE_ARGS, CLAUDE_CLEAR_ENV),
        "codex" => ("codex", CODEX_ARGS, &[][..]),
        "opencode" => ("opencode", &[][..], &[][..]),
        "gemini" => ("gemini", &[][..], &[][..]),
        _ => return Err(format!("Agent {id} 暂不支持测试")),
    };
    if !wired(id, paths, state.port())? {
        return Err(format!("{name} 尚未接线；测试只验证已指向 aubridge 的配置"));
    }
    let program = resolve_program(id, name)?;
    let _guard = TEST_LOCK
        .try_lock()
        .map_err(|_| "已有 Agent 测试正在运行，请稍后重试".to_string())?;
    let secrets = secret_values(id, paths);
    let dir = temp_dir();
    let mut work = dir.clone();
    let _cleanup = TempCleanup(dir.clone());
    let mut command = Command::new(&program);
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建测试目录失败: {error}"))?;
    if id == "gemini" {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("保护测试目录失败: {error}"))?;
        }
        let mut child_path = Vec::new();
        if let Some(parent) = program.parent() {
            child_path.push(parent.to_path_buf());
        }
        if let Some(home) = std::env::var_os("HOME") {
            child_path.push(PathBuf::from(home).join(".local/node/bin"));
        }
        if let Some(path) = std::env::var_os("PATH") {
            child_path.extend(std::env::split_paths(&path));
        }
        command.env(
            "PATH",
            std::env::join_paths(child_path)
                .map_err(|error| format!("配置 Gemini PATH 失败: {error}"))?,
        );
        let layout = agents::probe_config::gemini_layout(&dir)
            .map_err(|error| format!("创建隔离 Gemini 目录失败: {error}"))?;
        let home = layout.home;
        work = layout.work;
        let config = home.join(".gemini");
        let model = agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            state.port(),
        )
        .current_model()
        .ok_or("Gemini 受管模型缺失")?;
        agents::gemini::GeminiCli::new(
            config.join("settings.json"),
            config.join(".env"),
            dir.join("wiring"),
            state.port(),
        )
        .apply()
        .map_err(|error| format!("创建隔离 Gemini 配置失败: {error}"))?;
        agents::gemini::GeminiCli::new(
            config.join("settings.json"),
            config.join(".env"),
            dir.join("wiring"),
            state.port(),
        )
        .set_model(&model)
        .map_err(|error| format!("设置隔离 Gemini 模型失败: {error}"))?;
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if ["GEMINI", "GOOGLE", "VERTEX", "GCLOUD"]
                .iter()
                .any(|token| upper.contains(token))
            {
                command.env_remove(key);
            }
        }
        command
            .args([
                "--prompt",
                PROMPT,
                "--output-format",
                "stream-json",
                "--approval-mode",
                "default",
            ])
            .env("HOME", &home)
            .env("GEMINI_CLI_HOME", &home)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("TMPDIR", &dir)
            .env("CI", "true")
            .env("GEMINI_CLI_TRUST_WORKSPACE", "true")
            .env(
                "GOOGLE_GEMINI_BASE_URL",
                format!("http://127.0.0.1:{}", state.port()),
            )
            .env("GEMINI_API_KEY", "aubridge-gemini");
    }

    if id == "opencode" {
        let mut version_command = Command::new(&program);
        version_command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let version = crate::agent_validation::capture_for_probe(
            version_command,
            state.begin_update_activity().map_err(str::to_owned)?,
            Duration::from_secs(5),
        )
        .await
        .map_err(|reason| {
            if reason == "timed_out" {
                "读取 OpenCode 版本超时"
            } else {
                "无法读取 OpenCode 版本"
            }
        })?;
        let version_text = String::from_utf8_lossy(&version.stdout);
        let major = version_text
            .split_whitespace()
            .find_map(|value| {
                value
                    .trim_start_matches('v')
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .filter(|_| version.status.success())
            .ok_or("OpenCode 未返回可识别的版本")?;
        if major >= 2 {
            command.arg("--standalone");
        }
        let model = agents::opencode::OpenCode::new(
            paths.opencode_config.clone(),
            paths.state_dir.clone(),
            state.port(),
        )
        .current_model()
        .unwrap_or_else(|| crate::AUTO_MODEL.into());
        command.args([
            "run",
            "--format",
            "json",
            "--model",
            &format!("aubridge/{model}"),
            PROMPT,
        ]);
        command
            .env("OPENCODE_CONFIG", &paths.opencode_config)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CACHE_HOME", dir.join("cache"));
    }
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建测试目录失败: {error}"))?;
    command
        .args(args)
        .current_dir(&work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    for key in clear_env {
        command.env_remove(key);
    }
    let started = Instant::now();
    let output = crate::agent_validation::capture_for_probe(command, _activity, TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&dir);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match output {
        Ok(output) => {
            let raw_stdout = String::from_utf8_lossy(&output.stdout);
            let gemini = (id == "gemini").then(|| gemini_output(&raw_stdout));
            let stdout = redact(
                &ansi_free(
                    gemini
                        .as_ref()
                        .map(|(text, _)| text.as_str())
                        .unwrap_or(&raw_stdout),
                ),
                &secrets,
            );
            let stderr = redact(
                &ansi_free(&String::from_utf8_lossy(&output.stderr)),
                &secrets,
            );
            let acknowledged =
                short_probe_acknowledged(id, &String::from_utf8_lossy(&output.stdout));
            let exit_code = output.status.code();
            let ok = exit_code == Some(0) && !stdout.trim().is_empty() && acknowledged;
            let message = if ok {
                if acknowledged {
                    "测试通过".to_string()
                } else {
                    "CLI 已返回，但输出未包含“收到”；请检查上方输出".to_string()
                }
            } else if matches!(id, "opencode" | "gemini") && exit_code == Some(0) {
                "CLI 已结束，但未通过预期回复与错误检查；请检查输出".into()
            } else if let Some(code) = exit_code {
                format!("测试失败（退出码 {code}）")
            } else {
                "测试失败（进程被信号终止）".to_string()
            };
            Ok(AgentTestResult {
                ok,
                exit_code,
                timed_out: false,
                duration_ms,
                stdout,
                stderr,
                message,
            })
        }
        Err(error) if error != "timed_out" => Ok(AgentTestResult {
            ok: false,
            exit_code: None,
            timed_out: false,
            duration_ms,
            stdout: String::new(),
            stderr: String::new(),
            message: format!("启动测试失败: {error}"),
        }),
        Err(_) => Ok(AgentTestResult {
            ok: false,
            exit_code: None,
            timed_out: true,
            duration_ms,
            stdout: String::new(),
            stderr: String::new(),
            message: format!("测试超时（{} 秒），已终止", TIMEOUT.as_secs()),
        }),
    }
}

fn wired(id: &str, paths: &agents::AgentsPaths, port: u16) -> Result<bool, String> {
    match id {
        "claude-code" => Ok(agents::ClaudeCode::new(
            paths.claude_settings.clone(),
            paths.state_dir.clone(),
            port,
        )
        .wired()),
        "codex" => Ok(agents::codex::Codex::new(
            paths.codex_config.clone(),
            paths.state_dir.clone(),
            port,
        )
        .wired()),
        "opencode" => Ok(agents::opencode::OpenCode::new(
            paths.opencode_config.clone(),
            paths.state_dir.clone(),
            port,
        )
        .wired()),
        "gemini" => Ok(agents::gemini::GeminiCli::new(
            paths.gemini_settings.clone(),
            paths.gemini_env.clone(),
            paths.state_dir.clone(),
            port,
        )
        .wired()),
        _ => Err(format!("Agent {id} 暂不支持测试")),
    }
}

pub(crate) fn resolve_program(id: &str, name: &str) -> Result<PathBuf, String> {
    agents::readiness::resolve_program(id, name)
}

fn short_probe_acknowledged(id: &str, stdout: &str) -> bool {
    match id {
        "opencode" => opencode_acknowledged(stdout),
        "gemini" => gemini_output(stdout).1,
        _ => ansi_free(stdout).trim() == "收到",
    }
}

fn opencode_acknowledged(stdout: &str) -> bool {
    let mut acknowledged = false;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event["type"] == "error" {
            return false;
        }
        acknowledged |= event["type"] == "text"
            && event["part"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("收到"));
    }
    acknowledged
}

fn gemini_output(stdout: &str) -> (String, bool) {
    let mut text = String::new();
    let mut success = false;
    let mut failed = false;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        match event["type"].as_str() {
            Some("message") if event["role"] == "assistant" => {
                if let Some(content) = event["content"].as_str() {
                    text.push_str(content);
                }
            }
            Some("result") => {
                success = event["status"] == "success";
            }
            Some("error") => {
                failed = true;
            }
            _ => {}
        }
    }
    let acknowledged = success && !failed && text.trim() == "收到";
    (text, acknowledged)
}

struct TempCleanup(PathBuf);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "aubridge-agent-test-{}-{nanos}",
        std::process::id()
    ))
}

fn secret_values(id: &str, paths: &agents::AgentsPaths) -> Vec<String> {
    match id {
        "claude-code" => {
            let Ok(text) = std::fs::read_to_string(&paths.claude_settings) else {
                return Vec::new();
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                return Vec::new();
            };
            value["env"]
                .as_object()
                .map(|env| {
                    env.iter()
                        .filter(|(key, _)| is_secret_key(key))
                        .filter_map(|(_, value)| value.as_str())
                        .filter(|value| value.len() >= 4)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        }
        "codex" => vec!["aubridge-codex".to_string()],
        "opencode" => vec!["aubridge-opencode".to_string()],
        "gemini" => vec!["aubridge-gemini".to_string()],
        _ => Vec::new(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|needle| key.contains(needle))
}

fn redact(text: &str, secrets: &[String]) -> String {
    let mut text: String = text.chars().take(MAX_OUTPUT).collect();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "[已隐藏]");
        }
    }
    text
}

fn ansi_free(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for character in chars.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_secrets_are_removed() {
        let output = "\u{1b}[31msecret-value\u{1b}[0m done";
        assert_eq!(
            redact(&ansi_free(output), &["secret-value".into()]),
            "[已隐藏] done"
        );
    }

    #[test]
    fn candidates_include_the_plain_name() {
        assert_eq!(
            agents::readiness::candidates(std::path::Path::new("/tmp"), "claude"),
            vec![PathBuf::from("/tmp/claude")]
        );
    }
    #[test]
    fn short_probe_rejects_plain_text_without_exact_acknowledgement() {
        assert!(!short_probe_acknowledged(
            "claude-code",
            "not the requested reply"
        ));
        assert!(!short_probe_acknowledged("codex", "收到 other text"));
        assert!(short_probe_acknowledged("claude-code", "收到\n"));
    }
    #[test]
    fn gemini_probe_requires_exact_final_and_success_event() {
        let good = "{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"收到\"}\n{\"type\":\"result\",\"status\":\"success\"}\n";
        assert_eq!(gemini_output(good), ("收到".into(), true));
        assert!(!gemini_output("收到").1);
        assert!(!gemini_output(&good.replace("success", "error")).1);
        assert!(!gemini_output(&good.replace("收到", "收到其他")).1);
        assert!(!gemini_output(&good.replace("assistant", "user")).1);
        assert!(!gemini_output(&(good.to_owned() + "{\"type\":\"error\"}\n")).1);
    }
}

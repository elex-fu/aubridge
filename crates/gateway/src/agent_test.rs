//! Fixed non-interactive probes for wired agents.
//!
//! The prompt and arguments are constants: the admin API cannot inject
//! flags, a shell, or arbitrary working directories. Each probe runs in
//! a throwaway directory, has a hard timeout, and redacts configured
//! credentials from its output.

use std::path::{Path, PathBuf};
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
const CLAUDE_CLEAR_ENV: &[&str] = &[
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
    let paths = state
        .agents_paths()
        .ok_or_else(|| "agent 接线未配置".to_string())?;
    let (name, args, clear_env) = match id {
        "claude-code" => ("claude", CLAUDE_ARGS, CLAUDE_CLEAR_ENV),
        "codex" => ("codex", CODEX_ARGS, &[][..]),
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
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建测试目录失败: {error}"))?;
    let mut command = Command::new(&program);
    command
        .args(args)
        .current_dir(&dir)
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
    let output = tokio::time::timeout(TIMEOUT, command.output()).await;
    let _ = std::fs::remove_dir_all(&dir);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match output {
        Ok(Ok(output)) => {
            let stdout = redact(
                &ansi_free(&String::from_utf8_lossy(&output.stdout)),
                &secrets,
            );
            let stderr = redact(
                &ansi_free(&String::from_utf8_lossy(&output.stderr)),
                &secrets,
            );
            let exit_code = output.status.code();
            let ok = exit_code == Some(0) && !stdout.trim().is_empty();
            let message = if ok {
                if stdout.contains("收到") {
                    "测试通过".to_string()
                } else {
                    "CLI 已返回，但输出未包含“收到”；请检查上方输出".to_string()
                }
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
        Ok(Err(error)) => Ok(AgentTestResult {
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
        _ => Err(format!("Agent {id} 暂不支持测试")),
    }
}

fn resolve_program(id: &str, name: &str) -> Result<PathBuf, String> {
    let env_key = match id {
        "claude-code" => "AUBRIDGE_CLAUDE_BIN",
        "codex" => "AUBRIDGE_CODEX_BIN",
        _ => return Err(format!("Agent {id} 暂不支持测试")),
    };
    if let Some(value) = std::env::var_os(env_key) {
        let path = PathBuf::from(value);
        if executable(&path) {
            return Ok(path);
        }
        return Err(format!(
            "{env_key} 指向的 {name} 不可执行: {}",
            path.display()
        ));
    }
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".cargo/bin",
            ".volta/bin",
            "n/bin",
            ".claude/local",
        ] {
            dirs.push(home.join(sub));
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"] {
        dirs.push(PathBuf::from(dir));
    }
    for dir in dirs {
        for candidate in candidates(&dir, name) {
            if executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "未找到 {name} 命令。请先安装并确保它在 PATH 中，或设置 {env_key} 指向可执行文件。"
    ))
}

fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut paths = vec![dir.join(name)];
    if cfg!(windows) {
        paths.push(dir.join(format!("{name}.exe")));
    }
    paths
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
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
            candidates(Path::new("/tmp"), "claude"),
            vec![PathBuf::from("/tmp/claude")]
        );
    }
}

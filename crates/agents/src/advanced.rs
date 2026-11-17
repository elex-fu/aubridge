//! Agent 高级配置读写(Claude Code settings.json / Codex 受管 profile)。
//!
//! 只暴露 agent 自己的配置文件内容,不触碰网关设置。两端都用
//! revision 防止 UI 持有旧快照覆盖外部编辑;Claude 的密钥环境变量
//! 读取时打码,保存时原样回填。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::codex::{Codex, CodexConfigFields};
use crate::{content_revision, read_optional_text, AgentsPaths, ClaudeCode};

const SECRET_MASK: &str = "••••••••";
const ROLES: [(&str, &str); 4] = [
    ("sonnet", "Sonnet"),
    ("opus", "Opus"),
    ("haiku", "Haiku"),
    ("fable", "Fable"),
];
const TEST_PROMPT: &str = "只回复两个字：收到";

#[derive(Debug, Serialize)]
pub struct AgentConfigView {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    pub detected: bool,
    pub wired: bool,
    pub path: String,
    pub revision: String,
    pub config: Value,
    pub roles: Option<Value>,
    pub text: Option<String>,
    pub secrets_masked: bool,
    pub test: TestInfo,
}

#[derive(Debug, Serialize)]
pub struct TestInfo {
    pub supported: bool,
    pub command: Option<String>,
    pub prompt: Option<String>,
    pub note: Option<String>,
}

#[derive(Deserialize)]
struct WriteRequest {
    revision: String,
    config: Value,
}

pub fn read_config(paths: &AgentsPaths, port: u16, id: &str) -> Result<AgentConfigView, String> {
    match id {
        "claude-code" => read_claude(paths, port),
        "codex" => read_codex(paths, port),
        _ => Err(format!("Agent {id} 暂不支持高级配置")),
    }
}

pub fn write_config(
    paths: &AgentsPaths,
    port: u16,
    id: &str,
    body: &str,
) -> Result<AgentConfigView, String> {
    let request: WriteRequest =
        serde_json::from_str(body).map_err(|error| format!("高级配置请求格式无效: {error}"))?;
    match id {
        "claude-code" => write_claude(paths, port, request),
        "codex" => write_codex(paths, port, request),
        _ => Err(format!("Agent {id} 暂不支持高级配置")),
    }
}

/// 固定测试命令(UI 只展示,不接受前端拼接参数)。
pub fn test_command(id: &str) -> Option<TestInfo> {
    match id {
        "claude-code" => Some(TestInfo {
            supported: true,
            command: Some(format!(
                "claude -p --strict-mcp-config --no-session-persistence --output-format text \"{TEST_PROMPT}\""
            )),
            prompt: Some(TEST_PROMPT.into()),
            note: Some("使用当前 Claude Code 配置发起一次非交互请求，不创建持久会话。".into()),
        }),
        "codex" => Some(TestInfo {
            supported: true,
            command: Some(format!(
                "codex --profile aubridge exec --skip-git-repo-check --ephemeral --sandbox read-only --color never \"{TEST_PROMPT}\""
            )),
            prompt: Some(TEST_PROMPT.into()),
            note: Some("使用 aubridge profile 发起一次非交互请求，不持久化会话。".into()),
        }),
        _ => None,
    }
}

fn read_claude(paths: &AgentsPaths, port: u16) -> Result<AgentConfigView, String> {
    let path = &paths.claude_settings;
    let raw = read_optional_text(path).map_err(io_error)?;
    let config = match &raw {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| format!("Claude Code 配置不是有效 JSON: {error}"))?,
        None => json!({}),
    };
    if !config.is_object() {
        return Err("Claude Code 配置根节点必须是 JSON 对象".into());
    }
    let claude = ClaudeCode::new(paths.claude_settings.clone(), paths.state_dir.clone(), port);
    Ok(AgentConfigView {
        id: "claude-code",
        name: "Claude Code",
        kind: "claude-code",
        detected: path.exists(),
        wired: claude.wired(),
        path: path.display().to_string(),
        revision: content_revision(raw.as_deref()),
        config: mask_settings(config),
        roles: Some(roles_from_settings(&raw)?),
        text: None,
        secrets_masked: true,
        test: test_command("claude-code").expect("claude test info"),
    })
}

fn write_claude(
    paths: &AgentsPaths,
    port: u16,
    request: WriteRequest,
) -> Result<AgentConfigView, String> {
    let path = &paths.claude_settings;
    let raw = read_optional_text(path).map_err(io_error)?;
    if content_revision(raw.as_deref()) != request.revision {
        return Err("Claude Code 配置已在其他地方修改，请重新加载后再保存".into());
    }
    let current = match &raw {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| format!("Claude Code 配置不是有效 JSON: {error}"))?,
        None => json!({}),
    };
    let mut next = request.config;
    if !next.is_object() {
        return Err("Claude Code 配置根节点必须是 JSON 对象".into());
    }
    restore_masked(&mut next, &current)?;
    restore_masked_headers(&mut next, &current)?;
    validate_claude(&next)?;
    crate::atomic_write_json(path, &next).map_err(io_error)?;
    read_claude(paths, port)
}

fn read_codex(paths: &AgentsPaths, port: u16) -> Result<AgentConfigView, String> {
    let codex = Codex::new(paths.codex_config.clone(), paths.state_dir.clone(), port);
    let text = codex.profile_text().map_err(io_error)?;
    let config = match codex.profile_fields().map_err(io_error)? {
        Some(fields) => serde_json::to_value(fields).map_err(|error| error.to_string())?,
        None => Value::Null,
    };
    Ok(AgentConfigView {
        id: "codex",
        name: "Codex CLI",
        kind: "codex",
        detected: paths.codex_config.exists() || text.is_some(),
        wired: codex.wired(),
        path: paths
            .codex_config
            .with_file_name("aubridge.config.toml")
            .display()
            .to_string(),
        revision: content_revision(text.as_deref()),
        config,
        roles: None,
        text,
        secrets_masked: false,
        test: test_command("codex").expect("codex test info"),
    })
}

fn write_codex(
    paths: &AgentsPaths,
    port: u16,
    request: WriteRequest,
) -> Result<AgentConfigView, String> {
    let codex = Codex::new(paths.codex_config.clone(), paths.state_dir.clone(), port);
    let fields: CodexConfigFields = serde_json::from_value(request.config)
        .map_err(|error| format!("Codex 高级配置格式无效: {error}"))?;
    codex
        .update_fields(&request.revision, fields)
        .map_err(io_error)?;
    read_codex(paths, port)
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

fn roles_from_settings(raw: &Option<String>) -> Result<Value, String> {
    let config: Value = match raw {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| format!("Claude Code 配置不是有效 JSON: {error}"))?,
        None => json!({}),
    };
    let env = config.get("env").and_then(Value::as_object);
    let mut roles = Map::new();
    for (id, label) in ROLES {
        let prefix = format!("ANTHROPIC_DEFAULT_{}_MODEL", id.to_ascii_uppercase());
        let model = env
            .and_then(|env| env.get(&prefix))
            .and_then(Value::as_str)
            .map(str::to_string);
        let name = env
            .and_then(|env| env.get(&format!("{prefix}_NAME")))
            .and_then(Value::as_str)
            .map(str::to_string);
        roles.insert(
            id.into(),
            json!({
                "label": label,
                "model": model,
                "name": name,
                "one_million": model.as_deref().is_some_and(has_one_million_suffix),
            }),
        );
    }
    Ok(Value::Object(roles))
}

fn has_one_million_suffix(model: &str) -> bool {
    model.to_ascii_lowercase().ends_with("[1m]")
}

fn mask_settings(mut config: Value) -> Value {
    if let Some(env) = config.get_mut("env").and_then(Value::as_object_mut) {
        for (key, value) in env.iter_mut() {
            if key == "ANTHROPIC_CUSTOM_HEADERS" {
                if let Some(text) = value.as_str() {
                    *value = json!(mask_headers(text));
                }
            } else if is_secret_key(key) && value.as_str().is_some_and(|value| !value.is_empty()) {
                *value = json!(SECRET_MASK);
            }
        }
    }
    config
}

/// 自定义请求头里也可能带凭据(Authorization / x-api-key 等)。
/// 只打码敏感头,其余原样展示;保存时按头名回填原值。
fn mask_headers(text: &str) -> String {
    text.lines()
        .map(|line| match line.split_once(':') {
            Some((name, _)) if is_secret_key(name.trim()) => {
                format!("{}: {SECRET_MASK}", name.trim_end())
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn restore_masked_headers(next: &mut Value, current: &Value) -> Result<(), String> {
    let Some(next_headers) = next
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get("ANTHROPIC_CUSTOM_HEADERS"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    if !next_headers.contains(SECRET_MASK) {
        return Ok(());
    }
    let current_headers = current
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_CUSTOM_HEADERS"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut restored = Vec::new();
    for line in next_headers.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if is_secret_key(name.trim()) && value.trim() == SECRET_MASK {
                let existing = current_headers.lines().find_map(|old| {
                    let (old_name, old_value) = old.split_once(':')?;
                    old_name
                        .trim()
                        .eq_ignore_ascii_case(name.trim())
                        .then(|| old_value.trim().to_string())
                });
                match existing {
                    Some(secret) if !secret.is_empty() => {
                        restored.push(format!("{}: {secret}", name.trim_end()));
                        continue;
                    }
                    _ => {
                        return Err(format!(
                            "自定义请求头 {} 是密钥占位符，请填写实际值",
                            name.trim()
                        ))
                    }
                }
            }
        }
        restored.push(line.to_string());
    }
    next["env"]["ANTHROPIC_CUSTOM_HEADERS"] = json!(restored.join("\n"));
    Ok(())
}

fn restore_masked(next: &mut Value, current: &Value) -> Result<(), String> {
    let Some(env) = next.get_mut("env").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let old = current.get("env").and_then(Value::as_object);
    for (key, value) in env.iter_mut() {
        if value.as_str() != Some(SECRET_MASK) {
            continue;
        }
        match old.and_then(|env| env.get(key)).and_then(Value::as_str) {
            Some(secret) if !secret.is_empty() => *value = json!(secret),
            _ => return Err(format!("环境变量 {key} 是密钥占位符，请填写实际值")),
        }
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "AUTHORIZATION",
        "COOKIE",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn validate_claude(config: &Value) -> Result<(), String> {
    let object = config
        .as_object()
        .ok_or_else(|| "Claude Code 配置根节点必须是 JSON 对象".to_string())?;
    if let Some(env) = object.get("env") {
        let env = env
            .as_object()
            .ok_or_else(|| "env 必须是 JSON 对象".to_string())?;
        for (key, value) in env {
            if !value.is_string() {
                return Err(format!("环境变量 {key} 的值必须是字符串"));
            }
        }
        if let Some(body) = env.get("CLAUDE_CODE_EXTRA_BODY").and_then(Value::as_str) {
            if !body.trim().is_empty() {
                let parsed: Value = serde_json::from_str(body)
                    .map_err(|error| format!("CLAUDE_CODE_EXTRA_BODY 不是有效 JSON: {error}"))?;
                if !parsed.is_object() {
                    return Err("CLAUDE_CODE_EXTRA_BODY 必须是 JSON 对象".into());
                }
            }
        }
        if let Some(headers) = env.get("ANTHROPIC_CUSTOM_HEADERS").and_then(Value::as_str) {
            for (index, line) in headers.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Some((name, value)) = line.split_once(':') else {
                    return Err(format!(
                        "ANTHROPIC_CUSTOM_HEADERS 第 {} 行缺少冒号",
                        index + 1
                    ));
                };
                if name.trim().is_empty() || value.trim().is_empty() {
                    return Err(format!(
                        "ANTHROPIC_CUSTOM_HEADERS 第 {} 行的名称或值不能为空",
                        index + 1
                    ));
                }
            }
        }
    }
    if let Some(model) = object.get("model") {
        if !model.is_string() {
            return Err("model 必须是字符串".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn paths(tag: &str) -> AgentsPaths {
        let root =
            std::env::temp_dir().join(format!("aubridge-advanced-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("state")).unwrap();
        AgentsPaths {
            claude_settings: root.join("claude").join("settings.json"),
            codex_config: root.join("codex").join("config.toml"),
            opencode_config: root.join("opencode").join("opencode.json"),
            gemini_settings: root.join("gemini").join("settings.json"),
            gemini_env: root.join("gemini").join(".env"),
            state_dir: root.join("state"),
        }
    }

    fn write_json(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn claude_view_masks_secrets_and_round_trips_roles() {
        let paths = paths("claude-roles");
        write_json(
            &paths.claude_settings,
            &json!({
                "model": "glm-5.3[1M]",
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "secret-value",
                    "ANTHROPIC_API_KEY": "sk-secret",
                    "ANTHROPIC_CUSTOM_HEADERS": "X-Trace: keep\nAuthorization: Bearer header-secret\nUser-Agent: aubridge-test",
                    "CLAUDE_CODE_EXTRA_BODY": "{\"reasoning\":{\"effort\":\"high\"}}",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.3[1M]",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "glm-5.3",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.3",
                },
            }),
        );

        let view = read_config(&paths, 62760, "claude-code").unwrap();
        assert!(view.secrets_masked);
        assert_eq!(view.config["env"]["ANTHROPIC_AUTH_TOKEN"], SECRET_MASK);
        assert_eq!(view.config["env"]["ANTHROPIC_API_KEY"], SECRET_MASK);
        assert_eq!(
            view.config["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "X-Trace: keep\nAuthorization: ••••••••\nUser-Agent: aubridge-test"
        );
        assert_eq!(view.roles.as_ref().unwrap()["sonnet"]["one_million"], true);
        assert_eq!(view.roles.as_ref().unwrap()["opus"]["one_million"], false);

        let mut config = view.config.clone();
        config["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"] = json!("next-model");
        config["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"] = json!("next-model");
        config["env"]["ANTHROPIC_CUSTOM_HEADERS"] =
            json!("X-Trace: keep\nAuthorization: ••••••••\nUser-Agent: next-agent");
        let next = write_config(
            &paths,
            62760,
            "claude-code",
            &json!({ "revision": view.revision, "config": config }).to_string(),
        )
        .unwrap();
        assert_eq!(next.config["env"]["ANTHROPIC_AUTH_TOKEN"], SECRET_MASK);
        assert_eq!(
            next.config["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "X-Trace: keep\nAuthorization: ••••••••\nUser-Agent: next-agent"
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(&paths.claude_settings).unwrap()).unwrap();
        assert_eq!(saved["env"]["ANTHROPIC_AUTH_TOKEN"], "secret-value");
        assert_eq!(
            saved["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "X-Trace: keep\nAuthorization: Bearer header-secret\nUser-Agent: next-agent"
        );
        assert_eq!(saved["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "next-model");
        assert_eq!(saved["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-5.3");
    }

    #[test]
    fn claude_save_rejects_stale_revision_and_non_string_env() {
        let paths = paths("claude-invalid");
        write_json(
            &paths.claude_settings,
            &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "secret"}}),
        );
        let view = read_config(&paths, 62760, "claude-code").unwrap();

        let stale = write_config(
            &paths,
            62760,
            "claude-code",
            &json!({ "revision": "stale", "config": {} }).to_string(),
        );
        assert!(stale.unwrap_err().contains("修改"));

        let invalid = write_config(
            &paths,
            62760,
            "claude-code",
            &json!({
                "revision": view.revision,
                "config": {"env": {"NUMBER": 1}},
            })
            .to_string(),
        );
        assert!(invalid.unwrap_err().contains("字符串"));

        let bad_body = write_config(
            &paths,
            62760,
            "claude-code",
            &json!({
                "revision": view.revision,
                "config": {"env": {"CLAUDE_CODE_EXTRA_BODY": "[1]"}},
            })
            .to_string(),
        );
        assert!(bad_body.unwrap_err().contains("JSON 对象"));

        let bad_headers = write_config(
            &paths,
            62760,
            "claude-code",
            &json!({
                "revision": view.revision,
                "config": {"env": {"ANTHROPIC_CUSTOM_HEADERS": "NoColon"}},
            })
            .to_string(),
        );
        assert!(bad_headers.unwrap_err().contains("冒号"));
    }

    #[test]
    fn codex_profile_round_trips_advanced_fields_and_keeps_model() {
        let paths = paths("codex-advanced");
        fs::create_dir_all(paths.codex_config.parent().unwrap()).unwrap();
        fs::write(&paths.codex_config, "model = \"gpt-5.1\"\n").unwrap();
        let codex = Codex::new(paths.codex_config.clone(), paths.state_dir.clone(), 62760);
        codex.apply().unwrap();

        let view = read_config(&paths, 62760, "codex").unwrap();
        assert!(view.wired);
        assert_eq!(view.config["model"], "aubridge");
        let mut config = view.config.clone();
        config["model"] = json!("gpt-6.1-sol");
        config["model_reasoning_effort"] = json!("high");
        config["model_context_window"] = json!(400000);
        let next = write_config(
            &paths,
            62760,
            "codex",
            &json!({ "revision": view.revision, "config": config }).to_string(),
        )
        .unwrap();
        assert_eq!(next.config["model"], "gpt-6.1-sol");
        assert_eq!(next.config["model_reasoning_effort"], "high");
        assert!(next.wired);
        let saved =
            fs::read_to_string(paths.codex_config.with_file_name("aubridge.config.toml")).unwrap();
        assert!(saved.contains("model_reasoning_effort = \"high\""));
        assert!(saved.contains("model_context_window = 400000"));

        codex.set_model("next-model").unwrap();
        assert_eq!(codex.current_model().as_deref(), Some("next-model"));
        let saved =
            fs::read_to_string(paths.codex_config.with_file_name("aubridge.config.toml")).unwrap();
        assert!(saved.contains("model_reasoning_effort = \"high\""));
    }

    #[test]
    fn codex_write_requires_wired_profile_and_valid_fields() {
        let paths = paths("codex-invalid");
        let error = write_config(
            &paths,
            62760,
            "codex",
            &json!({
                "revision": "missing",
                "config": {"model": "aubridge"},
            })
            .to_string(),
        )
        .unwrap_err();
        assert!(error.contains("接线"));
    }

    #[test]
    fn test_command_is_fixed_per_agent() {
        let claude = test_command("claude-code").unwrap();
        assert!(claude.command.unwrap().contains("--no-session-persistence"));
        let codex = test_command("codex").unwrap();
        assert!(codex.command.unwrap().contains("--ephemeral"));
        assert!(test_command("opencode").is_none());
    }
}

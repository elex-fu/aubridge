//! OAuth 登录导入(设计文档 §7):Claude Code / Codex 登录读取,
//! **只读**——刷新得到的 token 只存 aubridge 自己的 keyring,不动
//! 用户 CLI 的凭据文件(写回 opt-in 属后续迭代)。路径注入可测
//! (magpie `claude_dirs.go` / `codex_store.go` 读取语义)。

use std::io;
use std::path::Path;

use serde::Deserialize;

/// Claude Code 登录(`~/.claude/.credentials.json`)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClaudeLogin {
    #[serde(rename = "claudeAiOauth")]
    pub oauth: ClaudeOauth,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken", default)]
    pub refresh_token: String,
    /// 毫秒时间戳(Claude 惯例)。
    #[serde(rename = "expiresAt", default)]
    pub expires_at: u64,
}

/// 读取 Claude 登录。macOS 先 keychain 的语义属桌面壳集成;
/// 文件路径是跨平台基线,magpie 语义:解析 `claudeAiOauth` 字段。
pub fn read_claude_login(credentials_json: &Path) -> std::io::Result<ClaudeLogin> {
    let text = std::fs::read_to_string(credentials_json)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Codex 登录(`~/.codex/auth.json`,OAuth 模式的 `tokens` 字段)。
/// 真实形状(2026-11 实测):`auth_mode` + `last_refresh` + `tokens`;
/// `last_refresh` 写回侧由 chatgpt 模块维护,读取侧不消费(serde
/// 从宽,未知字段忽略)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexLogin {
    #[serde(default)]
    pub auth_mode: String,
    #[serde(default)]
    pub tokens: Option<CodexTokens>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CodexTokens {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "refresh_token", default)]
    pub refresh_token: String,
    /// OpenAI ID token(JWT):exp / chatgpt_account_id 的载体,§7。
    #[serde(rename = "id_token", default)]
    pub id_token: String,
    /// chatgpt-account-id 头的值;缺省时从 access token claim 解析。
    #[serde(rename = "account_id", default)]
    pub account_id: String,
}

pub(crate) fn codex_uses_keyring(config_toml: &Path) -> bool {
    std::fs::read_to_string(config_toml)
        .is_ok_and(|cfg| cfg.contains("cli_auth_credentials_store") && cfg.contains("keyring"))
}

/// 读取 Codex 文件型登录。退出登录和钥匙串模式返回 None，暂时
/// 无法读取或解析文件时返回错误，调用方可在文件恢复后核对原凭据。
pub fn read_codex_login(auth_json: &Path, config_toml: &Path) -> io::Result<Option<CodexLogin>> {
    if codex_uses_keyring(config_toml) {
        return Ok(None);
    }
    match std::fs::read_to_string(auth_json) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str, name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-cred-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn reads_claude_oauth_fields() {
        let p = tmp(
            "claude",
            ".credentials.json",
            r#"{"claudeAiOauth":{"accessToken":"at-1","refreshToken":"rt-1","expiresAt":1730000000000}}"#,
        );
        let login = read_claude_login(&p).unwrap();
        assert_eq!(login.oauth.access_token, "at-1");
        assert_eq!(login.oauth.refresh_token, "rt-1");
        assert_eq!(login.oauth.expires_at, 1730000000000);
    }

    #[test]
    fn codex_keyring_mode_reads_nothing() {
        let auth = tmp("codex", "auth.json", r#"{"tokens":{"access_token":"a"}}"#);
        let cfg = tmp(
            "codex",
            "config.toml",
            "cli_auth_credentials_store = \"keyring\"",
        );
        assert!(read_codex_login(&auth, &cfg).unwrap().is_none());
    }

    #[test]
    fn codex_file_mode_reads_tokens() {
        let auth = tmp(
            "codex2",
            "auth.json",
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"a","refresh_token":"r","id_token":"i","account_id":"acc"}}"#,
        );
        let cfg = auth.parent().unwrap().join("config.toml");
        std::fs::write(&cfg, "model = gpt-5").unwrap();
        let login = read_codex_login(&auth, &cfg).unwrap().unwrap();
        assert_eq!(login.auth_mode, "chatgpt");
        let t = login.tokens.unwrap();
        assert_eq!(t.access_token, "a");
        assert_eq!(t.id_token, "i");
        assert_eq!(t.account_id, "acc");
    }
}

/// opt-in 写回(设计文档 §7,显式开关才启用):
/// **字段级合并,绝不整体覆盖**——重新读盘、只改目标字段、原子落盘
/// (sub2api 事故:快照覆盖会回滚用户 CLI 刚轮换的新 refresh_token)。
/// 返回 false = 磁盘内容已变(别人写过),调用方应放弃本次写回。
pub fn write_back_claude_token(
    credentials_json: &Path,
    access_token: &str,
    refresh_token: &str,
    expected_old_access: &str,
) -> std::io::Result<bool> {
    // 重新读盘:绝不基于内存快照覆盖。
    let text = std::fs::read_to_string(credentials_json)?;
    let mut v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let current_access = v
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    if current_access != expected_old_access {
        // 别处已轮换:放弃写回,避免回滚(magpie/sub2api 事故语义)。
        return Ok(false);
    }
    if let Some(oauth) = v
        .pointer_mut("/claudeAiOauth")
        .and_then(|o| o.as_object_mut())
    {
        oauth.insert("accessToken".into(), serde_json::json!(access_token));
        if !refresh_token.is_empty() {
            oauth.insert("refreshToken".into(), serde_json::json!(refresh_token));
        }
    }
    let tmp = credentials_json.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v).expect("serialize"))?;
    std::fs::rename(&tmp, credentials_json)?;
    Ok(true)
}

#[cfg(test)]
#[allow(dead_code)]
mod writeback_tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-wb-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".credentials.json");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn field_merge_preserves_other_fields() {
        let p = tmp(
            "merge",
            r#"{"customApiKeyResponses":{"approved":true},"claudeAiOauth":{"accessToken":"old","refreshToken":"rt-old"}}"#,
        );
        let ok = write_back_claude_token(&p, "new", "rt-new", "old").unwrap();
        assert!(ok);
        let t = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        // 其它字段保留(sub2api 事故的反面)。
        assert_eq!(v["customApiKeyResponses"]["approved"], true);
        assert_eq!(v["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "rt-new");
    }

    #[test]
    fn concurrent_rotation_aborts_write() {
        let p = tmp(
            "conflict",
            r#"{"claudeAiOauth":{"accessToken":"someone-else-rotated","refreshToken":"rt"}}"#,
        );
        // 我们手里的旧值是 "old",但盘上已被别处轮换 → 放弃写回。
        let ok = write_back_claude_token(&p, "new", "rt-new", "old").unwrap();
        assert!(!ok);
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("someone-else-rotated"));
    }

    #[test]
    fn empty_refresh_keeps_existing() {
        let p = tmp(
            "keeprefresh",
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"keep-me"}}"#,
        );
        write_back_claude_token(&p, "new", "", "old").unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("keep-me"));
    }
}

//! ChatGPT(codex 订阅)凭据通道(设计文档 §7 / §6.3)。
//!
//! 职责:JWT payload 解码(exp / chatgpt_account_id,只解不验签)、
//! 刷新触发判定(余量 < 5 分钟或 401)、auth.json 写回(字段级合并 +
//! 临时文件 rename + 跨进程文件锁,安全红线)。登录本身只导入不代登录
//! (§7):登录由用户跑 `codex login`,这里读凭据 + 代刷新。
//!
//! design: docs/product-design.md §7(Codex token 刷新 / 写回红线)

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::oauth::{codex_uses_keyring, CodexTokens};

/// Codex 官方 CLI client_id(§7:sub2api `oauth.go` 已验证)。
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Codex token 端点(刷新与登录共用)。
pub const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// 刷新触发余量(magpie 实证:余量 < 5 分钟即刷,§7)。
const REFRESH_MARGIN_SECS: u64 = 300;

/// 锁文件视为陈旧的年龄(持锁进程崩溃后允许他人接管)。
const LOCK_STALE: Duration = Duration::from_secs(30);

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// JWT payload(只解不验签,§7:account_id 缺省时从 access token 的
// `https://api.openai.com/auth` claim 读;exp 也从 access token 读)
// ---------------------------------------------------------------------------

/// base64url 解码(std-only,避免为 payload 解码引入新依赖)。解码
/// 失败返回 None——调用方走保守分支(无 exp → 401 兜底),不 panic。
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        let v = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            // padding 与空白:跳过
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// JWT payload JSON(不验签:我们只读自己的登录凭据的公开声明)。
fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload_b64)?;
    serde_json::from_slice(&bytes).ok()
}

/// access token 的 `exp`(unix 秒)。读不到 → None(刷新判定交给 401)。
pub fn access_expires_at(access_token: &str) -> Option<u64> {
    jwt_payload(access_token)?["exp"].as_u64()
}

/// 刷新触发判定(§7:余量 < 5 分钟)。无 exp 可读 → false:
/// 不主动刷,等 401 兜底(宁可少刷不可错刷——OpenAI 轮换 refresh
/// token,多余的刷新请求有真实代价)。
pub fn needs_refresh(access_token: &str, now: u64) -> bool {
    match access_expires_at(access_token) {
        Some(exp) => exp.saturating_sub(now) < REFRESH_MARGIN_SECS,
        None => false,
    }
}

/// chatgpt-account-id 解析(§7):`tokens.account_id` 优先,缺省时从
/// access token JWT 的 `https://api.openai.com/auth` claim 读
/// `chatgpt_account_id`。都无 → None(网关侧缺头降级处理)。
pub fn resolve_account_id(tokens: &CodexTokens) -> Option<String> {
    if !tokens.account_id.is_empty() {
        return Some(tokens.account_id.clone());
    }
    jwt_payload(&tokens.access_token)?["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// 跨进程文件锁(std-only:O_EXCL 锁文件 + 超时 + 陈旧接管)
// ---------------------------------------------------------------------------

/// auth.json 写回互斥锁。std 无 flock,用 create_new 锁文件实现;
/// 陈旧接管存在极小竞窗(两进程同时清同一陈旧锁),但写回路径在锁内
/// 还有"重读盘 + 旧值比对"这道真正的安全网(见 write_back),锁只是
/// 减少无谓冲突的第一道防线。
pub struct FileGuard {
    path: PathBuf,
}

impl FileGuard {
    /// 获取锁;超时(持锁方僵死且未到陈旧年龄)返回 Err。
    pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self, String> {
        let deadline = SystemTime::now() + timeout;
        loop {
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(lock_path)
            {
                Ok(f) => {
                    // 记录 pid 便于人工排查陈旧锁归属。
                    let _ = write!(&f, "{}", std::process::id());
                    return Ok(FileGuard {
                        path: lock_path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // 陈旧接管:锁龄超过 LOCK_STALE 视为持锁方已死。
                    if let Ok(meta) = fs::metadata(lock_path) {
                        if meta
                            .modified()
                            .ok()
                            .and_then(|m| SystemTime::now().duration_since(m).ok())
                            .is_some_and(|age| age > LOCK_STALE)
                        {
                            let _ = fs::remove_file(lock_path);
                            continue;
                        }
                    }
                    if SystemTime::now() > deadline {
                        return Err(format!("file lock busy: {}", lock_path.display()));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// auth.json 写回(红线:字段级合并 + 原子 rename,绝不整体覆盖快照)
// ---------------------------------------------------------------------------

/// 写回 auth.json(§7):持锁重读盘 → 只改 `tokens` 目标字段与
/// `last_refresh` → 临时文件 rename 原子落盘。登录源变化时放弃
/// 写回，避免覆盖 CLI 更新过的凭据。
pub fn write_back_codex_tokens(
    auth_json: &Path,
    config_toml: &Path,
    expected: &CodexTokens,
    fresh: &CodexTokens,
    now: u64,
) -> io::Result<bool> {
    let lock_path = auth_json.with_extension("json.lock");
    let _guard =
        FileGuard::acquire(&lock_path, Duration::from_secs(5)).map_err(io::Error::other)?;

    if codex_uses_keyring(config_toml) {
        return Ok(false);
    }
    // Compare the original file credentials, including same-access-token
    // account changes, before merging into the latest document.
    let text = fs::read_to_string(auth_json)?;
    let mut v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let current: Option<CodexTokens> = serde::Deserialize::deserialize(&v["tokens"])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if current.as_ref() != Some(expected) {
        return Ok(false);
    }
    let Some(tokens) = v.get_mut("tokens").and_then(|t| t.as_object_mut()) else {
        return Ok(false);
    };
    tokens.insert("access_token".into(), serde_json::json!(fresh.access_token));
    if !fresh.refresh_token.is_empty() {
        tokens.insert(
            "refresh_token".into(),
            serde_json::json!(fresh.refresh_token),
        );
    }
    if !fresh.id_token.is_empty() {
        tokens.insert("id_token".into(), serde_json::json!(fresh.id_token));
    }
    // last_refresh 与 codex CLI 同格式(RFC3339 UTC),CLI 以它判断
    // 登录态新鲜度,写新 token 不更新它会让 CLI 误判过期。
    if let Some(obj) = v.as_object_mut() {
        obj.insert("last_refresh".into(), serde_json::json!(rfc3339_utc(now)));
    }
    crate::private_file::replace(auth_json, &serde_json::to_vec_pretty(&v)?)?;
    Ok(true)
}

/// unix 秒 → RFC3339 UTC(无 chrono 依赖,Howard Hinnant 算法)。
fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// epoch 天数 → 公历年月日(days-from-civil 逆运算)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工 JWT:header.payload.sig,payload 是 base64url(JSON)。
    fn jwt(payload_json: &str) -> String {
        format!(
            "e30.{}.sig",
            base64url_encode(payload_json.as_bytes()).unwrap()
        )
    }

    /// 测试侧编码器(仅拼装语料用;生产路径只解码)。
    fn base64url_encode(bytes: &[u8]) -> Option<String> {
        const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut buf: u32 = 0;
        let mut bits = 0u32;
        for &b in bytes {
            buf = (buf << 8) | b as u32;
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                out.push(TBL[((buf >> bits) & 0x3f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(TBL[((buf << (6 - bits)) & 0x3f) as usize] as char);
        }
        Some(out)
    }

    fn tmp_auth(tag: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-cgpt-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn base64url_roundtrip_and_lenient() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi");
        // 标准字母表(+/)与 URL 字母表(-_)都接受;padding 跳过。
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi");
        assert_eq!(
            base64url_decode("aGk=").unwrap(),
            base64url_decode("aGk").unwrap()
        );
        assert!(base64url_decode("!!").is_none());
    }

    #[test]
    fn exp_read_from_access_token() {
        let t = jwt(r#"{"exp":1730000000,"other":1}"#);
        assert_eq!(access_expires_at(&t), Some(1_730_000_000));
        // 非 JWT / payload 坏 JSON → None(401 兜底)。
        assert_eq!(access_expires_at("garbage"), None);
        assert_eq!(access_expires_at("a.###.b"), None);
        for payload in [
            r#"{}"#,
            r#"{"exp":"1730000000"}"#,
            r#"{"exp":-1}"#,
            r#"{"exp":1.5}"#,
        ] {
            assert_eq!(access_expires_at(&jwt(payload)), None);
        }
    }

    #[test]
    fn refresh_margin_boundary() {
        let now = 1_000_000u64;
        assert!(needs_refresh(
            &jwt(&format!(r#"{{"exp":{}}}"#, now + 299)),
            now
        ));
        assert!(!needs_refresh(
            &jwt(&format!(r#"{{"exp":{}}}"#, now + 300)),
            now
        ));
        assert!(!needs_refresh(
            &jwt(&format!(r#"{{"exp":{}}}"#, now + 301)),
            now
        ));
        assert!(needs_refresh(&jwt(&format!(r#"{{"exp":{now}}}"#)), now));
        assert!(needs_refresh(
            &jwt(&format!(r#"{{"exp":{}}}"#, now - 1)),
            now
        ));
        // 无 exp → 不主动刷(§7:401 兜底)。
        assert!(!needs_refresh("garbage", now));
        for payload in [
            r#"{}"#,
            r#"{"exp":"1730000000"}"#,
            r#"{"exp":-1}"#,
            r#"{"exp":1.5}"#,
        ] {
            assert!(!needs_refresh(&jwt(payload), now));
        }
    }

    #[test]
    fn account_id_field_wins_then_claim_fallback() {
        let access = jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"from-claim"}}"#);
        let explicit = CodexTokens {
            access_token: access.clone(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: "explicit".into(),
        };
        assert_eq!(resolve_account_id(&explicit).as_deref(), Some("explicit"));

        let fallback = CodexTokens {
            access_token: access,
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
        };
        assert_eq!(resolve_account_id(&fallback).as_deref(), Some("from-claim"));

        let none = CodexTokens {
            access_token: jwt("{}"),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
        };
        assert_eq!(resolve_account_id(&none), None);
    }

    #[test]
    fn file_guard_mutual_exclusion_and_release() {
        let dir = std::env::temp_dir().join("aubridge-cgpt-lock");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("auth.json.lock");
        let g = FileGuard::acquire(&lock, Duration::from_millis(200)).unwrap();
        // 二次获取:同进程也走 AlreadyExists,超时报 busy。
        assert!(FileGuard::acquire(&lock, Duration::from_millis(150)).is_err());
        drop(g);
        // 释放后可再获取。
        assert!(FileGuard::acquire(&lock, Duration::from_millis(200)).is_ok());
        assert!(!lock.exists());
    }

    #[test]
    fn write_back_merges_fields_and_updates_last_refresh() {
        let p = tmp_auth(
            "merge",
            r#"{"auth_mode":"chatgpt","last_refresh":"2026-11-01T00:00:00Z","OPENAI_API_KEY":"","tokens":{"access_token":"old-at","refresh_token":"old-rt","id_token":"old-id","account_id":"acc"}}"#,
        );
        let cfg = p.with_file_name("config.toml");
        let expected = crate::oauth::read_codex_login(&p, &cfg)
            .unwrap()
            .unwrap()
            .tokens
            .unwrap();
        let fresh = CodexTokens {
            access_token: "new-at".into(),
            refresh_token: "new-rt".into(),
            id_token: "new-id".into(),
            ..expected.clone()
        };
        let ok = write_back_codex_tokens(&p, &cfg, &expected, &fresh, 1_730_000_000).unwrap();
        assert!(ok);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        // 字段级合并:未知字段(OPENAI_API_KEY/auth_mode/account_id)保留。
        assert_eq!(v["auth_mode"], "chatgpt");
        assert_eq!(v["OPENAI_API_KEY"], "");
        assert_eq!(v["tokens"]["account_id"], "acc");
        assert_eq!(v["tokens"]["access_token"], "new-at");
        assert_eq!(v["tokens"]["refresh_token"], "new-rt");
        assert_eq!(v["tokens"]["id_token"], "new-id");
        assert_eq!(v["last_refresh"], "2024-10-27T03:33:20Z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // 原子写:不留 tmp。
        assert!(!p.with_extension("json.tmp").exists());
    }

    #[test]
    fn write_back_aborts_on_concurrent_rotation() {
        let p = tmp_auth(
            "conflict",
            r#"{"tokens":{"access_token":"at","refresh_token":"rt","account_id":"a1"}}"#,
        );
        let cfg = p.with_file_name("config.toml");
        let expected = crate::oauth::read_codex_login(&p, &cfg)
            .unwrap()
            .unwrap()
            .tokens
            .unwrap();
        let fresh = CodexTokens {
            access_token: "new-at".into(),
            ..expected.clone()
        };
        for changed in [
            r#"{"tokens":{"access_token":"someone-else","refresh_token":"rt","account_id":"a1"}}"#,
            r#"{"tokens":{"access_token":"at","refresh_token":"else-rt","account_id":"a1"}}"#,
            r#"{"tokens":{"access_token":"at","refresh_token":"rt","account_id":"a2"}}"#,
        ] {
            fs::write(&p, changed).unwrap();
            assert!(!write_back_codex_tokens(&p, &cfg, &expected, &fresh, 0).unwrap());
            assert_eq!(fs::read_to_string(&p).unwrap(), changed);
        }
    }

    #[test]
    fn write_back_empty_refresh_keeps_existing() {
        let p = tmp_auth(
            "keeprefresh",
            r#"{"tokens":{"access_token":"old-at","refresh_token":"keep-me"}}"#,
        );
        let cfg = p.with_file_name("config.toml");
        let expected = crate::oauth::read_codex_login(&p, &cfg)
            .unwrap()
            .unwrap()
            .tokens
            .unwrap();
        let fresh = CodexTokens {
            access_token: "new-at".into(),
            refresh_token: String::new(),
            ..expected.clone()
        };
        assert!(write_back_codex_tokens(&p, &cfg, &expected, &fresh, 0).unwrap());
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["tokens"]["refresh_token"], "keep-me");
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 闰日:2000-02-29 是 1970-02-29 之后 30 年的闰日边界。
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_730_000_000), "2024-10-27T03:33:20Z");
    }
}

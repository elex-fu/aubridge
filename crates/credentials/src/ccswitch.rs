//! cc-switch provider 配置只读导入(契约:
//! docs/cc-switch-import-design-2026-11-15.md)。
//!
//! 纪律:只读打开 SQLite(不写 cc-switch 任何文件);key 只在
//! `ImportedProvider` 内存中短暂出现,落点由调用方进 keyring;
//! 回环 base 一律 skip(本机实测 Kimi 行即 aubridge 自身,导入即
//! 自环,设计 D4);坏行 skip 不中断整表。

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use serde_json::Value;

/// 一条候选:可导入(`provider` 有值)或被跳过(`skip` 有原因)。
#[derive(Debug, Clone, PartialEq)]
pub struct CcSwitchCandidate {
    /// `app_type/id` —— POST 回传凭据(表主键即天然去重键)。
    pub reference: String,
    /// cc-switch 里的显示名。
    pub name: String,
    /// 来源 app 的用户可见名("Claude Code" / "Codex"…)。
    pub source: String,
    pub skip: Option<String>,
    pub provider: Option<ImportedProvider>,
}

/// 一行的解析产物:字段与 `credentials::settings::CustomProvider`
/// 一一对应 + key(仅内存传递,永不进 GET 应答 / config.json)。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedProvider {
    pub id: String,
    pub base: String,
    /// `anthropic` / `chat` / `responses`(manifest 字符串枚举,
    /// 由 registry::load 从宽校验兜底)。
    pub protocol: String,
    pub auth: String,
    pub kind: String,
    pub models: Vec<String>,
    pub key: String,
}

/// 一行解析的三态:可导入 / 带原因跳过 / 官方登录行(固定文案)。
enum Parsed {
    Provider(ImportedProvider),
    Skip(String),
    Official,
}

/// cc-switch app_type 的用户可见名(picker 同款命名)。
fn app_name(app_type: &str) -> &str {
    match app_type {
        "claude" => "Claude Code",
        "claude-desktop" => "Claude Desktop",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        _ => "",
    }
}

/// db 缺失 → Ok(vec![])(上层显示"未找到 cc-switch 配置");
/// 存在但读失败 → Err(带 db 上下文)。
pub fn read_candidates(db: &Path) -> io::Result<Vec<CcSwitchCandidate>> {
    if !db.exists() {
        return Ok(Vec::new());
    }
    let connection =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| io::Error::other(format!("打开 cc-switch 数据库失败: {e}")))?;
    // 坏行(解析失败)按 skip 落表,不中断整表:一行坏数据不该挡住
    // 其他行,又不该静默消失。
    let mut entries = Vec::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT id, app_type, name, settings_config
                 FROM providers
                 ORDER BY app_type, COALESCE(sort_index, 0), created_at",
            )
            .map_err(|e| io::Error::other(format!("读 cc-switch providers 表失败: {e}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| io::Error::other(format!("读 cc-switch providers 表失败: {e}")))?;
        for row in rows {
            let (id, app_type, name, settings) =
                row.map_err(|e| io::Error::other(format!("读 cc-switch 行失败: {e}")))?;
            entries.push((id, app_type, name, settings));
        }
    }

    let mut used_ids: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for (id, app_type, name, settings) in entries {
        out.push(candidate(
            &format!("{app_type}/{id}"),
            &app_type,
            &name,
            &settings,
            &mut used_ids,
        ));
    }
    Ok(out)
}

fn candidate(
    reference: &str,
    app_type: &str,
    name: &str,
    settings: &str,
    used_ids: &mut BTreeSet<String>,
) -> CcSwitchCandidate {
    let source = app_name(app_type);
    let mut entry = CcSwitchCandidate {
        reference: reference.to_string(),
        name: name.trim().to_string(),
        source: if source.is_empty() {
            app_type.to_string()
        } else {
            source.to_string()
        },
        skip: None,
        provider: None,
    };
    // 坏 JSON 行按 skip 落表,不让一行坏数据挡住整表。
    let Ok(settings) = serde_json::from_str::<Value>(settings) else {
        entry.skip = Some("配置行不是合法 JSON".into());
        return entry;
    };
    let parsed = match app_type {
        "claude" | "claude-desktop" => claude_row(&settings),
        "codex" => codex_row(&settings),
        // 设计范围:claude / codex 行;gemini 官方 key 行本机没有
        // 可导内容,不做(需要时按 env 补一分支)。
        _ => Some(Parsed::Skip("仅导入 Claude Code / Codex 行".into())),
    }
    .unwrap_or(Parsed::Official);
    match parsed {
        Parsed::Provider(mut provider) => {
            provider.id = unique_id(&entry.name, app_type, used_ids);
            entry.provider = Some(provider);
        }
        Parsed::Skip(reason) => entry.skip = Some(reason),
        Parsed::Official => entry.skip = Some("官方登录行,订阅请在 aubridge 内登录".into()),
    }
    entry
}

/// claude / claude-desktop 行:`env` 里的 base + token + 模型名。
fn claude_row(settings: &Value) -> Option<Parsed> {
    let env = settings["env"].as_object()?;
    let text = |key: &str| {
        env.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let base = text("ANTHROPIC_BASE_URL");
    let key = text("ANTHROPIC_AUTH_TOKEN").or_else(|| text("ANTHROPIC_API_KEY"));
    if base.is_none() && key.is_none() {
        return None; // 官方登录行
    }
    let Some(base) = base else {
        // 设计 D5:有 key 无 base 的行不补默认端点——此形态实际只见于
        // 官方登录行,补全 = 导入一个错端点。
        return Some(Parsed::Skip("仅有 key 无 base,不导入".into()));
    };
    if is_loopback(base) {
        return Some(Parsed::Skip(
            "本地代理端点(可能自环,如经 aubridge 转发),不导入".into(),
        ));
    }
    // 保序去重:cc-switch 常把同名模型写成 [1M] 与裸名两条 env。
    let mut models: Vec<String> = Vec::new();
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ] {
        if let Some(model) = text(key) {
            if !models.iter().any(|m| m == model) {
                models.push(model.to_string());
            }
        }
    }
    Some(Parsed::Provider(ImportedProvider {
        id: String::new(),
        // Claude Code 的 env base 不含版本段(请求走 {base}/v1/messages),
        // aubridge 的 Anthropic 协议路径是 {base}/messages——补上 /v1
        // 才与该行在 Claude Code 里的真实路径一致(2026-11-15 实测
        // longcat:…/anthropic 404,…/anthropic/v1/messages 401=路径存在)。
        // 已含 /v1 的 base 原样保留。
        base: anthropic_base(base),
        protocol: "anthropic".into(),
        auth: "bearer".into(),
        kind: "subscription".into(),
        models,
        key: key.unwrap_or_default().to_string(),
    }))
}

/// env 的 Anthropic base → aubridge 的 messages base:补 `/v1` 段。
fn anthropic_base(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// codex 行:`auth.OPENAI_API_KEY` + 内嵌 TOML 的 model_providers 表。
fn codex_row(settings: &Value) -> Option<Parsed> {
    let key = settings["auth"]["OPENAI_API_KEY"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let config = settings["config"].as_str().unwrap_or_default();
    let toml = if key.is_none() && config.trim().is_empty() {
        return None; // auth.json 型官方行:连 config 都没有
    } else {
        match toml::from_str::<toml::Value>(config) {
            Ok(value) => value,
            Err(_) => return Some(Parsed::Skip("Codex config 不是合法 TOML".into())),
        }
    };
    let provider_name = toml
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let entry = |field: &str| {
        toml.get("model_providers")
            .and_then(|table| table.get(provider_name))
            .and_then(|table| table.get(field))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let base = entry("base_url");
    let key = key
        .map(str::to_string)
        .or_else(|| entry("experimental_bearer_token").map(str::to_string));
    if base.is_none() && key.is_none() {
        return None; // 官方登录行
    }
    let Some(base) = base else {
        return Some(Parsed::Skip("仅有 key 无 base,不导入".into()));
    };
    if is_loopback(base) {
        return Some(Parsed::Skip(
            "本地代理端点(可能自环,如经 aubridge 转发),不导入".into(),
        ));
    }
    // Codex 默认 wire_api 即 responses(设计 D2:一并支持)。
    let protocol = if entry("wire_api") == Some("chat") {
        "chat"
    } else {
        "responses"
    };
    let models = toml
        .get("model")
        .and_then(toml::Value::as_str)
        .map(|model| vec![model.to_string()])
        .unwrap_or_default();
    Some(Parsed::Provider(ImportedProvider {
        id: String::new(),
        base: base.to_string(),
        protocol: protocol.into(),
        auth: "bearer".into(),
        kind: "subscription".into(),
        models,
        key: key.unwrap_or_default(),
    }))
}

/// host 提取仅服务回环判定(设计 D4):不做完整 URL 校验——base 的
/// 有效性由 registry::load 在 POST 落地时校验。
fn is_loopback(base: &str) -> bool {
    let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let host = rest.split(['/', ':', '?']).next().unwrap_or_default();
    matches!(
        host.trim_start_matches('[').trim_end_matches(']'),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// slug + 去重:claude 行保裸名 slug,codex 行撞名追加 `-codex`
/// (Longcat 双行 → `longcat` / `longcat-codex`),仍冲突追加序号。
fn unique_id(name: &str, app_type: &str, used: &mut BTreeSet<String>) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = slug.trim_matches('-');
    let base = if base.is_empty() { "provider" } else { base };
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let with_app = format!("{base}-{app_type}");
    if used.insert(with_app.clone()) {
        return with_app;
    }
    let mut n = 2;
    while !used.insert(format!("{base}-{n}")) {
        n += 1;
    }
    format!("{base}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// fixture 用本机真实行的结构字节(token 换占位符,base/模型名
    /// 保留——语料纪律:不自造样本)。
    const CLAUDE_LONGCAT: &str = r#"{
        "enabledPlugins": {"warp@claude-code-warp": true},
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "placeholder-token",
            "ANTHROPIC_BASE_URL": "https://api.longcat.chat/anthropic",
            "ANTHROPIC_MODEL": "LongCat-2.0[1M]",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "LongCat-2.0",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "LongCat-2.0",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "LongCat-2.0",
            "ANTHROPIC_SMALL_FAST_MODEL": "LongCat-2.0[1M]"
        }
    }"#;

    const CODEX_LONGCAT: &str = r#"{
        "auth": {"OPENAI_API_KEY": "placeholder-key"},
        "config": "model_provider = \"custom\"\nmodel = \"LongCat-2.0\"\n\n[model_providers.custom]\nname = \"longcat\"\nbase_url = \"https://api.longcat.chat/openai/v1\"\nwire_api = \"responses\"\n"
    }"#;

    fn db(tag: &str, rows: &[(&str, &str, &str, &str)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("aubridge-ccswitch-{tag}.db"));
        let _ = std::fs::remove_file(&path);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE providers (
                    id TEXT NOT NULL, app_type TEXT NOT NULL, name TEXT NOT NULL,
                    settings_config TEXT NOT NULL,
                    created_at INTEGER, sort_index INTEGER,
                    PRIMARY KEY (id, app_type))",
                [],
            )
            .unwrap();
        for (i, (id, app_type, name, settings)) in rows.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, created_at, sort_index)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![id, app_type, name, settings, i as i64, i as i64],
                )
                .unwrap();
        }
        path
    }

    #[test]
    fn claude_row_imports_with_deduped_models() {
        let path = db("claude", &[("0a77", "claude", "Longcat", CLAUDE_LONGCAT)]);
        let candidates = read_candidates(&path).unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.reference, "claude/0a77");
        assert_eq!(candidate.source, "Claude Code");
        assert!(candidate.skip.is_none());
        let provider = candidate.provider.as_ref().unwrap();
        assert_eq!(provider.id, "longcat");
        // env base 不含 /v1,导入补上(anthropic_base)。
        assert_eq!(provider.base, "https://api.longcat.chat/anthropic/v1");
        assert_eq!(provider.protocol, "anthropic");
        assert_eq!(provider.auth, "bearer");
        assert_eq!(provider.kind, "subscription");
        assert_eq!(provider.models, vec!["LongCat-2.0[1M]", "LongCat-2.0"]);
        assert_eq!(provider.key, "placeholder-token");
    }

    #[test]
    fn codex_row_maps_wire_api_to_protocol() {
        // responses(默认/显式)与 chat 两个分支。
        let chat_config = r#"{
            "auth": {"OPENAI_API_KEY": "placeholder-key"},
            "config": "model_provider = \"p\"\n\n[model_providers.p]\nbase_url = \"https://relay.example/v1\"\nwire_api = \"chat\"\n"
        }"#;
        let path = db(
            "codex",
            &[
                ("7ee9", "codex", "Longcat", CODEX_LONGCAT),
                ("aaa0", "codex", "Chat relay", chat_config),
            ],
        );
        let candidates = read_candidates(&path).unwrap();
        let responses = candidates
            .iter()
            .find(|c| c.reference == "codex/7ee9")
            .unwrap();
        let provider = responses.provider.as_ref().unwrap();
        assert_eq!(provider.protocol, "responses");
        assert_eq!(provider.base, "https://api.longcat.chat/openai/v1");
        assert_eq!(provider.models, vec!["LongCat-2.0"]);
        assert_eq!(provider.key, "placeholder-key");
        let chat = candidates
            .iter()
            .find(|c| c.reference == "codex/aaa0")
            .unwrap();
        assert_eq!(chat.provider.as_ref().unwrap().protocol, "chat");
    }

    #[test]
    fn same_name_across_apps_gets_app_suffix_ids() {
        let path = db(
            "merge",
            &[
                ("0a77", "claude", "Longcat", CLAUDE_LONGCAT),
                ("7ee9", "codex", "Longcat", CODEX_LONGCAT),
            ],
        );
        let candidates = read_candidates(&path).unwrap();
        let ids: Vec<&str> = candidates
            .iter()
            .map(|c| c.provider.as_ref().unwrap().id.as_str())
            .collect();
        assert_eq!(ids, vec!["longcat", "longcat-codex"]);
    }

    #[test]
    fn loopback_base_is_skipped_as_self_loop_risk() {
        // 本机 Kimi 行的结构:base 即 aubridge 网关回环端口。
        let kimi = CLAUDE_LONGCAT.replace(
            "https://api.longcat.chat/anthropic",
            "http://127.0.0.1:62760",
        );
        let path = db("loopback", &[("fe2d", "claude", "Kimi For Coding", &kimi)]);
        let candidates = read_candidates(&path).unwrap();
        assert!(candidates[0].provider.is_none());
        assert!(candidates[0].skip.as_deref().unwrap().contains("自环"));
    }

    #[test]
    fn official_and_partial_rows_are_skipped() {
        let rows = [
            // 官方登录行:无 base 无 key。
            ("official", "claude", "Claude Official", r#"{"env":{}}"#),
            // 有 key 无 base:不猜端点(D5)。
            (
                "token-only",
                "claude",
                "Token only",
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"k"}}"#,
            ),
            // gemini 行:v1 范围外。
            (
                "gem",
                "gemini",
                "Google Official",
                r#"{"env":{"GEMINI_API_KEY":"g"}}"#,
            ),
            // 坏 JSON 行:skip 不中断整表。
            ("bad", "claude", "Bad row", "not json"),
            // codex 官方 default 行:无 auth 无 config。
            ("default", "codex", "default", r#"{}"#),
        ];
        let path = db("skips", &rows);
        let candidates = read_candidates(&path).unwrap();
        assert_eq!(candidates.len(), rows.len());
        for candidate in &candidates {
            assert!(
                candidate.provider.is_none(),
                "{} 不应可导入",
                candidate.name
            );
            assert!(
                candidate.skip.is_some(),
                "{} 应有 skip 原因",
                candidate.name
            );
        }
        // 排序按 app_type + sort_index:claude(official/token-only/bad)
        // → codex(default)→ gemini(gem)。
        assert!(candidates[0].skip.as_deref().unwrap().contains("官方登录"));
        assert!(candidates[3].skip.as_deref().unwrap().contains("官方登录"));
        assert!(candidates[4]
            .skip
            .as_deref()
            .unwrap()
            .contains("Claude Code / Codex"));
        assert!(candidates[2].skip.as_deref().unwrap().contains("JSON"));
    }

    #[test]
    fn missing_db_yields_empty_list() {
        let candidates = read_candidates(Path::new("/nonexistent/cc-switch.db")).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn anthropic_base_appends_v1_only_when_missing() {
        assert_eq!(
            anthropic_base("https://r.example/anthropic"),
            "https://r.example/anthropic/v1"
        );
        assert_eq!(
            anthropic_base("https://r.example/anthropic/"),
            "https://r.example/anthropic/v1"
        );
        assert_eq!(
            anthropic_base("https://r.example/v1"),
            "https://r.example/v1"
        );
        assert_eq!(
            anthropic_base("https://r.example/v1/"),
            "https://r.example/v1"
        );
    }
}

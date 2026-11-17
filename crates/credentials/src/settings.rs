//! settings —— 用户设置存储(§7 写回 opt-in;engineering-constraints
//! B-7:`~/.aubridge/config.json`,原子写)。
//!
//! 线纪律从宽:未知字段忽略、缺省 default——真实文件可能被未来版本
//! 或用户手改造型。

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

/// 自定义源(UI "添加 Provider",ui-design §6.2;变更记录 32-①):
/// 存 config.json,网关装载时经 registry::load 同一校验路径变成
/// manifest 并入候选。字段固定四项 + 类别/鉴权,无自由 JSON 编辑器
/// (ui-design §10 反目标)。serde 从宽:坏值留给装载侧校验报错。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CustomProvider {
    pub id: String,
    pub base: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// `bearer` / `api-key`(缺省 bearer)。
    #[serde(default = "default_auth")]
    pub auth: String,
    /// 已有自定义源继续使用 Chat，Anthropic 兼容地址可以显式选择 Messages。
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// `free_tier` / `subscription`(缺省 free_tier)。
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_auth() -> String {
    "bearer".to_string()
}

fn default_protocol() -> String {
    "chat".to_string()
}

fn default_kind() -> String {
    "free_tier".to_string()
}

fn default_rule() -> String {
    "free_first".to_string()
}

/// 空串 = 从未选择(opencode-auth-design §4,2026-11-14):网关据此
/// 走首启自动判定;显式 codex/opencode 与无效值分别走显式与
/// fail-closed 路径。POST 只接受 codex/opencode,空串不会经 API 持久化。
fn default_chatgpt_login_source() -> String {
    String::new()
}

/// 模型分组(ui-design §6.3,变更记录 32-③):对外名 + 成员模型
/// (声明序 = 偏好序)+ 路由规则。两类分组同一结构:同名多源映射
/// (name = 成员模型名,单成员多源)与跨模型合并(自定义 name)。
/// rule 为 `free_first`/`free_only`/`sequential`/`round_robin`(枚举只
/// 选择,无规则编辑器——ui-design §10 反目标);坏值留给网关侧
/// 校验报错(serde 从宽)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelGroup {
    /// 对外模型名(agent 请求此名;"group/" 前缀与自动别名保留)。
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    /// `free_first` / `free_only` / `sequential` / `round_robin`(缺省 free_first)。
    #[serde(default = "default_rule")]
    pub rule: String,
}

/// 用户设置。`share_login` = "与 CLI 共享登录"(§7):开启时代刷产物
/// 写回用户 CLI 凭据文件(字段级合并 + 原子 rename);默认只读——
/// 刷新得到的 token 只更新 aubridge 内存缓存。`providers` = 自定义
/// 源清单(变更记录 32-①);`model_groups` = 模型分组 store
/// (变更记录 32-③,网关启动时并入候选表)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub share_login: bool,
    /// Invalid configured values remain visible so the gateway can fail closed.
    #[serde(default = "default_chatgpt_login_source")]
    pub chatgpt_login_source: String,
    #[serde(default)]
    pub providers: Vec<CustomProvider>,
    #[serde(default)]
    pub model_groups: Vec<ModelGroup>,
    #[serde(default)]
    pub paused_providers: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            share_login: false,
            chatgpt_login_source: default_chatgpt_login_source(),
            providers: Vec::new(),
            model_groups: Vec::new(),
            paused_providers: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// 读设置;文件缺失/坏 JSON/读失败一律回默认(设置是尽力语义,
    /// 不因文件问题拒绝启动)。
    pub fn load(path: &Path) -> Self {
        Self::load_checked(path).unwrap_or_default()
    }

    /// Load an editable configuration. Only a missing file can use defaults.
    pub fn load_checked(path: &Path) -> io::Result<Self> {
        read_document(path).map(|(_, settings)| settings)
    }

    /// 原子写(temp + rename)。字段级合并:config.json 是共享文档
    /// (端口等字段由别的写入方维护),只改本结构体持有的字段,
    /// 不整体覆盖——与凭据写回同一条
    /// 红线(绝不基于快照覆盖别人字段)。调用方必须先 load 再改再
    /// save:凭空构造新 Settings 保存会把未携带字段抹成默认值。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let (mut v, _) = read_document(path)?;
        v["share_login"] = serde_json::json!(self.share_login);
        v["chatgpt_login_source"] = serde_json::json!(self.chatgpt_login_source);
        v["providers"] = serde_json::json!(self.providers);
        v["model_groups"] = serde_json::json!(self.model_groups);
        v["paused_providers"] = serde_json::json!(self.paused_providers);
        let text = serde_json::to_string_pretty(&v).map_err(io::Error::other)?;
        atomic_write(path, text.as_bytes())
    }

    pub fn save_port(path: &Path, port: u16) -> io::Result<()> {
        let (mut config, _) = read_document(path)?;
        config["port"] = serde_json::json!(port);
        let text = serde_json::to_vec_pretty(&config).map_err(io::Error::other)?;
        atomic_write(path, &text)
    }

    pub fn load_port(path: &Path) -> Option<u16> {
        let (config, _) = read_document(path).ok()?;
        config["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
    }
}

fn read_document(path: &Path) -> io::Result<(serde_json::Value, Settings)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((serde_json::json!({}), Settings::default()));
        }
        Err(error) => return Err(error),
    };
    let document: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !document.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config.json 需要 JSON 对象",
        ));
    }
    let settings = serde::Deserialize::deserialize(&document)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((document, settings))
}

/// temp + rename 原子写(与 chatgpt.rs 写回同构;所有权:本 crate)。
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-settings-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn missing_file_defaults_to_read_only() {
        let p = tmp("missing");
        assert_eq!(Settings::load(&p), Settings::default());
        assert_eq!(Settings::default().chatgpt_login_source, "");
        assert_eq!(Settings::load_checked(&p).unwrap().chatgpt_login_source, "");
    }

    #[test]
    fn chatgpt_source_roundtrip_preserves_shared_fields_and_read_only_preference() {
        let p = tmp("chatgpt-source");
        std::fs::write(
            &p,
            r#"{"port":3427,"share_login":true,"future":{"preserve":true}}"#,
        )
        .unwrap();
        let mut settings = Settings::load_checked(&p).unwrap();
        assert_eq!(settings.chatgpt_login_source, "");
        settings.chatgpt_login_source = "opencode".into();
        settings.save(&p).unwrap();
        assert_eq!(Settings::load_checked(&p).unwrap(), settings);
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(document["port"], 3427);
        assert_eq!(document["share_login"], true);
        assert_eq!(document["future"], serde_json::json!({"preserve":true}));
        assert_eq!(document["chatgpt_login_source"], "opencode");
        Settings::save_port(&p, 3456).unwrap();
        assert_eq!(
            Settings::load_checked(&p).unwrap().chatgpt_login_source,
            "opencode"
        );
    }

    #[test]
    fn checked_source_preserves_invalid_values_or_reports_bad_types() {
        let p = tmp("chatgpt-source-invalid");
        // "" 是"从未选择"哨兵而非无效值(default 覆盖);无效字符串
        // 原样保留,由网关侧 fail-closed。
        for source in ["unknown", "Codex"] {
            std::fs::write(
                &p,
                serde_json::json!({"chatgpt_login_source":source}).to_string(),
            )
            .unwrap();
            assert_eq!(
                Settings::load_checked(&p).unwrap().chatgpt_login_source,
                source
            );
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!([]),
        ] {
            let content = serde_json::json!({"chatgpt_login_source":value}).to_string();
            std::fs::write(&p, &content).unwrap();
            assert_eq!(
                Settings::load_checked(&p).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                Settings::default().save(&p).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read_to_string(&p).unwrap(), content);
        }
    }

    #[test]
    fn roundtrip_and_lenient_unknown_fields() {
        let p = tmp("round");
        Settings {
            share_login: true,
            ..Default::default()
        }
        .save(&p)
        .unwrap();
        assert_eq!(
            Settings::load(&p),
            Settings {
                share_login: true,
                ..Default::default()
            }
        );
        // 未知字段忽略(从宽),已知字段照读。
        std::fs::write(&p, r#"{"share_login":true,"future":42}"#).unwrap();
        assert_eq!(
            Settings::load(&p),
            Settings {
                share_login: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn bad_json_falls_back_to_default() {
        let p = tmp("bad");
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
    }

    #[test]
    fn custom_provider_roundtrip_with_defaults() {
        let p = tmp("custom");
        let s = Settings {
            share_login: false,
            providers: vec![CustomProvider {
                id: "my-glm".into(),
                base: "https://api.example.dev/v1".into(),
                models: vec!["glm-4.5-flash".into()],
                // 缺省字段(手写 config.json 常见):auth/kind 走默认。
                auth: default_auth(),
                protocol: default_protocol(),
                kind: default_kind(),
            }],
            ..Default::default()
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        // 手写文件缺 auth/kind → 默认值补齐(从宽)。
        std::fs::write(
            &p,
            r#"{"providers":[{"id":"x","base":"https://x.dev","models":["m"]}]}"#,
        )
        .unwrap();
        let loaded = Settings::load(&p);
        assert_eq!(loaded.providers[0].auth, "bearer");
        assert_eq!(loaded.providers[0].protocol, "chat");
        assert_eq!(loaded.providers[0].kind, "free_tier");
    }

    #[test]
    fn model_group_roundtrip_with_default_rule() {
        let p = tmp("groups");
        let s = Settings {
            model_groups: vec![ModelGroup {
                name: "my-daily".into(),
                members: vec!["glm-4.5-flash".into(), "Qwen/…".into()],
                rule: default_rule(),
            }],
            ..Default::default()
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        // 手写文件缺 rule → free_first(从宽)。
        std::fs::write(&p, r#"{"model_groups":[{"name":"g","members":["m"]}]}"#).unwrap();
        assert_eq!(Settings::load(&p).model_groups[0].rule, "free_first");
        // 手写文件已有别的字段,save 不抹(model_groups 是新增字段,
        // 旧版本写的 config.json 走这里升级)。
        std::fs::write(&p, r#"{"port":3427}"#).unwrap();
        let mut s2 = Settings::load(&p);
        s2.model_groups.push(ModelGroup {
            name: "g".into(),
            members: vec!["m".into()],
            rule: "sequential".into(),
        });
        s2.save(&p).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["port"], 3427, "共享字段保留");
        assert_eq!(v["model_groups"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn save_merges_into_shared_config_document() {
        // config.json 与端口等字段共享(B-7):写开关不能覆盖别人的字段。
        let p = tmp("merge");
        std::fs::write(&p, r#"{"port":3427,"share_login":false}"#).unwrap();
        Settings {
            share_login: true,
            ..Default::default()
        }
        .save(&p)
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["port"], 3427, "共享字段保留");
        assert_eq!(v["share_login"], true);
        for invalid in ["not json", "[]", r#"{"providers":"invalid"}"#] {
            std::fs::write(&p, invalid).unwrap();
            assert_eq!(
                Settings::default().save(&p).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                Settings::save_port(&p, 3456).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read_to_string(&p).unwrap(), invalid);
        }
    }
}

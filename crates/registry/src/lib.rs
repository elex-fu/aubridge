//! registry —— 源注册表(设计文档 §7)。
//!
//! M1 第一版:内置 preset(与第三方插件同 schema、同装载路径)+
//! manifest 装载校验。探活/quota 查询(reqwest)与订阅导入随后迭代
//! (crate-layout:registry 的 IO 部分按里程碑补,合同先行)。

use manifest::schema::ProviderKind;
use manifest::schema::{validate, Manifest};
use manifest::state::ProviderState;

/// 装载错误:逐条可读(插件页展示)。
#[derive(Debug, Clone, PartialEq)]
pub struct LoadError(pub Vec<String>);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "manifest 校验失败:{}", self.0.join("; "))
    }
}

/// 解析 + 校验一份 manifest 文本(第三方插件与 preset 同路径)。
pub fn load(text: &str) -> Result<Manifest, LoadError> {
    let m: Manifest =
        serde_json::from_str(text).map_err(|e| LoadError(vec![format!("JSON 解析失败: {e}")]))?;
    let errs = validate(&m);
    if errs.is_empty() {
        Ok(m)
    } else {
        Err(LoadError(errs))
    }
}

/// 内置 preset(M1 三家,设计文档 §7 白名单)。
pub fn presets() -> Vec<Manifest> {
    [
        openrouter(),
        zhipu(),
        modelscope(),
        kimi_code(),
        chatgpt(),
        claude_cli(),
    ]
    .to_vec()
}

fn manifest(json: serde_json::Value) -> Manifest {
    serde_json::from_value(json).expect("builtin preset json")
}

/// OpenRouter 的 key 预算和免费请求限额分别观测。
pub fn openrouter() -> Manifest {
    manifest(serde_json::json!({
        "id": "openrouter",
        "version": "1",
        "provider": { "base": "https://openrouter.ai/api/v1", "auth": "bearer" },
        "free_models": [
            "cohere/north-mini-code:free", "nvidia/nemotron-3.5-lightning:free"
        ],
        "quota": { "source": { "queryable": "/api/v1/key" }, "rest_hint": "daily-utc" },
        "permissions": { "net": ["openrouter.ai"], "store_key": true }
    }))
}

/// 智谱 / Z.ai:429/响应头推断。
pub fn zhipu() -> Manifest {
    manifest(serde_json::json!({
        "id": "zhipu",
        "version": "1",
        "provider": { "base": "https://open.bigmodel.cn/api/paas/v4", "auth": "api-key" },
        "free_models": ["glm-4.5-flash", "glm-4.6v-flash"],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["open.bigmodel.cn"], "store_key": true }
    }))
}

/// ModelScope:每日免费额度,推断为主。
pub fn modelscope() -> Manifest {
    manifest(serde_json::json!({
        "id": "modelscope",
        "version": "1",
        "provider": { "base": "https://api-inference.modelscope.cn/v1", "auth": "bearer" },
        "free_models": [
            "Qwen/Qwen3-235B-A22B", "Qwen/Qwen3-Coder-30B-A3B-Instruct",
            "zai-org/GLM-4.5-Flash"
        ],
        "quota": { "source": "inferred", "rest_hint": "daily-utc" },
        "permissions": { "net": ["api-inference.modelscope.cn"], "store_key": true }
    }))
}

/// Kimi Coding Plan(订阅源,§7 C 类)。协议字段按 model-router
/// `profiles.ts` 的 kimi-code 核实:anthropic 原生协议,
/// `https://api.kimi.com/coding/v1` + `messages` 端点,x-api-key 鉴权。
pub fn kimi_code() -> Manifest {
    manifest(serde_json::json!({
        "id": "kimi-code", "version": "1",
        "kind": "subscription",
        "provider": {
            "base": "https://api.kimi.com/coding/v1",
            "auth": "api-key",
            "protocol": "anthropic"
        },
        "free_models": ["kimi-k3", "kimi-k3.1-pro", "kimi-k2.8-preview"],
        "quota": { "source": "inferred", "rest_hint": "5h-window" },
        "permissions": { "net": ["api.kimi.com"], "store_key": true }
    }))
}

/// ChatGPT/Codex 订阅(§6.3):Responses 方言,端点
/// `chatgpt.com/backend-api/codex/responses`(gateway 拼 `responses`)。
/// 模型清单按本机套餐实测枚举(2026-11,models_cache.json 全量
/// 10 slug;§6.3:不抄 sub2api 全家桶别名表,写死最小集)。
/// 凭据走 §7 ChatGPT 通道(~/.codex/auth.json 代刷新),非 store_key
/// 通道;额度 = wham/usage 查询 + x-codex-* 被动采样(sub2api 口径,
/// §7)。
pub fn chatgpt() -> Manifest {
    manifest(serde_json::json!({
        "id": "chatgpt", "version": "1",
        "kind": "subscription",
        "provider": {
            "base": "https://chatgpt.com/backend-api/codex",
            "auth": "bearer",
            "protocol": "responses"
        },
        "free_models": [
            "gpt-6.1-sol", "gpt-6-astra", "gpt-6.1-terra", "gpt-6-sol",
            "gpt-reserve", "gpt-6-luna", "gpt-5.6-sol", "gpt-5.6-terra",
            "gpt-5.6-luna", "codex-auto-review"
        ],
        "quota": { "source": "usage_api", "rest_hint": "5h-window" },
        "permissions": { "net": ["chatgpt.com"], "store_key": false }
    }))
}

/// Official locally installed Claude CLI; authentication remains owned by that CLI.
pub fn claude_cli() -> Manifest {
    manifest(serde_json::json!({
        "id":"claude-cli", "version":"1", "kind":"subscription",
        "provider":{"base":"https://claude.ai", "auth":"claude_cli", "protocol":"anthropic"},
        "free_models":["sonnet", "opus", "haiku"],
        "quota":{"source":"inferred", "rest_hint":"unknown"},
        "permissions":{"net":["claude.ai"], "store_key":false}
    }))
}

/// 从 manifest 产出 ProviderState 快照(M1:初始态)。
pub fn initial_state(m: &Manifest, kind: ProviderKind) -> ProviderState {
    ProviderState::new(&m.id, kind, m.free_models.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_presets_all_valid() {
        for p in presets() {
            assert!(
                validate(&p).is_empty(),
                "preset {} invalid: {:?}",
                p.id,
                validate(&p)
            );
        }
    }

    #[test]
    fn third_party_manifest_same_path() {
        // 设计文档 §9 的插件示例:同一 load 路径。
        let text = r#"{
            "id": "kenari-free", "version": "0.3.1",
            "provider": { "base": "https://api.kenari.dev/v1", "auth": "api-key" },
            "free_models": ["deepseek-v4-flash:free"],
            "permissions": { "net": ["api.kenari.dev"], "store_key": true }
        }"#;
        let m = load(text).expect("plugin manifest loads");
        assert_eq!(m.id, "kenari-free");
        assert!(matches!(
            m.quota.source,
            manifest::schema::QuotaSource::Inferred
        ));
    }

    #[test]
    fn invalid_manifest_reports_readable_errors() {
        let text = r#"{
            "id": "BAD", "version": "",
            "provider": { "base": "not-a-url", "auth": "api-key" },
            "free_models": []
        }"#;
        let errs = load(text).expect_err("should fail");
        assert!(errs.0.len() >= 4, "readable per-field errors: {:?}", errs.0);
    }

    #[test]
    fn chatgpt_preset_is_responses_subscription() {
        let m = chatgpt();
        assert_eq!(m.id, "chatgpt");
        assert!(matches!(m.kind, ProviderKind::Subscription));
        assert!(matches!(
            m.provider.protocol,
            manifest::schema::WireProtocol::Responses
        ));
        // 套餐实测 10 slug(§6.3:模型清单按用户套餐枚举,写死)。
        assert_eq!(m.free_models.len(), 10);
        assert!(m.free_models.contains(&"gpt-6.1-sol".to_string()));
        // OAuth 通道凭据不进 aubridge keyring(store_key=false)。
        assert!(!m.permissions.store_key);
    }

    #[test]
    fn initial_state_carries_models() {
        let s = initial_state(&zhipu(), ProviderKind::FreeTier);
        assert!(s.supplies("glm-4.5-flash"));
        assert!(!s.supplies("other"));
    }
}
pub mod plugins;

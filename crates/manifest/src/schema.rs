//! 声明 schema 与校验。
//!
//! 校验失败必须给出可读错误清单(crate-layout §3 合同要点),
//! 供插件页逐条展示;绝不 panic。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 插件/源声明。第三方插件与内置 preset 同构(设计文档 §9)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 源 ID(kebab-case slug)。
    pub id: String,
    pub version: String,
    pub provider: ProviderDecl,
    /// 源类别(§7:free_tier 缺省 / subscription = Coding Plan)。
    #[serde(default)]
    pub kind: ProviderKind,
    /// 供应模型(kimi 等订阅源同字段)。
    #[serde(default)]
    pub free_models: Vec<String>,
    /// 已知不接受工具调用的模型。旧 manifest 保留原有路由行为。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub models_without_tools: BTreeSet<String>,
    /// 已知上下文窗口，单位为 token。缺省表示未知。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_windows: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_facts: BTreeMap<String, crate::model::ModelFacts>,
    #[serde(default)]
    pub quota: QuotaDecl,
    #[serde(default)]
    pub permissions: Permissions,
}

/// 源类别(设计文档 §7:FreeTier | Subscription)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    FreeTier,
    Subscription,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDecl {
    /// 上游 base URL,如 `https://openrouter.ai/api/v1`。
    pub base: String,
    pub auth: AuthKind,
    /// 上游线协议:`chat`(OpenAI Chat,缺省,需翻译)/
    /// `anthropic`(Anthropic 原生,同协议字节透传,§6)/
    /// `responses`(OpenAI Responses,codex 订阅方言,§6.3)。
    #[serde(default)]
    pub protocol: WireProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    #[default]
    Chat,
    Anthropic,
    Responses,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// 接受 kebab-case 别名:第三方 manifest 手写 JSON 的常见拼写。
    #[serde(alias = "api-key")]
    ApiKey,
    Bearer,
    /// Native Claude Code subscription authentication; never an API credential.
    ClaudeCli,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QuotaDecl {
    /// `queryable(<uri>)` / `inferred` / `usage_api`。
    #[serde(default)]
    pub source: QuotaSource,
    /// 重置提示:`daily-utc` 等(rest 兜底语义,router 使用)。
    #[serde(default)]
    pub rest_hint: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// 精确查询(如 OpenRouter `GET /api/v1/key`)。
    Queryable(String),
    /// 429/响应头推断(缺省)。
    #[default]
    Inferred,
    /// 订阅 usage API(如 Claude `/api/oauth/usage`)。
    UsageApi,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// 允许宿主代为请求的域名单(宿主侧强制,§9)。
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub store_key: bool,
}

/// 校验 manifest:返回错误清单;空 = 通过。
pub fn validate(m: &Manifest) -> Vec<String> {
    let mut errs = Vec::new();
    if m.id.is_empty() {
        errs.push("id 不能为空".to_string());
    } else if !m
        .id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errs.push(format!("id `{}` 必须是 kebab-case 小写 slug", m.id));
    }
    if m.version.is_empty() {
        errs.push("version 不能为空".to_string());
    }
    match base_host(&m.provider.base) {
        Some(host) => {
            // 域名单强制:base 的 host 必须在声明的 net 白名单内
            //("插件夹带私货"的结构性防御,§9)。
            if !allows_host(host, &m.permissions.net) {
                errs.push("provider.base 的域名不在 permissions.net 白名单内".into());
            }
        }
        None => {
            errs.push("provider.base 必须是合法 HTTP(S) 地址，不能含用户信息、查询串或片段".into())
        }
    }
    if m.free_models.is_empty() {
        errs.push("free_models 不能为空(声明免费源是插件唯一职责)".to_string());
    }
    for (model, window) in &m.context_windows {
        if *window == 0 || !m.free_models.contains(model) {
            errs.push(format!(
                "context_windows `{model}` 必须为已声明模型指定正数窗口"
            ));
        }
    }
    for (model, facts) in &m.model_facts {
        if !m.free_models.contains(model)
            || facts.context == Some(0)
            || facts.output == Some(0)
            || facts.price.as_ref().is_some_and(|price| !price.valid())
        {
            errs.push(format!(
                "model_facts `{model}` 必须属于已声明模型且容量、价格有效"
            ));
        }
    }
    errs
}

/// Check the authority shape without an HTTP dependency. The gateway also
/// verifies the parsed host used by its client before sending credentials.
pub fn base_host(raw: &str) -> Option<&str> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    if rest.contains(['?', '#', '\\']) || rest.bytes().any(|b| b.is_ascii_control()) {
        return None;
    }
    let authority = rest.split('/').next()?;
    if authority.contains('@') || authority.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        authority[1..end].parse::<std::net::Ipv6Addr>().ok()?;
        authority.split_at(end + 1)
    } else {
        authority.split_at(authority.find(':').unwrap_or(authority.len()))
    };
    if !port.is_empty() {
        let port = port.strip_prefix(':')?;
        if !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err() {
            return None;
        }
    }
    (!host.is_empty()).then_some(host)
}

pub fn allows_host(host: &str, domains: &[String]) -> bool {
    fn bare_ip(value: &str) -> &str {
        value
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(value)
    }
    let host = bare_ip(host);
    domains.is_empty()
        || domains.iter().any(|domain| {
            let domain = bare_ip(domain);
            host.eq_ignore_ascii_case(domain)
                || host
                    .strip_prefix("www.")
                    .is_some_and(|h| h.eq_ignore_ascii_case(domain))
                || host
                    .parse::<std::net::Ipv6Addr>()
                    .ok()
                    .zip(domain.parse::<std::net::Ipv6Addr>().ok())
                    .is_some_and(|(host, domain)| host == domain)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "id": "kenari-free",
            "version": "0.3.1",
            "provider": { "base": "https://api.kenari.dev/v1", "auth": "api-key" },
            "free_models": ["deepseek-v4-flash:free"],
            "permissions": { "net": ["api.kenari.dev"], "store_key": true }
        }))
        .expect("manifest json")
    }

    #[test]
    fn valid_manifest_passes() {
        assert!(validate(&minimal()).is_empty());
    }

    #[test]
    fn context_windows_require_a_declared_model_and_positive_capacity() {
        let mut manifest = minimal();
        let model = manifest.free_models[0].clone();
        manifest.context_windows.insert(model.clone(), 0);
        manifest
            .context_windows
            .insert("missing-model".into(), 1024);
        assert_eq!(validate(&manifest).len(), 2);
        manifest.context_windows.remove("missing-model");
        manifest.context_windows.insert(model, 4096);
        assert!(validate(&manifest).is_empty());
    }

    #[test]
    fn base_host_outside_net_allowlist_rejected() {
        let mut m = minimal();
        m.provider.base = "https://evil.example.com/v1".into();
        let errs = validate(&m);
        assert!(errs.iter().any(|e| e.contains("不在 permissions.net")));
    }

    #[test]
    fn readable_errors_for_empty_fields() {
        let m = serde_json::from_value(serde_json::json!({
            "id": "BAD_ID", "version": "", "provider": { "base": "not-url", "auth": "api-key" },
            "free_models": []
        }))
        .expect("manifest json");
        let errs = validate(&m);
        assert!(
            errs.len() >= 4,
            "each invalid field gets a readable error: {errs:?}"
        );
    }
}

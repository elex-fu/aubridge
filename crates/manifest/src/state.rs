//! ProviderState —— router 的纯函数输入快照(crate-layout §2)。
//!
//! registry 持有真实状态并周期性产出快照;router 在快照上做决策,
//! 无锁无 IO。时间统一 unix 秒(engineering-constraints C)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::schema::ProviderKind;

/// 某一时刻的源状态快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderState {
    pub id: String,
    pub kind: ProviderKind,
    /// 该源供应的模型 ID(free_models)。
    pub models: Vec<String>,
    /// 当日剩余量(精确查询或推断);None = 未知(按有余量处理)。
    pub remaining: Option<u64>,
    /// 额度重置时刻(unix 秒)。
    pub resets_at: Option<u64>,
    /// rest 到该时刻(不含);None = 未休息。
    pub rest_until: Option<u64>,
    /// 旧快照没有冷却原因，保留原有时限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_reason: Option<AccountRestReason>,
    /// sink:真没量,沉底兜底(与 rest 独立,magpie `sink.go`)。
    pub sunk: bool,
    /// 模型单独不可用时，保留同一凭据下的其他候选。旧账本没有此字段。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_cooldowns: BTreeMap<String, ModelCooldown>,
    /// Short-lived rejection of the key used by the current connection.
    #[serde(skip)]
    pub key_retry_at: Option<u64>,
    /// 探活结论(3 连败标记 unhealthy)。
    pub healthy: bool,
    /// credit 爬梯当前档(0 起;设计文档 §8.1 四桶表)。
    pub credit_ladder_step: u8,
    /// revoked / 403 下架后为 false,不再进候选。
    pub enabled: bool,
    /// 5xx/网络失败熔断器(§8.1:5 连败 open,30s 半开,2 成恢复)。
    #[serde(default)]
    pub breaker: BreakerState,
    /// 5h 窗口利用率(百分比 0–100;§8.2 被动采样/主动查询写入,
    /// None = 未知)。各来源在边界归一:Anthropic 头是 0–1 小数,
    /// codex 头与 wham 体是百分比。
    #[serde(default)]
    pub util_5h: Option<f64>,
    /// 7d 窗口利用率(百分比 0–100;口径同 `util_5h`)。
    #[serde(default)]
    pub util_7d: Option<f64>,
    #[serde(default)]
    pub quota: crate::quota::QuotaState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountRestReason {
    RateLimit,
    QuotaExhausted,
    Credit,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCooldown {
    pub until: u64,
    pub reason: ModelCooldownReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelCooldownReason {
    ModelRateLimit,
    ModelQuotaExhausted,
    ModelUnavailable,
}

/// 熔断器状态(closed → open → half-open → closed)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BreakerState {
    /// 5xx/网络失败连续计数(closed 期;任何非熔断失败/成功清零)。
    pub fail_streak: u8,
    /// open 到该时刻(unix 秒;已过 = half-open,None = closed)。
    pub open_until: Option<u64>,
    /// 半开期成功计数(2 成恢复)。
    pub half_wins: u8,
}

impl ProviderState {
    pub fn new(id: &str, kind: ProviderKind, models: Vec<String>) -> Self {
        Self {
            id: id.to_string(),
            kind,
            models,
            remaining: None,
            resets_at: None,
            rest_until: None,
            rest_reason: None,
            sunk: false,
            model_cooldowns: BTreeMap::new(),
            key_retry_at: None,
            healthy: true,
            credit_ladder_step: 0,
            enabled: true,
            breaker: BreakerState::default(),
            util_5h: None,
            util_7d: None,
            quota: crate::quota::QuotaState::default(),
        }
    }

    /// 熔断 open(§8.1:5xx/网络失败 5 连败,30s 半开)。
    pub fn is_breaker_open(&self, now: u64) -> bool {
        self.breaker.open_until.is_some_and(|t| t > now)
    }

    /// 是否供应该模型(直接 id 或 `free/<model>` 组的成员)。
    pub fn supplies(&self, model: &str) -> bool {
        if let Some(free) = model.strip_prefix("free/") {
            // 组合模型:免费池语义 —— 只有免费源参与。
            self.kind == ProviderKind::FreeTier && self.models.iter().any(|m| m == free)
        } else {
            self.models.iter().any(|m| m == model)
        }
    }

    pub fn is_resting(&self, now: u64) -> bool {
        self.rest_until.is_some_and(|t| t > now)
    }

    pub fn model_cooldown(&self, model: &str, now: u64) -> Option<&ModelCooldown> {
        self.model_cooldowns
            .get(model)
            .filter(|cooldown| cooldown.until > now)
    }
}

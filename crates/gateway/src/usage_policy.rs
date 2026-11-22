//! Decision 52: one cost predicate for every inference candidate.
use credentials::usage_policy::{connection_identity, CostIntent, SubscriptionAuthorization};
use manifest::schema::{AuthKind, Manifest, ProviderKind, WireProtocol};
use serde::{Deserialize, Serialize};

use super::GatewayState;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UsagePolicy {
    pub cost_intent: Option<CostIntent>,
    #[serde(skip_deserializing)]
    pub configuration_issue: Option<String>,
    pub subscription_authorizations: Vec<SubscriptionAuthorization>,
}

impl UsagePolicy {
    pub(crate) fn invalid() -> Self {
        Self {
            configuration_issue: Some("invalid-settings".into()),
            ..Self::default()
        }
    }

    pub(crate) fn from_settings(settings: &credentials::settings::Settings) -> Self {
        Self {
            cost_intent: settings.cost_intent,
            configuration_issue: None,
            subscription_authorizations: settings.subscription_authorizations.clone(),
        }
    }
}

impl GatewayState {
    pub(crate) fn cost_policy(&self) -> UsagePolicy {
        self.usage_policy.read().expect("cost policy lock").clone()
    }

    pub(crate) fn set_cost_policy(&self, policy: UsagePolicy) -> Result<(), String> {
        let (config, _) = self.settings.as_ref().ok_or("设置存储未接线")?;
        if policy.subscription_authorizations.len() > 1000 {
            return Err("订阅授权过多".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for authorization in &policy.subscription_authorizations {
            let manifest = self
                .manifest(&authorization.provider)
                .ok_or("订阅来源不存在")?;
            if !seen.insert(&authorization.provider)
                || authorization.connection_identity != connection_identity(&manifest)
                || !self.subscription_transport(&manifest)
            {
                return Err("请重新确认当前个人订阅通道；普通按量 API 不能用于订阅兜底".into());
            }
        }
        let _settings = self.settings_io.lock().map_err(|_| "设置忙")?;
        let mut settings =
            credentials::settings::Settings::load_checked(config).map_err(|_| "读取设置失败")?;
        settings.cost_intent = policy.cost_intent;
        settings.subscription_authorizations = policy.subscription_authorizations.clone();
        settings
            .save_confirmed_cost_policy(config)
            .map_err(|_| "保存费用意图失败")?;
        *self.usage_policy.write().map_err(|_| "费用意图忙")? = policy;
        Ok(())
    }

    pub(crate) fn subscription_transport(&self, manifest: &Manifest) -> bool {
        manifest.kind == ProviderKind::Subscription
            && ((self.is_chatgpt(manifest)
                && manifest.provider.base == crate::chatgpt::ChatGptChannel::RESPONSES_BASE
                && manifest.provider.protocol == WireProtocol::Responses
                && manifest.provider.auth == AuthKind::Bearer)
            // Audited Kimi Code membership endpoint, distinct from Moonshot PAYG.
            || (manifest.provider.base == "https://api.kimi.com/coding/v1"
                && manifest.provider.auth == AuthKind::ApiKey
                && manifest.provider.protocol == WireProtocol::Anthropic)
            || (crate::claude_cli::owns(manifest)
                && manifest.provider.protocol == WireProtocol::Anthropic))
    }

    pub(crate) fn confirmed_free(&self, manifest: &Manifest, model: &str, now: u64) -> bool {
        let Some(catalog) = &self.openrouter_catalog else {
            return false;
        };
        if manifest.provider.base != "https://openrouter.ai/api/v1"
            || manifest.provider.protocol != WireProtocol::Chat
            || manifest.provider.auth != AuthKind::Bearer
            || catalog.price_source() != crate::catalog::OPENROUTER_MODELS
        {
            return false;
        }
        let view = catalog.view(now);
        view.issue.is_none()
            && !view.stale
            && view.fetched_at.is_some_and(|at| at <= now)
            && view
                .models
                .iter()
                .any(|row| row.id == model && row.pricing_verified)
    }

    pub(crate) fn cost_block(
        &self,
        manifest: &Manifest,
        model: &str,
        now: u64,
    ) -> Option<&'static str> {
        let policy = self.cost_policy();
        if policy.configuration_issue.is_some() {
            return Some("cost-policy-invalid");
        }
        let intent = policy.cost_intent?;
        if self.confirmed_free(manifest, model, now) {
            return None;
        }
        if intent == CostIntent::FreeThenSubscription
            && self.subscription_transport(manifest)
            && policy
                .subscription_authorizations
                .iter()
                .any(|authorization| {
                    authorization.provider == manifest.id
                        && authorization.connection_identity == connection_identity(manifest)
                })
        {
            return None;
        }
        Some(if intent == CostIntent::ConfirmedFreeOnly {
            "cost-unconfirmed-free"
        } else {
            "cost-subscription-not-authorized"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Model, OpenRouterCatalog, Snapshot, OPENROUTER_MODELS};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn manifest(base: &str) -> Manifest {
        serde_json::from_value(json!({"id":"openrouter","version":"1","kind":"free_tier",
            "provider":{"base":base,"protocol":"chat","auth":"bearer"},
            "free_models":["owner/model:free"],"permissions":{"net":["openrouter.ai"]}}))
        .unwrap()
    }

    fn state(endpoint: &str, at: u64, proof: bool) -> GatewayState {
        let catalog = OpenRouterCatalog::new(endpoint.into());
        catalog.complete(Ok(Snapshot {
            fetched_at: at,
            models: vec![Model {
                id: "owner/model:free".into(),
                tools: true,
                context: None,
                facts: Default::default(),
                pricing_verified: proof,
            }],
        }));
        let mut state = GatewayState::with_keys(BTreeMap::new(), "fixture", BTreeMap::new());
        state.openrouter_catalog = Some(catalog);
        state
    }

    #[test]
    fn only_current_official_complete_pricing_proof_confirms_free() {
        let manifest = manifest("https://openrouter.ai/api/v1");
        for (endpoint, at, proof, expected) in [
            (OPENROUTER_MODELS, 99_900, true, true),
            (OPENROUTER_MODELS, 99_900, false, false),
            (OPENROUTER_MODELS, 1, true, false),
            (OPENROUTER_MODELS, 100_001, true, false),
            ("https://untrusted.invalid/models", 100, true, false),
        ] {
            let now = if at == 1 { 86_401 } else { 100_000 };
            assert_eq!(
                state(endpoint, at, proof).confirmed_free(&manifest, "owner/model:free", now),
                expected
            );
        }
        assert!(!state(OPENROUTER_MODELS, 100, true).confirmed_free(
            &super::tests::manifest("https://openrouter.ai.evil.invalid/api/v1"),
            "owner/model:free",
            100
        ));
    }

    #[test]
    fn subscription_authorization_is_bound_to_audited_transport_and_connection() {
        let mut manifest: Manifest = serde_json::from_value(json!({"id":"chatgpt","version":"1","kind":"subscription",
            "provider":{"base":crate::chatgpt::ChatGptChannel::RESPONSES_BASE,"protocol":"responses","auth":"bearer"},
            "free_models":["m"],"permissions":{"net":["chatgpt.com"]}})).unwrap();
        let state = state(OPENROUTER_MODELS, 100, true);
        *state.usage_policy.write().unwrap() = UsagePolicy {
            cost_intent: Some(CostIntent::FreeThenSubscription),
            configuration_issue: None,
            subscription_authorizations: vec![SubscriptionAuthorization {
                provider: "chatgpt".into(),
                basis: credentials::usage_policy::SubscriptionBasis::PersonalChannel,
                connection_identity: connection_identity(&manifest),
            }],
        };
        assert!(state.cost_block(&manifest, "m", 100).is_none());
        manifest.provider.base = "https://api.openai.com/v1".into();
        assert!(state.cost_block(&manifest, "m", 100).is_some());
        manifest.provider.base = crate::chatgpt::ChatGptChannel::RESPONSES_BASE.into();
        state
            .usage_policy
            .write()
            .unwrap()
            .subscription_authorizations
            .clear();
        assert!(state.cost_block(&manifest, "m", 100).is_some());
    }
    #[test]
    fn kimi_membership_transport_requires_exact_connection_and_authorization() {
        let state = state(OPENROUTER_MODELS, 100, true);
        let mut manifest = registry::kimi_code();
        manifest.id = "my-membership".into();
        assert!(state.subscription_transport(&manifest));
        *state.usage_policy.write().unwrap() = UsagePolicy {
            cost_intent: Some(CostIntent::FreeThenSubscription),
            configuration_issue: None,
            subscription_authorizations: vec![SubscriptionAuthorization {
                provider: manifest.id.clone(),
                connection_identity: connection_identity(&manifest),
                basis: credentials::usage_policy::SubscriptionBasis::UserDeclared,
            }],
        };
        assert!(state.cost_block(&manifest, "kimi-k3", 100).is_none());
        manifest.provider.base = "https://api.moonshot.cn/v1".into();
        assert!(!state.subscription_transport(&manifest));
        assert!(state.cost_block(&manifest, "kimi-k3", 100).is_some());
        manifest.provider.base = "https://api.kimi.com/coding/v1".into();
        manifest.provider.protocol = WireProtocol::Chat;
        assert!(!state.subscription_transport(&manifest));
        manifest.provider.protocol = WireProtocol::Anthropic;
        state.usage_policy.write().unwrap().cost_intent = Some(CostIntent::ConfirmedFreeOnly);
        assert!(state.cost_block(&manifest, "kimi-k3", 100).is_some());
    }
}

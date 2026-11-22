//! Independent facts; health never generates an inference request.
use manifest::schema::Manifest;
use serde_json::{json, Value};

use super::{ConnectionVersion, GatewayState};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

#[derive(Default)]
pub(crate) struct Observations(RwLock<BTreeMap<String, Observation>>);

struct Observation {
    version: ConnectionVersion,
    subject: String,
    last_check: Option<Value>,
    last_real_success_at: Option<u64>,
}

impl Observations {
    fn record(
        &self,
        id: &str,
        version: &ConnectionVersion,
        subject: &str,
        update: impl FnOnce(&mut Observation),
    ) {
        let mut entries = self.0.write().expect("source observations lock");
        let entry = entries.entry(id.into()).or_insert_with(|| Observation {
            version: version.clone(),
            subject: subject.into(),
            last_check: None,
            last_real_success_at: None,
        });
        if !Arc::ptr_eq(&entry.version, version) || entry.subject != subject {
            *entry = Observation {
                version: version.clone(),
                subject: subject.into(),
                last_check: None,
                last_real_success_at: None,
            };
        }
        update(entry);
    }
}

impl GatewayState {
    /// Call only after the existing bound connection guard accepts this receipt.
    pub(crate) fn record_source_success(
        &self,
        id: &str,
        version: &ConnectionVersion,
        subject: &str,
        now: u64,
    ) {
        self.source_health.record(id, version, subject, |entry| {
            entry.last_real_success_at = Some(now)
        });
    }

    pub(crate) fn record_source_check(
        &self,
        id: &str,
        version: &ConnectionVersion,
        subject: &str,
        status: &str,
        model: &str,
        now: u64,
    ) {
        self.source_health.record(id, version, subject, |entry| {
            entry.last_check = Some(json!({
                "status":status,"model":model,"observed_at":now,"scope":"explicit_source_inference"
            }))
        });
    }

    pub(crate) fn source_health(&self, manifest: &Manifest, now: u64) -> Value {
        let present = if crate::claude_cli::owns(manifest) {
            self.claude_cli.ready()
        } else if self.is_chatgpt(manifest) {
            self.has_chatgpt_login()
        } else {
            self.api_key_opt(&manifest.id)
                .is_some_and(|key| !key.trim().is_empty())
        };
        let local = matches!(
            crate::route_plan::credential(self, manifest),
            Some(crate::route_plan::Credential::Local)
        );
        let source = self
            .providers
            .read()
            .expect("providers lock")
            .get(&manifest.id)
            .cloned();
        let usable = source.as_ref().is_some_and(|source| {
            (present || local) && source.key_retry_at.is_none_or(|at| at <= now)
        });
        let directory = serde_json::to_value(self.directory_view(manifest)).unwrap_or(Value::Null);
        let confirmed = manifest
            .free_models
            .iter()
            .any(|model| self.confirmed_free(manifest, model, now));
        let policy = self.cost_policy();
        let identity = credentials::usage_policy::connection_identity(manifest);
        let authorized = self.subscription_transport(manifest)
            && policy
                .subscription_authorizations
                .iter()
                .any(|authorization| {
                    authorization.provider == manifest.id
                        && authorization.connection_identity == identity
                });
        let version = self
            .connection_versions
            .read()
            .expect("connection versions lock")
            .get(&manifest.id)
            .cloned();
        let subject = self.current_quota_subject(manifest);
        let observations = self
            .source_health
            .0
            .read()
            .expect("source observations lock");
        let observation = observations.get(&manifest.id).filter(|entry| {
            version
                .as_ref()
                .is_some_and(|version| Arc::ptr_eq(version, &entry.version))
                && subject.as_deref() == Some(entry.subject.as_str())
        });
        json!({
            "credential":{"present":present,"usable":usable,"required":!local},
            "paused":self.provider_paused(&manifest.id),
            "last_check":observation.and_then(|entry|entry.last_check.as_ref()),
            "last_real_success_at":observation.and_then(|entry|entry.last_real_success_at),
            "directory":{
                "fetched_at":directory["fetched_at"],"checked_at":directory["checked_at"],
                "stale":directory["stale"],"issue":directory["issue"]
            },
            "quota":source.map(|source|source.quota.view(now)),
            "cost":{
                "basis":if confirmed {"confirmed_free"} else if authorized {"authorized_subscription"} else {"unconfirmed"},
                "connection_identity":identity,
                "subscription_eligible":self.subscription_transport(manifest),
                "subscription_billing_notice": (self.subscription_transport(manifest)
                    && manifest.provider.base=="https://api.kimi.com/coding/v1")
                    .then_some("kimi_vendor_extra_usage"),
                "authorized":authorized
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_facts_keep_checks_real_tasks_and_credential_identity_separate() {
        let manifest: Manifest =
            serde_json::from_value(json!({"id":"vendor","version":"1","kind":"free_tier",
            "provider":{"base":"https://vendor.invalid/v1","auth":"bearer","protocol":"chat"},
            "free_models":["m"],"permissions":{"net":["vendor.invalid"],"store_key":true}}))
            .unwrap();
        let state = GatewayState::with_keys(
            BTreeMap::from([("vendor".into(), manifest.clone())]),
            "fixture",
            BTreeMap::from([("vendor".into(), "synthetic-key".into())]),
        );
        let (_, version) = state.dispatch_credential(&manifest).unwrap();
        let subject = state.current_quota_subject(&manifest).unwrap();
        state.record_source_check("vendor", &version, &subject, "ok", "m", 100);
        let health = state.source_health(&manifest, 101);
        assert_eq!(health["last_check"]["observed_at"], 100);
        assert!(health["last_real_success_at"].is_null());
        assert!(health["quota"]["remaining"].is_null());
        state.record_source_success("vendor", &version, &subject, 102);
        assert_eq!(
            state.source_health(&manifest, 103)["last_real_success_at"],
            102
        );
        state
            .connection_versions
            .write()
            .unwrap()
            .insert("vendor".into(), Arc::new(()));
        let changed = state.source_health(&manifest, 104);
        assert!(changed["last_check"].is_null());
        assert!(changed["last_real_success_at"].is_null());
        assert!(!changed.to_string().contains("synthetic-key"));
    }
}

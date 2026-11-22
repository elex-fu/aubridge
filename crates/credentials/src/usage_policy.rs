//! Decision 52: user authorization is separate from a provider billing fact.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostIntent {
    ConfirmedFreeOnly,
    FreeThenSubscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionBasis {
    PersonalChannel,
    UserDeclared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionAuthorization {
    pub provider: String,
    pub basis: SubscriptionBasis,
    pub connection_identity: String,
}

/// Stable across restarts, excludes credentials and never exposes the endpoint.
pub fn connection_identity(manifest: &manifest::schema::Manifest) -> String {
    let bytes =
        serde_json::to_vec(&(&manifest.id, &manifest.provider, manifest.kind)).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

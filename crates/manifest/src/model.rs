//! Optional vendor facts are distinct from a configured routing protocol.
use serde::{Deserialize, Serialize};

use crate::schema::WireProtocol;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreeEvidence {
    #[default]
    Unknown,
    DeclaredQuota,
    VendorZeroPrice,
    Subscription,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FactSource {
    #[default]
    Declared,
    VendorDirectory,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
    #[serde(default)]
    pub cache_write_per_million: Option<f64>,
}

impl ModelPrice {
    pub fn valid(&self) -> bool {
        [
            Some(self.input_per_million),
            Some(self.output_per_million),
            self.cache_read_per_million,
            self.cache_write_per_million,
        ]
        .into_iter()
        .flatten()
        .all(|price| price.is_finite() && price >= 0.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFacts {
    pub source: FactSource,
    pub name: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    pub tools: Option<bool>,
    pub image_input: Option<bool>,
    pub reasoning: Option<bool>,
    pub reasoning_efforts: Vec<String>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub protocol: Option<WireProtocol>,
    pub price: Option<ModelPrice>,
    pub free_evidence: FreeEvidence,
    pub observed_at: Option<u64>,
}

/// Explicit user comparison, never an assertion of the served model's tariff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkMapping {
    pub provider: String,
    pub model: String,
    pub target_provider: String,
    pub target_model: String,
    pub source: String,
    pub version: String,
    pub observed_at: u64,
    pub price: ModelPrice,
}

impl BenchmarkMapping {
    pub fn valid(&self) -> bool {
        [
            &self.provider,
            &self.model,
            &self.target_provider,
            &self.target_model,
            &self.source,
            &self.version,
        ]
        .into_iter()
        .all(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
            && self.observed_at > 0
            && self.price.valid()
    }
}

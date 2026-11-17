//! The public OpenRouter directory is reduced to locally usable free text models.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const OPENROUTER_MODELS: &str = "https://openrouter.ai/api/v1/models";
const DIRECTORY_LIMIT: usize = 4 * 1024 * 1024;
const MODEL_LIMIT: usize = 128;
const STALE_AFTER: u64 = 86_400;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Model {
    pub id: String,
    pub tools: bool,
    pub context: Option<u64>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub fetched_at: u64,
    pub models: Vec<Model>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Issue {
    Network,
    HttpStatus,
    InvalidDirectory,
    Storage,
}

pub(crate) enum RefreshError {
    Unavailable,
    Busy,
}

#[derive(Serialize)]
pub(crate) struct View {
    fetched_at: Option<u64>,
    stale: bool,
    issue: Option<Issue>,
    models: Vec<Model>,
}

#[derive(Default)]
struct Cache {
    snapshot: Option<Snapshot>,
    issue: Option<Issue>,
}

pub(crate) struct OpenRouterCatalog {
    endpoint: String,
    pub refresh: tokio::sync::Mutex<()>,
    cache: RwLock<Cache>,
}

impl OpenRouterCatalog {
    pub(crate) fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            refresh: tokio::sync::Mutex::new(()),
            cache: RwLock::default(),
        }
    }

    pub(crate) fn view(&self, now: u64) -> View {
        let cache = self.cache.read().expect("catalog lock");
        let fetched_at = cache.snapshot.as_ref().map(|snapshot| snapshot.fetched_at);
        View {
            fetched_at,
            stale: fetched_at
                .is_some_and(|at| cache.issue.is_some() || now.saturating_sub(at) >= STALE_AFTER),
            issue: cache.issue,
            models: cache
                .snapshot
                .as_ref()
                .map_or_else(Vec::new, |snapshot| snapshot.models.clone()),
        }
    }

    pub(crate) fn complete(&self, snapshot: Result<Snapshot, Issue>) {
        let mut cache = self.cache.write().expect("catalog lock");
        match snapshot {
            Ok(snapshot) => {
                cache.snapshot = Some(snapshot);
                cache.issue = None;
            }
            Err(issue) => cache.issue = Some(issue),
        }
    }

    pub(crate) async fn fetch(&self, http: &reqwest::Client) -> Result<Snapshot, Issue> {
        let response = http
            .get(&self.endpoint)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| Issue::Network)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Issue::HttpStatus);
        }
        let bytes = crate::stream::read_limited(response, DIRECTORY_LIMIT)
            .await
            .map_err(|_| Issue::InvalidDirectory)?;
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| Issue::InvalidDirectory)?;
        let entries = payload["data"].as_array().ok_or(Issue::InvalidDirectory)?;
        if !entries.iter().any(|item| {
            item["id"].as_str().is_some_and(|id| !id.is_empty())
                && recognizable_price(&item["pricing"]["prompt"])
                && recognizable_price(&item["pricing"]["completion"])
        }) {
            return Err(Issue::InvalidDirectory);
        }
        let mut seen = BTreeSet::new();
        let mut models = BTreeMap::new();
        for item in entries {
            let Some(id) = item["id"].as_str().filter(|id| free_id(id)) else {
                continue;
            };
            if !seen.insert(id) {
                models.remove(id);
                continue;
            }
            if let Some(model) = parse_model(id, item) {
                models.insert(id, model);
            }
        }
        if models.len() > MODEL_LIMIT {
            return Err(Issue::InvalidDirectory);
        }
        Ok(Snapshot {
            fetched_at: crate::upstream::unix_now(),
            models: models.into_values().collect(),
        })
    }
}

impl Snapshot {
    pub(crate) fn from_cache(json: &str) -> Option<Self> {
        let snapshot: Self = serde_json::from_str(json).ok()?;
        (snapshot.fetched_at > 0
            && snapshot.models.len() <= MODEL_LIMIT
            && snapshot.models.iter().all(|model| free_id(&model.id))
            && snapshot
                .models
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id))
        .then_some(snapshot)
    }
}

fn parse_model(id: &str, item: &Value) -> Option<Model> {
    let prices = item["pricing"].as_object()?;
    let architecture = &item["architecture"];
    if !prices.contains_key("prompt")
        || !prices.contains_key("completion")
        || !prices.iter().all(|(key, value)| {
            zero_price(value) || (key == "discount" && value.as_u64() == Some(0))
        })
        || !declares(&architecture["input_modalities"], "text")
        || !declares(&architecture["output_modalities"], "text")
    {
        return None;
    }
    Some(Model {
        id: id.into(),
        tools: declares(&item["supported_parameters"], "tools"),
        context: item["context_length"].as_u64().filter(|count| *count > 0),
    })
}

fn declares(field: &Value, value: &str) -> bool {
    field
        .as_array()
        .is_some_and(|entries| entries.iter().any(|entry| entry == value))
}

pub(crate) fn free_id(id: &str) -> bool {
    let slug = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    };
    id.len() <= 160
        && id
            .strip_suffix(":free")
            .and_then(|base| base.split_once('/'))
            .is_some_and(|(owner, name)| slug(owner) && slug(name))
}

fn recognizable_price(value: &Value) -> bool {
    value
        .as_str()
        .and_then(|price| price.parse::<f64>().ok())
        .is_some_and(|price| price.is_finite() && price.is_sign_positive())
}

fn zero_price(value: &Value) -> bool {
    // Test the exact decimal. A tiny nonzero value can round to zero as a float.
    let zeros = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte == b'0');
    value.as_str().is_some_and(|price| {
        price.split_once('.').map_or_else(
            || zeros(price),
            |(whole, decimal)| zeros(whole) && zeros(decimal),
        )
    })
}

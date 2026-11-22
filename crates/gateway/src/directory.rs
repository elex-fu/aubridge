//! Complete, identity-bound vendor directories. No network IO in routing reads.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use manifest::model::{FreeEvidence, ModelFacts, ModelPrice};
use manifest::schema::{AuthKind, Manifest, ProviderKind, WireProtocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::GatewayState;
use crate::route_plan::Credential;

const STALE_AFTER: u64 = 86_400;
const BODY_LIMIT: usize = 4 * 1024 * 1024;
const MODEL_LIMIT: usize = 1000;
const PAGE_LIMIT: usize = 10;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Model {
    pub id: String,
    pub facts: ModelFacts,
}

#[derive(Clone, Deserialize, Serialize)]
struct Snapshot {
    base: String,
    subject: String,
    fetched_at: u64,
    models: Vec<Model>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Issue {
    UnsupportedProvider,
    MissingCredential,
    ProviderChanged,
    Network,
    Timeout,
    Authentication,
    HttpStatus,
    InvalidDirectory,
    TooLarge,
    Storage,
    Busy,
}

#[derive(Default)]
struct Entry {
    snapshot: Option<Snapshot>,
    checked_subject: Option<String>,
    checked_at: Option<u64>,
    issue: Option<Issue>,
}

#[derive(Default)]
pub(super) struct Directories {
    entries: RwLock<BTreeMap<String, Entry>>,
    refresh: tokio::sync::Mutex<()>,
}

#[derive(Serialize)]
pub(crate) struct View {
    pub(super) fetched_at: Option<u64>,
    checked_at: Option<u64>,
    pub(super) stale: bool,
    issue: Option<Issue>,
    pub(super) models: Vec<Model>,
}

impl GatewayState {
    pub(super) fn load_model_selections(&self, selections: &BTreeMap<String, Vec<String>>) {
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut providers = self.providers.write().expect("providers lock");
        for (id, models) in selections {
            if models.is_empty()
                || models.len() > MODEL_LIMIT
                || !models.iter().all(|model| valid_id(model))
            {
                continue;
            }
            if let (Some(manifest), Some(provider)) = (manifests.get_mut(id), providers.get_mut(id))
            {
                if manifest.kind != ProviderKind::Subscription {
                    continue;
                }
                manifest.free_models = models.clone();
                manifest.model_facts.retain(|id, _| models.contains(id));
                manifest.context_windows.retain(|id, _| models.contains(id));
                manifest
                    .models_without_tools
                    .retain(|id| models.contains(id));
                provider.models = models.clone();
            }
        }
    }

    pub(crate) fn select_directory_models(
        &self,
        id: &str,
        models: &[String],
    ) -> Result<(), String> {
        let (config, _) = self.settings.as_ref().ok_or("设置存储未接线")?;
        let _agents = self.agents_io.lock().expect("agent config lock");
        let _settings = self.settings_io.lock().expect("settings lock");
        let manifest = self.manifest(id).ok_or("来源不存在")?;
        if manifest.kind != ProviderKind::Subscription {
            return Err("订阅源可从目录选择；免费源仍按已确认免费清单供应".into());
        }
        if models.is_empty()
            || models.len() > MODEL_LIMIT
            || !models.iter().all(|model| valid_id(model))
        {
            return Err("请选择有效的模型清单".into());
        }
        let view = self.directory_view(&manifest);
        if view.stale || view.fetched_at.is_none() || view.issue.is_some() {
            return Err("请先成功刷新当前来源的完整目录".into());
        }
        if models
            .iter()
            .any(|id| !view.models.iter().any(|model| &model.id == id))
        {
            return Err("只能选择厂商当前目录中的模型".into());
        }
        let mut seen = BTreeSet::new();
        let models: Vec<_> = models
            .iter()
            .filter(|model| seen.insert((*model).clone()))
            .cloned()
            .collect();
        self.check_custom_provider_supply(id, Some((&models, None)))?;
        let mut settings =
            credentials::settings::Settings::load_checked(config).map_err(|_| "读取配置失败")?;
        settings.model_selections.insert(id.into(), models.to_vec());
        settings.save(config).map_err(|_| "保存模型选择失败")?;
        self.load_model_selections(&BTreeMap::from([(id.into(), models.to_vec())]));
        self.mark_dirty(id);
        Ok(())
    }

    pub(crate) fn directory_view(&self, manifest: &Manifest) -> View {
        if manifest.id == "openrouter" {
            if let Some(catalog) = &self.openrouter_catalog {
                let view = catalog.view(crate::upstream::unix_now());
                return View {
                    fetched_at: view.fetched_at,
                    checked_at: view.fetched_at,
                    stale: view.stale || view.fetched_at.is_none(),
                    issue: view.issue.map(|_| Issue::InvalidDirectory),
                    models: view
                        .models
                        .into_iter()
                        .map(|model| Model {
                            id: model.id,
                            facts: model.facts,
                        })
                        .collect(),
                };
            }
        }
        let subject = self.current_quota_subject(manifest);
        let entries = self.directories.entries.read().expect("directories lock");
        let entry = entries.get(&manifest.id);
        let checked = entry.filter(|entry| entry.checked_subject.as_deref() == subject.as_deref());
        let snapshot = entry
            .and_then(|entry| entry.snapshot.as_ref())
            .filter(|snapshot| {
                snapshot.base == manifest.provider.base
                    && Some(snapshot.subject.as_str()) == subject.as_deref()
            });
        let now = crate::upstream::unix_now();
        View {
            fetched_at: snapshot.map(|snapshot| snapshot.fetched_at),
            checked_at: checked.and_then(|entry| entry.checked_at),
            stale: snapshot.is_none_or(|snapshot| {
                now < snapshot.fetched_at || now - snapshot.fetched_at >= STALE_AFTER
            }),
            issue: checked.and_then(|entry| entry.issue).or_else(|| {
                entry.and_then(|entry| {
                    matches!(entry.issue, Some(Issue::Storage)).then_some(Issue::Storage)
                })
            }),
            models: snapshot.map_or_else(Vec::new, |snapshot| snapshot.models.clone()),
        }
    }

    pub(super) fn load_directories(&self, ledger: &ledger::Ledger) {
        let ids: Vec<_> = self
            .manifests
            .read()
            .expect("manifests lock")
            .keys()
            .cloned()
            .collect();
        let mut entries = self.directories.entries.write().expect("directories lock");
        for id in ids {
            match ledger.load_provider_catalog(&format!("live/{id}")) {
                Ok(Some(json)) => {
                    let snapshot =
                        serde_json::from_str::<Snapshot>(&json)
                            .ok()
                            .filter(|snapshot| {
                                snapshot.models.len() <= MODEL_LIMIT
                                    && snapshot.fetched_at > 0
                                    && snapshot.models.iter().all(|model| {
                                        valid_id(&model.id)
                                            && model
                                                .facts
                                                .price
                                                .as_ref()
                                                .is_none_or(ModelPrice::valid)
                                    })
                                    && snapshot
                                        .models
                                        .windows(2)
                                        .all(|pair| pair[0].id < pair[1].id)
                            });
                    let issue = snapshot.is_none().then_some(Issue::Storage);
                    entries.insert(
                        id,
                        Entry {
                            checked_subject: snapshot
                                .as_ref()
                                .map(|snapshot| snapshot.subject.clone()),
                            snapshot,
                            issue,
                            checked_at: None,
                        },
                    );
                }
                Ok(None) => {}
                Err(_) => {
                    entries.insert(
                        id,
                        Entry {
                            issue: Some(Issue::Storage),
                            ..Entry::default()
                        },
                    );
                }
            }
        }
    }

    pub(crate) fn directory_availability(&self, manifest: &Manifest, model: &str) -> Option<bool> {
        let subject = self.current_quota_subject(manifest)?;
        let entries = self.directories.entries.read().expect("directories lock");
        let snapshot = entries.get(&manifest.id)?.snapshot.as_ref()?;
        let now = crate::upstream::unix_now();
        if snapshot.base != manifest.provider.base
            || snapshot.subject != subject
            || now < snapshot.fetched_at
            || now - snapshot.fetched_at >= STALE_AFTER
        {
            return None;
        }
        Some(
            snapshot
                .models
                .binary_search_by(|row| row.id.as_str().cmp(model))
                .is_ok(),
        )
    }

    pub(super) fn apply_directory(&self, manifest: &mut Manifest) {
        if manifest.id == "openrouter" {
            return;
        }
        let Some(subject) = self.current_quota_subject(manifest) else {
            return;
        };
        let entries = self.directories.entries.read().expect("directories lock");
        let Some(snapshot) = entries
            .get(&manifest.id)
            .and_then(|entry| entry.snapshot.as_ref())
        else {
            return;
        };
        let now = crate::upstream::unix_now();
        if snapshot.subject != subject
            || snapshot.base != manifest.provider.base
            || now < snapshot.fetched_at
            || now - snapshot.fetched_at >= STALE_AFTER
        {
            return;
        }
        for id in &manifest.free_models {
            let Ok(index) = snapshot.models.binary_search_by(|model| model.id.cmp(id)) else {
                continue;
            };
            let model = &snapshot.models[index];
            if let Some(context) = model.facts.context {
                manifest.context_windows.insert(model.id.clone(), context);
            }
            match model.facts.tools {
                Some(false) => {
                    manifest.models_without_tools.insert(model.id.clone());
                }
                Some(true) => {
                    manifest.models_without_tools.remove(&model.id);
                }
                None => {}
            }
            manifest
                .model_facts
                .insert(model.id.clone(), model.facts.clone());
        }
    }

    pub(crate) fn configured_model_facts(
        &self,
        manifest: &Manifest,
    ) -> BTreeMap<String, ModelFacts> {
        manifest
            .free_models
            .iter()
            .map(|id| {
                let mut facts = manifest.model_facts.get(id).cloned().unwrap_or_default();
                facts.protocol = Some(manifest.provider.protocol);
                if facts.context.is_none() {
                    facts.context = manifest.context_windows.get(id).copied();
                }
                if manifest.models_without_tools.contains(id) {
                    facts.tools = Some(false);
                }
                if facts.free_evidence == FreeEvidence::Unknown {
                    facts.free_evidence = if manifest.kind == ProviderKind::Subscription {
                        FreeEvidence::Subscription
                    } else {
                        FreeEvidence::DeclaredQuota
                    };
                }
                (id.clone(), facts)
            })
            .collect()
    }

    pub(crate) fn model_facts(&self) -> BTreeMap<String, ModelFacts> {
        let mut collected: BTreeMap<String, Vec<ModelFacts>> = BTreeMap::new();
        for (_, manifest) in self.routing_sources() {
            for (id, facts) in self.configured_model_facts(&manifest) {
                if self.directory_availability(&manifest, &id) != Some(false) {
                    collected.entry(id).or_default().push(facts);
                }
            }
        }
        collected
            .into_iter()
            .map(|(id, values)| {
                let mut facts = values[0].clone();
                // A shared alias must not promise a limit or capability its other
                // suppliers haven't confirmed. Routing still checks each source.
                facts.context = values
                    .iter()
                    .map(|value| value.context)
                    .collect::<Option<Vec<_>>>()
                    .and_then(|values| values.into_iter().min());
                facts.output = values
                    .iter()
                    .map(|value| value.output)
                    .collect::<Option<Vec<_>>>()
                    .and_then(|values| values.into_iter().min());
                for field in [0, 1, 2] {
                    let get = |value: &ModelFacts| match field {
                        0 => value.tools,
                        1 => value.image_input,
                        _ => value.reasoning,
                    };
                    let common = values
                        .iter()
                        .all(|value| get(value) == get(&values[0]))
                        .then(|| get(&values[0]))
                        .flatten();
                    match field {
                        0 => facts.tools = common,
                        1 => facts.image_input = common,
                        _ => facts.reasoning = common,
                    }
                }
                if values.iter().any(|value| value.protocol != facts.protocol) {
                    facts.protocol = None;
                }
                if values.iter().any(|value| value.price != facts.price) {
                    facts.price = None;
                }
                facts.reasoning_efforts.retain(|level| {
                    values
                        .iter()
                        .all(|value| value.reasoning_efforts.contains(level))
                });
                facts.input_modalities.retain(|kind| {
                    values
                        .iter()
                        .all(|value| value.input_modalities.contains(kind))
                });
                facts.output_modalities.retain(|kind| {
                    values
                        .iter()
                        .all(|value| value.output_modalities.contains(kind))
                });
                facts.observed_at = values
                    .iter()
                    .map(|value| value.observed_at)
                    .collect::<Option<Vec<_>>>()
                    .and_then(|values| values.into_iter().min());
                if values
                    .iter()
                    .any(|value| value.free_evidence != facts.free_evidence)
                {
                    facts.free_evidence = FreeEvidence::Unknown;
                }
                (id, facts)
            })
            .collect()
    }

    async fn refresh_directory(&self, manifest: &Manifest) -> Result<(), Issue> {
        let _guard = self
            .directories
            .refresh
            .try_lock()
            .map_err(|_| Issue::Busy)?;
        let (credential, version) = self
            .dispatch_credential(manifest)
            .map_err(|_| Issue::MissingCredential)?;
        let login = if matches!(credential, Credential::ChatGpt) {
            Some(
                self.chatgpt
                    .quota_snapshot()
                    .ok_or(Issue::MissingCredential)?,
            )
        } else {
            None
        };
        let subject = self
            .current_quota_subject(manifest)
            .ok_or(Issue::MissingCredential)?;
        let checked_at = crate::upstream::unix_now();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            fetch(self, manifest, &credential, login.as_ref()),
        )
        .await
        .unwrap_or(Err(Issue::Timeout));
        let mut published = false;
        let mut stored_result = Ok(());
        self.update_bound_connection(&manifest.id, Some(&version), login.as_ref(), None, || {
            if self
                .current_quota_subject_unlocked(manifest, login.as_ref())
                .as_deref()
                != Some(subject.as_str())
            {
                return;
            }
            let result = result.and_then(|models| {
                let snapshot = Snapshot {
                    base: manifest.provider.base.clone(),
                    subject: subject.clone(),
                    fetched_at: checked_at,
                    models,
                };
                if let Some(ledger) = &self.ledger {
                    let json = serde_json::to_string(&snapshot).map_err(|_| Issue::Storage)?;
                    ledger
                        .lock()
                        .map_err(|_| Issue::Storage)?
                        .save_provider_catalog(&format!("live/{}", manifest.id), &json)
                        .map_err(|_| Issue::Storage)?;
                }
                Ok(snapshot)
            });
            let mut entries = self.directories.entries.write().expect("directories lock");
            let entry = entries.entry(manifest.id.clone()).or_default();
            entry.checked_at = Some(checked_at);
            entry.checked_subject = Some(subject.clone());
            match result {
                Ok(snapshot) => {
                    entry.snapshot = Some(snapshot);
                    entry.issue = None;
                }
                Err(issue) => {
                    entry.issue = Some(issue);
                    stored_result = Err(issue);
                }
            }
            published = true;
        });
        if !published {
            return Err(Issue::ProviderChanged);
        }
        stored_result
    }

    // Called under the login guard; never reconcile that same lock recursively.
    fn current_quota_subject_unlocked(
        &self,
        manifest: &Manifest,
        login: Option<&crate::chatgpt::LoginSnapshot>,
    ) -> Option<String> {
        login
            .map(|login| login.quota_subject())
            .or_else(|| self.current_quota_subject(manifest))
    }
}

pub(crate) async fn route(
    state: &GatewayState,
    headers: &HeaderMap,
    id: &str,
    refresh: bool,
) -> Response<Body> {
    if !state.is_valid_admin_key(crate::security::caller_key(headers).as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"admin key required"})),
        )
            .into_response();
    }
    let Some(manifest) = state.manifest(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if refresh {
        if let Err(issue) = state.refresh_directory(&manifest).await {
            if matches!(
                issue,
                Issue::MissingCredential | Issue::ProviderChanged | Issue::Busy
            ) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"issue":issue,"error":match issue {
                        Issue::MissingCredential=>"当前来源凭据不可用",
                        Issue::ProviderChanged=>"来源或登录已变化，请重新刷新",
                        _=>"另一目录正在刷新",
                    }})),
                )
                    .into_response();
            }
        }
    }
    if refresh {
        return crate::sync_opencode_models(
            state,
            serde_json::json!(state.directory_view(&manifest)),
        )
        .into_response();
    }
    (
        [("cache-control", "no-store")],
        Json(state.directory_view(&manifest)),
    )
        .into_response()
}

pub(crate) fn price_source(manifest: &Manifest) -> String {
    let base = manifest.provider.base.trim_end_matches('/');
    if manifest.provider.protocol == WireProtocol::Anthropic
        && reqwest::Url::parse(base)
            .ok()
            .is_some_and(|url| url.path().trim_matches('/').is_empty())
    {
        format!("{base}/v1/models")
    } else {
        format!("{base}/models")
    }
}

async fn fetch(
    state: &GatewayState,
    manifest: &Manifest,
    credential: &Credential,
    login: Option<&crate::chatgpt::LoginSnapshot>,
) -> Result<Vec<Model>, Issue> {
    let path = price_source(manifest);
    let mut url = reqwest::Url::parse(&path).map_err(|_| Issue::InvalidDirectory)?;
    if login.is_some() {
        let version = crate::chatgpt::CODEX_USER_AGENT
            .strip_prefix("codex_cli_rs/")
            .and_then(|value| value.split_whitespace().next())
            .ok_or(Issue::InvalidDirectory)?;
        url.query_pairs_mut().append_pair("client_version", version);
    }
    let mut cursors = BTreeSet::new();
    let mut models = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut bytes_left = BODY_LIMIT;
    let at = crate::upstream::unix_now();
    for _ in 0..PAGE_LIMIT {
        let mut request = state.http().get(url.clone());
        request = match credential {
            Credential::Key(key) => match manifest.provider.auth {
                AuthKind::ClaudeCli => request,
                AuthKind::Bearer => request.bearer_auth(key),
                AuthKind::ApiKey => request.header("x-api-key", key),
            },
            Credential::ClaudeCli => return Err(Issue::UnsupportedProvider),
            Credential::Local => request,
            Credential::ChatGpt => {
                let login = login.ok_or(Issue::MissingCredential)?;
                let mut request = request
                    .bearer_auth(&login.access_token)
                    .header("originator", crate::chatgpt::CODEX_ORIGINATOR)
                    .header("user-agent", crate::chatgpt::CODEX_USER_AGENT);
                if let Some(account) = credentials::chatgpt::resolve_account_id(login) {
                    request = request.header("chatgpt-account-id", account);
                }
                request
            }
        };
        if manifest.provider.protocol == WireProtocol::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = request.send().await.map_err(|_| Issue::Network)?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(Issue::Authentication),
            _ => return Err(Issue::HttpStatus),
        }
        let bytes = crate::stream::read_limited(response, bytes_left)
            .await
            .map_err(|reason| {
                if reason == "reply-too-large" {
                    Issue::TooLarge
                } else {
                    Issue::Network
                }
            })?;
        bytes_left = bytes_left.saturating_sub(bytes.len());
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| Issue::InvalidDirectory)?;
        if payload.get("error").is_some()
            || payload["success"].as_bool() == Some(false)
            || payload
                .get("code")
                .is_some_and(|code| !matches!(code.as_i64(), Some(0 | 200)))
        {
            return Err(Issue::InvalidDirectory);
        }
        let entries = payload
            .get("data")
            .or_else(|| payload.get("models"))
            .and_then(Value::as_array)
            .ok_or(Issue::InvalidDirectory)?;
        for entry in entries {
            let Some(id) = entry
                .get("id")
                .or_else(|| entry.get("slug"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| valid_id(id))
            else {
                return Err(Issue::InvalidDirectory);
            };
            if duplicates.contains(id) {
                continue;
            }
            let model = Model {
                id: id.into(),
                facts: parse_facts(entry, manifest.kind, manifest.provider.protocol, at),
            };
            if let Some(previous) = models.get(id) {
                if serde_json::to_value(previous).ok() != serde_json::to_value(&model).ok() {
                    models.remove(id);
                    duplicates.insert(id.to_string());
                }
            } else {
                models.insert(id.to_string(), model);
            }
            if models.len() + duplicates.len() > MODEL_LIMIT {
                return Err(Issue::TooLarge);
            }
        }
        match payload.get("has_more") {
            None | Some(Value::Bool(false)) => return Ok(models.into_values().collect()),
            Some(Value::Bool(true)) => {}
            _ => return Err(Issue::InvalidDirectory),
        }
        let cursor = payload["last_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(Issue::InvalidDirectory)?;
        if !cursors.insert(cursor.to_string()) || bytes_left == 0 {
            return Err(Issue::InvalidDirectory);
        }
        // Replace only our own cursor, retaining the original vendor query.
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key != "after")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("after", cursor);
    }
    Err(Issue::InvalidDirectory)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && !id.contains(|ch: char| ch.is_control() || ch == ',')
}

pub(crate) fn parse_facts(
    row: &Value,
    kind: ProviderKind,
    protocol: WireProtocol,
    at: u64,
) -> ModelFacts {
    let strings = |value: &Value| {
        value.as_array().map_or_else(Vec::new, |values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| {
                    !value.is_empty() && value.len() <= 64 && !value.contains(char::is_control)
                })
                .take(32)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
    };
    let input = strings(
        row["architecture"]
            .get("input_modalities")
            .unwrap_or(&row["input_modalities"]),
    );
    let output = strings(
        row["architecture"]
            .get("output_modalities")
            .unwrap_or(&row["output_modalities"]),
    );
    let parameters = row["supported_parameters"].as_array();
    let efforts = row["supported_reasoning_levels"]
        .as_array()
        .map_or_else(Vec::new, |levels| {
            levels
                .iter()
                .filter_map(|level| level.as_str().or_else(|| level["effort"].as_str()))
                .filter(|level| level.len() <= 32 && !level.contains(char::is_control))
                .take(16)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
    let number = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| row[*name].as_u64().filter(|value| *value > 0))
    };
    let rate = |name: &str| {
        row["pricing"][name]
            .as_str()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value * 1e6)
            .filter(|value| value.is_finite())
    };
    let price =
        rate("prompt")
            .zip(rate("completion"))
            .map(|(input_per_million, output_per_million)| ModelPrice {
                input_per_million,
                output_per_million,
                cache_read_per_million: rate("input_cache_read"),
                cache_write_per_million: rate("input_cache_write"),
            });
    ModelFacts {
        source: manifest::model::FactSource::VendorDirectory,
        name: row
            .get("display_name")
            .or_else(|| row.get("name"))
            .and_then(Value::as_str)
            .filter(|name| name.len() <= 160 && !name.contains(char::is_control))
            .map(str::to_owned),
        context: number(&["context_window", "context_length"]),
        output: number(&["max_output_tokens", "max_completion_tokens"]).or_else(|| {
            row["top_provider"]["max_completion_tokens"]
                .as_u64()
                .filter(|value| *value > 0)
        }),
        tools: row["capabilities"]["tools"]
            .as_bool()
            .or_else(|| row["supports_tools"].as_bool())
            .or_else(|| {
                parameters.map(|parameters| parameters.iter().any(|value| value == "tools"))
            }),
        image_input: row["supports_image_input"]
            .as_bool()
            .or_else(|| (!input.is_empty()).then(|| input.iter().any(|value| value == "image"))),
        reasoning: row["supports_reasoning"]
            .as_bool()
            .or_else(|| {
                parameters.map(|values| {
                    values
                        .iter()
                        .any(|value| value == "reasoning" || value == "include_reasoning")
                })
            })
            .or_else(|| {
                row.get("supported_reasoning_levels")
                    .map(|_| !efforts.is_empty())
            }),
        reasoning_efforts: efforts,
        input_modalities: input,
        output_modalities: output,
        protocol: Some(protocol),
        price,
        free_evidence: if kind == ProviderKind::Subscription {
            FreeEvidence::Subscription
        } else {
            FreeEvidence::DeclaredQuota
        },
        observed_at: Some(at),
    }
}

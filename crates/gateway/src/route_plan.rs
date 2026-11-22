//! Filter credentials and request capabilities before spending the retry budget.

use std::collections::{BTreeMap, BTreeSet};

use axum::http::HeaderMap;
use manifest::schema::{Manifest, ProviderKind, WireProtocol};
use manifest::state::{AccountRestReason, ModelCooldownReason, ProviderState};
use proto::protocol::{
    InboundBlock, InboundContent, MessagesRequest, SystemContent, ToolResultContent,
};
use router::GroupRule;

use crate::state::GatewayState;

pub(crate) enum Credential {
    Key(String),
    ChatGpt,
    ClaudeCli,
    Local,
}

pub(crate) struct Candidate {
    pub manifest: Manifest,
    pub model: String,
    pub codex: bool,
}

impl Candidate {
    pub(crate) fn context_window(&self) -> Option<u64> {
        self.manifest.context_windows.get(&self.model).copied()
    }
}

const SMALL_CONTEXT: &str = "context-window-too-small";
pub(crate) const PROVIDER_PAUSED: &str = "provider-paused";
pub(crate) const PROVIDER_CHANGED: &str = "provider-changed";
pub(crate) const INVALID_PROVIDER_URL: &str = "invalid-provider-url";

#[derive(Clone, Copy, serde::Serialize)]
pub(crate) struct Block {
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<u64>,
    #[serde(skip)]
    throttled_until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_input_tokens: Option<u64>,
}

impl Block {
    pub(crate) fn new(reason: &'static str, retry_at: Option<u64>) -> Self {
        Self {
            reason,
            retry_at,
            throttled_until: None,
            context_window: None,
            estimated_input_tokens: None,
        }
    }

    pub(crate) fn context(reason: &'static str, window: Option<u64>, estimated: u64) -> Self {
        Self {
            context_window: window,
            estimated_input_tokens: Some(estimated),
            ..Self::new(reason, None)
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct Skipped {
    id: String,
    model: String,
    kind: ProviderKind,
    #[serde(flatten)]
    block: Block,
}

impl Skipped {
    pub(crate) fn context_for(&self, candidate: &Candidate) -> bool {
        self.matches(candidate, SMALL_CONTEXT)
    }

    pub(crate) fn matches(&self, candidate: &Candidate, reason: &str) -> bool {
        self.block.reason == reason
            && self.id == candidate.manifest.id
            && self.model == candidate.model
    }
}

pub(crate) const MAX_SKIPPED: usize = 48;

pub(crate) fn record_skip(
    skipped: &mut Vec<Skipped>,
    id: &str,
    model: &str,
    kind: ProviderKind,
    block: Block,
) {
    if skipped.len() < MAX_SKIPPED {
        skipped.push(Skipped {
            id: id.into(),
            model: model.into(),
            kind,
            block,
        });
    }
}

pub(crate) struct Plan {
    pub ordered: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
    pub free_first: bool,
    pub recognized_model: bool,
    pub saved_group: bool,
    blocked_reasons: BTreeSet<&'static str>,
    compatible: Vec<(String, String)>,
}

impl Plan {
    pub(crate) fn has_block(&self, reason: &str) -> bool {
        self.blocked_reasons.contains(reason)
    }

    pub(crate) fn retry_after(&self, state: &GatewayState, now: u64) -> Option<u64> {
        let mut earliest = None;
        for (id, model) in &self.compatible {
            // A later attempt can put an already skipped model's account into cooldown.
            let block = state.route_block(id, model, now)?;
            if block.reason == PROVIDER_PAUSED {
                continue;
            }
            let until = block.throttled_until?;
            earliest = Some(earliest.map_or(until, |at: u64| at.min(until)));
        }
        earliest.map(|until| until - now)
    }

    pub(crate) fn prefer_context(&mut self, request: &MessagesRequest) -> bool {
        if self.ordered.len() < 2
            || !self
                .ordered
                .iter()
                .any(|candidate| candidate.context_window().is_some())
        {
            return false;
        }
        let estimated = translate::count::estimate_tokens(request) as u64;
        let too_small = |candidate: &Candidate| {
            candidate
                .context_window()
                .is_some_and(|window| estimated > window)
        };
        let small = self
            .ordered
            .iter()
            .filter(|candidate| too_small(candidate))
            .count();
        if small == 0 || small == self.ordered.len() {
            return false;
        }
        // An estimate may be wrong. Keep these candidates for the remaining budget.
        self.ordered.sort_by_key(too_small);
        for candidate in self.ordered.iter().filter(|candidate| too_small(candidate)) {
            record_skip(
                &mut self.skipped,
                &candidate.manifest.id,
                &candidate.model,
                candidate.manifest.kind,
                Block::context(SMALL_CONTEXT, candidate.context_window(), estimated),
            );
        }
        true
    }
}

pub(crate) fn plan(
    state: &GatewayState,
    req: &MessagesRequest,
    headers: &HeaderMap,
    now: u64,
    advance_rotation: bool,
    require_tools: bool,
    request_block: impl Fn(&Manifest, &str) -> Option<Block>,
) -> Plan {
    state.reconcile_external_api_key();
    let named_group = state.group_of(&req.model);
    let saved_group = named_group.is_some();
    let (members, rule) = match named_group {
        Some((members, rule)) => (Some(members), rule),
        None => (router::parse_members(&req.model), GroupRule::FreeFirst),
    };
    let auto = req.model == crate::AUTO_MODEL;
    let mut recognized_model = auto || members.is_some();
    let literal = req.model.strip_prefix("free/").unwrap_or(&req.model);
    let (tools, native_only) = request_needs(req);
    let tools = tools || require_tools;

    let mut ready = Vec::new();
    let mut targets = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut blocked_reasons = BTreeSet::new();
    let mut compatible = Vec::new();
    for (mut source, manifest) in state.routing_sources() {
        if !auto && members.is_none() && !source.supplies(&req.model) {
            continue;
        }
        recognized_model = true;
        let allowed = rule.allows_provider(source.kind);
        let paused = state.provider_paused(&source.id);
        let ready_credential = !paused && allowed && credential(state, &manifest).is_some();
        let models = source
            .models
            .iter()
            .filter(|model| {
                members
                    .as_ref()
                    .map_or(auto || model.as_str() == literal, |members| {
                        members.contains(model)
                    })
            })
            .filter_map(|model| {
                let ineligible = (!allowed)
                    .then(|| Block::new("subscription-excluded", None))
                    .or_else(|| paused.then(|| Block::new(PROVIDER_PAUSED, None)))
                    .or_else(|| {
                        (!source.enabled || !source.healthy)
                            .then(|| Block::new("provider-disabled", None))
                    })
                    .or_else(|| (!ready_credential).then(|| Block::new("missing-credential", None)))
                    .or_else(|| {
                        (native_only && manifest.provider.protocol != WireProtocol::Anthropic)
                            .then(|| Block::new("native-protocol-required", None))
                    })
                    .or_else(|| {
                        (state.is_chatgpt(&manifest) && translate::media::has_documents(req))
                            .then(|| Block::new(translate::media::UNSUPPORTED, None))
                    })
                    .or_else(|| {
                        state
                            .cost_block(&manifest, model, now)
                            .map(|reason| Block::new(reason, None))
                    })
                    .or_else(|| request_block(&manifest, model))
                    .or_else(|| {
                        (state.directory_availability(&manifest, model) == Some(false))
                            .then(|| Block::new("model-not-listed", None))
                    })
                    .or_else(|| {
                        (tools && manifest.models_without_tools.contains(model))
                            .then(|| Block::new("tools-unsupported", None))
                    })
                    .or_else(|| {
                        let facts = manifest.model_facts.get(model)?;
                        if translate::media::has_images(req) && facts.image_input == Some(false) {
                            return Some(Block::new(translate::media::IMAGE_UNSUPPORTED, None));
                        }
                        if facts.output.is_some_and(|limit| {
                            u64::try_from(req.max_tokens).is_ok_and(|requested| requested > limit)
                        }) {
                            return Some(Block::new("output-limit-exceeded", None));
                        }
                        let reasoning = req.thinking.as_ref().is_some_and(|thinking| {
                            matches!(thinking["type"].as_str(), Some("enabled" | "adaptive"))
                        });
                        (reasoning && facts.reasoning == Some(false))
                            .then(|| Block::new("reasoning-unsupported", None))
                    });
                let block = ineligible.or_else(|| {
                    compatible.push((source.id.clone(), model.clone()));
                    blocked(&source, model, now)
                });
                if let Some(block) = block {
                    blocked_reasons.insert(block.reason);
                    record_skip(&mut skipped, &source.id, model, source.kind, block);
                    None
                } else {
                    Some(model.clone())
                }
            })
            .collect::<Vec<_>>();
        if models.is_empty() {
            continue;
        }
        if source.rest_until.is_some_and(|until| until <= now) {
            source.sunk = false;
        }
        if source.resets_at.is_some_and(|reset| reset <= now) {
            source.remaining = None;
            source.resets_at = None;
        }
        source.models = models;
        targets.insert(source.id.clone(), manifest);
        ready.push(source);
    }

    let mut ordered = if let Some(members) = &members {
        let rotation = match (rule, advance_rotation) {
            (GroupRule::RoundRobin, true) => state.next_group_rotation(&req.model),
            (GroupRule::RoundRobin, false) => state.group_rotation(&req.model),
            _ => 0,
        };
        router::candidates_named_group(members, rule, &ready, now, rotation)
    } else {
        let ids = if auto {
            router::candidates_auto(&ready, now)
        } else {
            router::candidates(&req.model, &ready, now)
        };
        let sources: Vec<_> = ids
            .iter()
            .filter_map(|id| ready.iter().find(|source| source.id == *id))
            .collect();
        let mut ordered: Vec<_> = sources
            .iter()
            .map(|&source| (source, source.models[0].as_str()))
            .collect();
        if auto {
            // Try each provider's preferred model before using extra variants from its free tier.
            ordered.extend(
                sources
                    .into_iter()
                    .filter(|source| source.kind == ProviderKind::FreeTier)
                    .flat_map(|source| {
                        source
                            .models
                            .iter()
                            .skip(1)
                            .map(move |model| (source, model.as_str()))
                    }),
            );
        }
        ordered
    };
    // Ordered and rotating groups retain their explicit order.
    if rule.prefers_free() {
        if let Some(entry) = session_key(req, headers).and_then(|key| state.stick(&key)) {
            if let Some(manifest) = state.manifest(&entry.main) {
                router::affinity::apply_affinity(
                    &mut ordered,
                    &ready,
                    &entry,
                    manifest.provider.protocol,
                    now,
                    |(source, _)| &source.id,
                );
            }
        }
    }
    let free_first = auto || (members.is_some() && rule.prefers_free());
    if free_first {
        router::group::prioritize_free(&mut ordered, now);
    }
    let ordered = ordered
        .into_iter()
        .filter_map(|(source, model)| {
            let manifest = targets.get(&source.id)?;
            Some(Candidate {
                manifest: manifest.clone(),
                model: model.into(),
                codex: state.is_chatgpt(manifest),
            })
        })
        .collect();
    Plan {
        ordered,
        skipped,
        free_first,
        recognized_model,
        saved_group,
        blocked_reasons,
        compatible,
    }
}

pub(crate) fn blocked(source: &ProviderState, model: &str, now: u64) -> Option<Block> {
    if !source.enabled || !source.healthy {
        return Some(Block::new("provider-disabled", None));
    }
    if let Some(retry_at) = source.key_retry_at.filter(|at| *at > now) {
        return Some(Block::new("credential-rejected", Some(retry_at)));
    }
    if source.is_breaker_open(now) {
        return Some(Block::new("circuit-open", source.breaker.open_until));
    }
    let account_rest = source.rest_until.filter(|until| *until > now);
    let exhausted = source.remaining == Some(0) && source.resets_at.is_none_or(|at| at > now);
    let sunk = source.sunk && source.rest_until.is_none_or(|at| at > now);
    let account = (account_rest.is_some() || exhausted || sunk).then(|| {
        let rest_reason = source.rest_reason.filter(|_| account_rest.is_some());
        let reset = source.resets_at.filter(|_| exhausted);
        let retry_at = if (exhausted && reset.is_none()) || (sunk && account_rest.is_none()) {
            None
        } else {
            account_rest.max(reset)
        };
        let reason = match rest_reason {
            Some(AccountRestReason::Credit) => "account-credit-exhausted",
            Some(AccountRestReason::QuotaExhausted) => "account-quota-exhausted",
            _ if exhausted || sunk => "account-quota-exhausted",
            Some(AccountRestReason::RateLimit) => "account-rate-limit",
            _ => "provider-cooldown",
        };
        let mut block = Block::new(reason, retry_at);
        if matches!(
            rest_reason,
            Some(AccountRestReason::RateLimit | AccountRestReason::QuotaExhausted)
        ) || (rest_reason.is_none() && reset.is_some())
        {
            block.throttled_until = retry_at;
        }
        block
    });
    let model = source.model_cooldown(model, now).map(|cooldown| {
        let mut block = Block::new(model_reason(cooldown.reason), Some(cooldown.until));
        if cooldown.reason != ModelCooldownReason::ModelUnavailable {
            block.throttled_until = Some(cooldown.until);
        }
        block
    });
    match (account, model) {
        (Some(mut account), Some(model)) => {
            account.retry_at = account.retry_at.zip(model.retry_at).map(|(a, b)| a.max(b));
            account.throttled_until = account
                .throttled_until
                .zip(model.throttled_until)
                .map(|(a, b)| a.max(b));
            Some(account)
        }
        (account, model) => account.or(model),
    }
}

pub(crate) fn model_reason(reason: ModelCooldownReason) -> &'static str {
    match reason {
        ModelCooldownReason::ModelRateLimit => "model-rate-limit",
        ModelCooldownReason::ModelQuotaExhausted => "model-quota-exhausted",
        ModelCooldownReason::ModelUnavailable => "model-unavailable",
    }
}

pub(crate) fn session_key(req: &MessagesRequest, headers: &HeaderMap) -> Option<String> {
    router::affinity::session_key(req, |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    })
}

pub(crate) fn credential(state: &GatewayState, manifest: &Manifest) -> Option<Credential> {
    credential_with_key(state, manifest, || state.api_key_opt(&manifest.id))
}

pub(crate) fn credential_with_key(
    state: &GatewayState,
    manifest: &Manifest,
    resolve_key: impl FnOnce() -> Option<String>,
) -> Option<Credential> {
    if manifest.provider.auth == manifest::schema::AuthKind::ClaudeCli {
        return (crate::claude_cli::owns(manifest)
            && !state.provider_paused(crate::claude_cli::ID)
            && state.claude_cli.ready())
        .then_some(Credential::ClaudeCli);
    }
    if state.is_chatgpt(manifest) {
        return state.has_chatgpt_login().then_some(Credential::ChatGpt);
    }
    if let Some(key) = resolve_key().filter(|key| !key.trim().is_empty()) {
        return Some(Credential::Key(key));
    }
    // Local inference servers can run without authentication.
    reqwest::Url::parse(&manifest.provider.base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
        .then_some(Credential::Local)
}

fn request_needs(req: &MessagesRequest) -> (bool, bool) {
    let tools = req.tools.as_deref().unwrap_or_default();
    let mut needs_tools = !tools.is_empty();
    let mut native_only = tools.iter().any(|tool| !tool.input_schema.is_object());
    if let Some(SystemContent::Blocks(blocks)) = &req.system {
        native_only |= blocks
            .iter()
            .any(|block| !block.kind.is_empty() && block.kind != "text");
    }
    for message in &req.messages {
        if let InboundContent::Blocks(blocks) = &message.content {
            for block in blocks {
                match block {
                    InboundBlock::NativeOnly => native_only = true,
                    InboundBlock::ToolUse { .. } => needs_tools = true,
                    InboundBlock::ToolResult { content, .. } => {
                        needs_tools = true;
                        if let ToolResultContent::Blocks(blocks) = content {
                            native_only |=
                                blocks.iter().any(|b| matches!(b, InboundBlock::NativeOnly));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    (needs_tools, native_only)
}

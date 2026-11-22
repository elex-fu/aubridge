//! Native checkpoints are credentials-bound capabilities, never portable summaries.
use crate::{route_plan::Candidate, state::GatewayState};
use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct State {
    owners: BTreeMap<String, String>,
    failures: BTreeMap<String, (u32, u64)>,
}

pub(crate) struct CompactInput {
    pub input: translate::responses_input::ResponsesInput,
    pub raw: Value,
}
impl CompactInput {
    pub fn parse(body: &str) -> Result<Self, &'static str> {
        let raw: Value = serde_json::from_str(body).map_err(|_| "Invalid JSON request")?;
        if !raw.is_object() || (!raw["stream"].is_null() && raw["stream"] != false) {
            return Err("Compact requires a unary request");
        }
        let mut normalized = raw.clone();
        normalized["stream"] = json!(true);
        let mut input = translate::responses_input::ResponsesInput::parse(&normalized.to_string())?;
        input.request.stream = Some(false);
        Ok(Self { input, raw })
    }
    pub fn native(&self, model: &str) -> Value {
        let mut raw = self.raw.clone();
        raw["model"] = json!(model);
        if let Some(object) = raw.as_object_mut() {
            object.remove("stream");
        }
        raw
    }
}

pub(crate) async fn route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    body: String,
) -> Response<Body> {
    if !state.is_valid_key(crate::security::caller_key(&headers).as_deref()) {
        return error(StatusCode::UNAUTHORIZED, "Missing or invalid gateway key");
    }
    if !state.is_serving() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is paused");
    }
    match CompactInput::parse(&body) {
        Ok(input) => crate::upstream::forward_compact(state, &input, &headers).await,
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}
pub(crate) fn error(status: StatusCode, message: &str) -> Response<Body> {
    crate::chat::error_response(
        status,
        message,
        "invalid_request_error",
        Some("compaction_error"),
    )
}
pub(crate) fn trigger(raw: &Value) -> bool {
    raw["input"]
        .as_array()
        .and_then(|v| v.last())
        .is_some_and(|v| v["type"] == "compaction_trigger")
}
pub(crate) fn is_v2_checkpoint(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("compaction" | "compaction_summary")
    )
}
pub(crate) fn is_checkpoint(item: &Value) -> bool {
    is_v2_checkpoint(item)
        || (item["type"] == "context_compaction" && !item["encrypted_content"].is_null())
}
fn digests(raw: &Value) -> Result<Vec<String>, &'static str> {
    let mut out = Vec::new();
    if let Some(items) = raw["input"].as_array() {
        for item in items.iter().filter(|i| is_checkpoint(i)) {
            let blob = item["encrypted_content"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("Invalid checkpoint")?;
            out.push(digest(blob));
        }
    }
    Ok(out)
}
fn digest(blob: &str) -> String {
    credentials::quota::subject_tag("compaction-checkpoint", blob)
}
pub(crate) fn owner(candidate: &Candidate, subject: &str) -> String {
    json!([
        candidate.manifest.id,
        candidate.manifest.provider.base,
        subject
    ])
    .to_string()
}
impl GatewayState {
    fn checkpoint_owner(&self, digest: &str) -> Result<Option<String>, &'static str> {
        if let Some(ledger) = &self.ledger {
            return ledger
                .lock()
                .map_err(|_| "Checkpoint store unavailable")?
                .compaction_owner(digest)
                .map_err(|_| "Checkpoint store unavailable");
        }
        Ok(self
            .compaction
            .lock()
            .map_err(|_| "Checkpoint store unavailable")?
            .owners
            .get(digest)
            .cloned())
    }
    pub(crate) fn history_owner(&self, raw: &Value) -> Result<Option<String>, &'static str> {
        let mut owner = None;
        for hash in digests(raw)? {
            let found = self
                .checkpoint_owner(&hash)?
                .ok_or("Unknown checkpoint; resume using the original gateway data")?;
            if owner.as_ref().is_some_and(|old| old != &found) {
                return Err("Mixed checkpoint owners");
            }
            owner = Some(found);
        }
        Ok(owner)
    }
    pub(crate) fn save_checkpoint(
        &self,
        item: &Value,
        owner: &str,
        now: u64,
    ) -> Result<(), &'static str> {
        if !is_checkpoint(item) {
            return Ok(());
        }
        let blob = item["encrypted_content"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or("Empty checkpoint")?;
        let hash = digest(blob);
        if let Some(ledger) = &self.ledger {
            if !ledger
                .lock()
                .map_err(|_| "Checkpoint store unavailable")?
                .save_compaction_owner(&hash, owner, now)
                .map_err(|_| "Checkpoint store unavailable")?
            {
                return Err("Checkpoint owner conflict");
            }
        } else {
            let mut state = self
                .compaction
                .lock()
                .map_err(|_| "Checkpoint store unavailable")?;
            if state.owners.get(&hash).is_some_and(|old| old != owner) {
                return Err("Checkpoint owner conflict");
            }
            if state.owners.len() >= 4096 && !state.owners.contains_key(&hash) {
                return Err("Checkpoint store full");
            }
            state.owners.insert(hash, owner.into());
        }
        Ok(())
    }
    pub(crate) fn compact_ready(&self, key: &str, now: u64) -> bool {
        self.compaction
            .lock()
            .expect("compaction lock")
            .failures
            .get(key)
            .is_none_or(|(_, until)| now >= *until)
    }
    pub(crate) fn compact_result(&self, key: &str, success: bool, now: u64) {
        let mut state = self.compaction.lock().expect("compaction lock");
        if success {
            state.failures.remove(key);
        } else {
            if state.failures.len() >= 4096 && !state.failures.contains_key(key) {
                return;
            }
            let (count, _) = state.failures.get(key).copied().unwrap_or_default();
            let count = count.saturating_add(1);
            state
                .failures
                .insert(key.into(), (count, now + (5u64 << count.min(6)).min(300)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unary_does_not_relax_stream_contract() {
        let body = json!({"model":"x","input":"hello"}).to_string();
        assert!(CompactInput::parse(&body).is_ok());
        assert!(translate::responses_input::ResponsesInput::parse(&body).is_err());
    }
    #[test]
    fn checkpoints_are_immutable_and_unknown_is_rejected() {
        let state = crate::default_state("test");
        let item = json!({"type":"compaction","encrypted_content":"opaque"});
        let raw = json!({"input":[item]});
        assert!(state.history_owner(&raw).is_err());
        assert!(state
            .history_owner(
                &json!({"input":[{"type":"compaction_summary","encrypted_content":"unknown"}]})
            )
            .is_err());
        assert!(state
            .history_owner(
                &json!({"input":[{"type":"context_compaction","encrypted_content":"unknown"}]})
            )
            .is_err());
        assert!(state
            .history_owner(&json!({"input":[{"type":"context_compaction"}]}))
            .unwrap()
            .is_none());
        state.save_checkpoint(&item, "owner", 0).unwrap();
        assert_eq!(state.history_owner(&raw).unwrap().as_deref(), Some("owner"));
        assert!(state.save_checkpoint(&item, "other", 0).is_err());
        state.compact_result("compact", false, 0);
        assert!(!state.compact_ready("compact", 1));
        assert!(state.compact_ready("ordinary", 1));
        assert!(state.compact_ready("compact", 301));
    }
}

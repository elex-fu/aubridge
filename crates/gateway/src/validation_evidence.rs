//! Keep CLI validation receipts on the same settled decision/usage transaction.
use super::GatewayState;
use std::hash::{Hash, Hasher};

impl GatewayState {
    /// A copied command must also work when discovery used a non-PATH installation.
    pub(crate) fn validation_launch_command(&self, agent: &str) -> Option<String> {
        let name = agents::readiness::program_name(agent)?;
        let binary = agents::readiness::resolve_program(agent, name)
            .ok()?
            .canonicalize()
            .ok()?;
        let quoted = format!("'{}'", binary.to_str()?.replace('\'', "'\"'\"'"));
        Some(if agent == "codex" {
            format!("{quoted} --profile aubridge")
        } else {
            quoted
        })
    }

    /// Process-local validity fingerprint; never returned in receipts or logged.
    pub(crate) fn validation_routing_identity(&self) -> String {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let provider_ids = if let Ok(manifests) = self.manifests.read() {
            serde_json::to_vec(&*manifests).ok().hash(&mut hash);
            manifests.keys().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // Includes read-only external API credentials, not just stored-key booleans.
        for provider in provider_ids {
            provider.hash(&mut hash);
            self.resolve_api_key(&provider).0.hash(&mut hash);
        }
        if let Ok(keys) = self.keys.read() {
            keys.hash(&mut hash);
        }
        if let Ok(groups) = self.model_groups.read() {
            serde_json::to_vec(&*groups).ok().hash(&mut hash);
        }
        serde_json::to_vec(&self.cost_policy()).ok().hash(&mut hash);
        if let Ok(paused) = self.paused_providers.read() {
            paused.hash(&mut hash);
        }
        self.chatgpt.login_source().hash(&mut hash);
        self.chatgpt
            .quota_snapshot()
            .map(|snapshot| snapshot.quota_subject())
            .hash(&mut hash);
        self.claude_cli_enabled
            .load(std::sync::atomic::Ordering::SeqCst)
            .hash(&mut hash);
        self.claude_cli
            .binding()
            .map(|(subject, _)| subject)
            .hash(&mut hash);
        self.claude_cli.generation().hash(&mut hash);
        self.is_serving().hash(&mut hash);
        format!("{:016x}", hash.finish())
    }

    pub(crate) fn validation_session_evidence(
        &self,
        agent: &str,
        session_id: &str,
    ) -> Result<Vec<crate::agent_validation::ValidationRequest>, String> {
        self.flush_router_state();
        let ledger = self.ledger.as_ref().ok_or("ledger_unavailable")?;
        let rows = ledger
            .lock()
            .map_err(|_| "ledger_unavailable")?
            .validation_session_evidence(agent, session_id)
            .map_err(|_| "validation_evidence_mismatch")?;
        Ok(rows
            .into_iter()
            .map(|row| crate::agent_validation::ValidationRequest {
                request_id: row.request_id,
                provider: row.provider,
                served_model: row.served_model,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
            })
            .collect())
    }
}

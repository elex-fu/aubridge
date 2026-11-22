//! Exact local session/request joins for explicit CLI validation jobs.
use std::collections::BTreeSet;

use rusqlite::params;

use super::{DecisionFilter, Ledger};

/// Safe metadata only; no prompt, tool arguments, response text or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationRequest {
    pub request_id: String,
    pub provider: String,
    pub served_model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Ledger {
    /// An explicit native session is mandatory. Pending settlement returns no evidence.
    /// Malformed or conflicting ownership fails closed instead of selecting nearby traffic.
    pub fn validation_session_evidence(
        &self,
        agent: &str,
        session: &str,
    ) -> rusqlite::Result<Vec<ValidationRequest>> {
        if agent.is_empty() || session.is_empty() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let page = self.decision_page(
            DecisionFilter {
                agent: Some(agent),
                session_id: Some(session),
                ..Default::default()
            },
            64,
        )?;
        if page.next.is_some() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let mut ids = BTreeSet::new();
        let mut evidence = Vec::new();
        for decision in page.rows.into_iter().rev() {
            if decision.purpose.is_some() {
                continue;
            }
            if decision.outcome != "ok" {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let request_id = decision
                .request_id
                .filter(|id| !id.is_empty())
                .ok_or(rusqlite::Error::InvalidQuery)?;
            if !ids.insert(request_id.clone()) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let served_model = decision
                .served_model
                .filter(|model| !model.is_empty())
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let attempts: Vec<serde_json::Value> = serde_json::from_str(&decision.attempts)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let chosen: Vec<_> = attempts
                .iter()
                .filter(|attempt| attempt.get("chosen").and_then(|v| v.as_bool()) == Some(true))
                .collect();
            if chosen.len() != 1
                || chosen[0].get("id").and_then(|v| v.as_str()) != Some(decision.provider.as_str())
                || chosen[0].get("model").and_then(|v| v.as_str()) != Some(served_model.as_str())
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let mut query = self.conn.prepare(
                "SELECT u.input_tokens,u.output_tokens FROM usage_details x
                 JOIN usage u ON x.usage_rowid=u.rowid
                 WHERE x.request_id=?1 AND u.agent=?2 AND u.provider=?3 AND u.model=?4",
            )?;
            let rows = query.query_map(
                params![request_id, agent, decision.provider, served_model],
                |row| Ok((row.get::<_, Option<u64>>(0)?, row.get::<_, Option<u64>>(1)?)),
            )?;
            let usage = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if usage.is_empty() {
                let recorded: u64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM usage_details WHERE request_id=?1",
                    params![request_id],
                    |row| row.get(0),
                )?;
                return if recorded == 0 {
                    Ok(Vec::new())
                } else {
                    Err(rusqlite::Error::InvalidQuery)
                };
            }
            if usage.len() != 1
                || decision
                    .input_tokens
                    .zip(usage[0].0)
                    .is_some_and(|(expected, actual)| expected != actual)
                || decision
                    .output_tokens
                    .zip(usage[0].1)
                    .is_some_and(|(expected, actual)| expected != actual)
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            evidence.push(ValidationRequest {
                request_id,
                provider: decision.provider,
                served_model,
                input_tokens: usage[0].0,
                output_tokens: usage[0].1,
            });
        }
        if evidence.len() < 2 {
            return Ok(Vec::new());
        }
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompletedRequest, DecisionRecord, UsageRecord};

    fn request(id: &str, agent: &str, session: &str) -> CompletedRequest {
        CompletedRequest {
            decision: DecisionRecord {
                ts: 1,
                agent: agent.into(),
                session_id: Some(session.into()),
                request_id: Some(id.into()),
                provider: "source".into(),
                served_model: Some("actual-model".into()),
                outcome: "ok".into(),
                attempts: r#"[{"id":"source","model":"actual-model","chosen":true}]"#.into(),
                ..Default::default()
            },
            usage: vec![UsageRecord {
                ts: 1,
                agent: agent.into(),
                request_id: Some(id.into()),
                provider: "source".into(),
                model: "actual-model".into(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn validation_joins_exact_native_session_and_preserves_unknown_usage() {
        let ledger = Ledger::open(":memory:").unwrap();
        ledger
            .record_requests(&[
                request("one", "gemini", "session"),
                request("two", "gemini", "session"),
                request("other-agent", "codex", "session"),
                request("other-session", "gemini", "elsewhere"),
            ])
            .unwrap();
        let rows = ledger
            .validation_session_evidence("gemini", "session")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.input_tokens.is_none()));
        assert!(rows
            .iter()
            .all(|row| row.request_id == "one" || row.request_id == "two"));
        assert!(ledger
            .validation_session_evidence("gemini", "missing")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn validation_rejects_chosen_model_or_usage_ownership_conflicts() {
        for corrupt_chosen in [true, false] {
            let ledger = Ledger::open(":memory:").unwrap();
            let mut bad = request("two", "gemini", "session");
            if corrupt_chosen {
                bad.decision.attempts = r#"[{"id":"source","model":"wrong","chosen":true}]"#.into();
            } else {
                bad.usage[0].agent = "codex".into();
            }
            ledger
                .record_requests(&[request("one", "gemini", "session"), bad])
                .unwrap();
            let result = ledger.validation_session_evidence("gemini", "session");
            assert!(result.is_err());
        }
    }
}

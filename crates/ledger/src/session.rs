//! Session projections never join a request to its attempts before counting it.
use super::{Accounting, DecisionFilter, Ledger, UsageRecord};
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub type SessionPage = (Vec<SessionSummary>, Option<(u64, i64)>);

#[derive(Debug, Default, Serialize)]
pub struct SessionSource {
    pub provider: String,
    pub model: String,
    pub kind: String,
    pub requests: u64,
    pub usage_records: u64,
    pub attempt_records: u64,
    pub unreported_attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub partial_usage_records: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_reported_records: u64,
    pub cache_unknown_records: u64,
    pub cache_input_tokens: u64,
    pub cache_hit_read_tokens: u64,
    pub cache_hit_rate: Option<f64>,
    pub priced_records: u64,
    pub valued_records: u64,
    pub partial_estimate_records: u64,
    pub cache_rate_fallback_records: u64,
    pub price_coverage: Option<f64>,
    pub api_equivalent_usd: Option<f64>,
    pub cache_value_delta_usd: Option<f64>,
    pub cache_value_known_records: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct SessionSummary {
    pub agent: String,
    pub session_id: String,
    pub parent_session_ids: Vec<String>,
    pub first_ts: u64,
    pub last_ts: u64,
    pub requests: u64,
    pub successful_requests: u64,
    pub duration_ms: u64,
    pub duration_known_requests: u64,
    pub purposes: BTreeMap<String, u64>,
    pub free_requests: u64,
    pub subscription_requests: u64,
    pub unknown_source_requests: u64,
    pub sources: Vec<SessionSource>,
}

impl Ledger {
    /// Cursor and limit apply to distinct identities, never to individual usage rows.
    pub fn session_page(
        &self,
        agent: Option<&str>,
        session: Option<&str>,
        before: Option<(u64, i64)>,
        limit: u32,
    ) -> rusqlite::Result<SessionPage> {
        let mut stmt = self.conn.prepare("SELECT agent,session_id,MAX(ts),MAX(rowid) FROM decisions
            WHERE session_id IS NOT NULL AND (?1 IS NULL OR agent=?1) AND (?2 IS NULL OR session_id=?2)
            GROUP BY agent,session_id HAVING (?3 IS NULL OR (MAX(ts),MAX(rowid)) < (?3,?4))
            ORDER BY MAX(ts) DESC,MAX(rowid) DESC LIMIT ?5")?;
        let mut keys = stmt
            .query_map(
                params![
                    agent,
                    session,
                    before.map(|v| v.0),
                    before.map(|v| v.1),
                    i64::from(limit) + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if keys.len() > limit as usize {
            keys.truncate(limit as usize);
            keys.last().map(|v| (v.2, v.3))
        } else {
            None
        };
        let summaries = keys
            .into_iter()
            .map(|(agent, id, _, _)| self.session_summary(&agent, &id))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((summaries, next))
    }

    pub fn session_summary(&self, agent: &str, session: &str) -> rusqlite::Result<SessionSummary> {
        let mut summary = SessionSummary {
            agent: agent.into(),
            session_id: session.into(),
            ..Default::default()
        };
        let mut parents = BTreeSet::new();
        let mut kinds = BTreeMap::new();
        let mut free = BTreeSet::new();
        let mut subscription = BTreeSet::new();
        let mut known = BTreeSet::new();
        let mut sources: BTreeMap<(String, String, String), (SessionSource, BTreeSet<String>)> =
            BTreeMap::new();
        let mut before = None;
        loop {
            let page = self.decision_page(
                DecisionFilter {
                    agent: Some(agent),
                    session_id: Some(session),
                    before,
                    ..Default::default()
                },
                500,
            )?;
            before = page.next;
            for d in page.rows {
                summary.requests += 1;
                summary.successful_requests += u64::from(d.outcome == "ok");
                summary.first_ts = if summary.requests == 1 {
                    d.ts
                } else {
                    summary.first_ts.min(d.ts)
                };
                summary.last_ts = summary.last_ts.max(d.ts);
                if let Some(ms) = d.duration_ms {
                    summary.duration_ms = summary.duration_ms.saturating_add(ms);
                    summary.duration_known_requests += 1;
                }
                *summary
                    .purposes
                    .entry(d.purpose.clone().unwrap_or_else(|| "unknown".into()))
                    .or_default() += 1;
                if let Some(parent) = &d.parent_session_id {
                    parents.insert(parent.clone());
                }
                let Some(request) = &d.request_id else {
                    continue;
                };
                let attempts: Vec<serde_json::Value> =
                    serde_json::from_str(&d.attempts).unwrap_or_default();
                for a in attempts {
                    let Some(provider) = a.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let model = a.get("model").and_then(|v| v.as_str()).unwrap_or("");
                    match kind {
                        "free_tier" => {
                            free.insert(request.clone());
                            known.insert(request.clone());
                        }
                        "subscription" => {
                            subscription.insert(request.clone());
                            known.insert(request.clone());
                        }
                        _ => {}
                    }
                    let (source, requests) = sources
                        .entry((provider.to_owned(), model.to_owned(), kind.to_owned()))
                        .or_insert_with(|| {
                            (
                                SessionSource {
                                    provider: provider.into(),
                                    model: model.into(),
                                    kind: kind.into(),
                                    ..Default::default()
                                },
                                BTreeSet::new(),
                            )
                        });
                    source.attempt_records += 1;
                    requests.insert(request.clone());
                    kinds.insert(
                        (request.clone(), provider.to_owned(), model.to_owned()),
                        kind.to_owned(),
                    );
                }
            }
            if before.is_none() {
                break;
            }
        }
        // A separate usage query keeps request counts and elapsed time independent of retries.
        let mut stmt=self.conn.prepare("SELECT u.ts,u.agent,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting,
            x.request_id,x.cache_read_tokens,x.cache_write_tokens,x.input_includes_cache,x.price_json,x.benchmark_json
            FROM usage_details x JOIN usage u ON x.usage_rowid=u.rowid
            WHERE x.request_id IN(SELECT d.request_id FROM decisions d WHERE d.agent=?1 AND d.session_id=?2 AND d.request_id IS NOT NULL)")?;
        let rows = stmt.query_map(params![agent, session], |r| {
            Ok(UsageRecord {
                ts: r.get(0)?,
                agent: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                accounting: match r.get::<_, String>(6)?.as_str() {
                    "reported" => Accounting::Reported,
                    "estimated" => Accounting::Estimated,
                    _ => Accounting::Partial,
                },
                request_id: r.get(7)?,
                cache_read_tokens: r.get(8)?,
                cache_write_tokens: r.get(9)?,
                input_includes_cache: r.get(10)?,
                price_snapshot: super::value::parse_price(r.get(11)?, 11)?,
                benchmark_snapshot: super::value::parse_price(r.get(12)?, 12)?,
            })
        })?;
        for row in rows {
            let r = row?;
            let request = r.request_id.clone().unwrap_or_default();
            let kind = kinds
                .get(&(request.clone(), r.provider.clone(), r.model.clone()))
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            match kind.as_str() {
                "free_tier" => {
                    free.insert(request.clone());
                    known.insert(request.clone());
                }
                "subscription" => {
                    subscription.insert(request.clone());
                    known.insert(request.clone());
                }
                _ => {}
            }
            let (source, requests) = sources
                .entry((r.provider.clone(), r.model.clone(), kind.clone()))
                .or_insert_with(|| {
                    (
                        SessionSource {
                            provider: r.provider.clone(),
                            model: r.model.clone(),
                            kind,
                            ..Default::default()
                        },
                        BTreeSet::new(),
                    )
                });
            requests.insert(request);
            source.usage_records += 1;
            source.input_tokens = source
                .input_tokens
                .saturating_add(r.input_tokens.unwrap_or_default());
            source.output_tokens = source
                .output_tokens
                .saturating_add(r.output_tokens.unwrap_or_default());
            source.partial_usage_records += u64::from(
                r.accounting != Accounting::Reported
                    || r.input_tokens.is_none()
                    || r.output_tokens.is_none(),
            );
            source.cache_read_tokens = source
                .cache_read_tokens
                .saturating_add(r.cache_read_tokens.unwrap_or_default());
            source.cache_write_tokens = source
                .cache_write_tokens
                .saturating_add(r.cache_write_tokens.unwrap_or_default());
            if let Some(input) = r.cache_input() {
                source.cache_reported_records += 1;
                source.cache_input_tokens = source.cache_input_tokens.saturating_add(input);
                source.cache_hit_read_tokens = source
                    .cache_hit_read_tokens
                    .saturating_add(r.cache_read_tokens.unwrap_or_default());
            } else {
                source.cache_unknown_records += 1;
            }
            source.priced_records += u64::from(r.price().is_some());
            if let Some(estimate) = r.estimate() {
                source.valued_records += 1;
                source.partial_estimate_records += u64::from(estimate.partial_usage);
                source.cache_rate_fallback_records += u64::from(estimate.cache_rate_fallback);
                source.api_equivalent_usd = Some(
                    source.api_equivalent_usd.unwrap_or_default() + estimate.api_equivalent_usd,
                );
                if let Some(delta) = estimate.cache_value_delta_usd {
                    source.cache_value_known_records += 1;
                    source.cache_value_delta_usd =
                        Some(source.cache_value_delta_usd.unwrap_or_default() + delta);
                }
            }
        }
        summary.parent_session_ids = parents.into_iter().collect();
        summary.free_requests = free.len() as u64;
        summary.subscription_requests = subscription.len() as u64;
        summary.unknown_source_requests = summary.requests.saturating_sub(known.len() as u64);
        summary.sources = sources
            .into_values()
            .map(|(mut source, requests)| {
                source.requests = requests.len() as u64;
                source.attempt_records = source.attempt_records.max(source.usage_records);
                source.unreported_attempts =
                    source.attempt_records.saturating_sub(source.usage_records);
                source.cache_unknown_records = source
                    .cache_unknown_records
                    .saturating_add(source.unreported_attempts);
                if source.attempt_records > 0 {
                    source.price_coverage =
                        Some(source.priced_records as f64 / source.attempt_records as f64);
                }
                if source.cache_input_tokens > 0 {
                    source.cache_hit_rate = Some(
                        source.cache_hit_read_tokens as f64 / source.cache_input_tokens as f64,
                    );
                }
                source
            })
            .collect();
        Ok(summary)
    }
}

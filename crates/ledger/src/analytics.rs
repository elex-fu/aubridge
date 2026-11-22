//! Bounded, local-calendar summaries of retained ledger facts.
use super::{Accounting, Ledger, UsageRecord};
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeMap;

pub struct AnalyticsContext<'a> {
    pub now: u64,
    pub retention_since: u64,
    pub active_inference: bool,
    pub pending_timestamps: &'a [u64],
}

#[derive(Debug)]
pub enum AnalyticsError {
    Invalid,
    TooLarge,
    Storage,
}
impl From<rusqlite::Error> for AnalyticsError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}
impl std::fmt::Display for AnalyticsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid analytics window",
            Self::TooLarge => "analytics window too large",
            Self::Storage => "analytics unavailable",
        })
    }
}
impl std::error::Error for AnalyticsError {}

#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsWindow {
    pub start: String,
    pub end: String,
    pub timezone: &'static str,
    pub start_ts: u64,
    pub end_ts: u64,
    pub observed_at: u64,
    pub in_progress: bool,
    pub scope: &'static str,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Duration {
    pub known: u64,
    pub missing: u64,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Requests {
    pub total: u64,
    pub ok: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub success_rate: Option<f64>,
    pub attempt_known_requests: u64,
    pub retried_requests: u64,
    pub duration: Duration,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Tokens {
    pub tokens: Option<u64>,
    pub known: u64,
    pub missing: u64,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Cache {
    pub read_tokens: Option<u64>,
    pub write_tokens: Option<u64>,
    pub hit_read_tokens: u64,
    pub hit_input_tokens: u64,
    pub hit_rate: Option<f64>,
    pub unknown_records: u64,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Price {
    pub priced: u64,
    pub valued: u64,
    pub usd: Option<f64>,
    pub partial_valued: u64,
    pub fallback_valued: u64,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Prices {
    pub directory: Price,
    pub declared: Price,
    pub legacy: Price,
    pub benchmark: Price,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Usage {
    pub records: u64,
    pub reported: u64,
    pub partial: u64,
    pub estimated: u64,
    pub input: Tokens,
    pub output: Tokens,
    pub cache: Cache,
    pub prices: Prices,
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Metrics {
    pub requests: Requests,
    pub usage: Usage,
}
#[derive(Debug, Clone, Serialize)]
pub struct Day {
    pub date: String,
    pub metrics: Metrics,
}
#[derive(Debug, Clone, Serialize)]
pub struct GroupRow {
    pub key: Option<String>,
    pub other: bool,
    pub requests: Requests,
    pub usage: Usage,
}
#[derive(Debug, Default, Serialize)]
pub struct Groups {
    pub agents: Vec<GroupRow>,
    pub providers: Vec<GroupRow>,
    pub models: Vec<GroupRow>,
}
#[derive(Debug, Serialize)]
pub struct Change {
    pub rate: Option<f64>,
    pub reason: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Comparison {
    pub window: AnalyticsWindow,
    pub summary: Metrics,
    pub requests_change: Change,
    pub directory_usd_change: Change,
}
#[derive(Debug, Serialize)]
pub struct AnalyticsReport {
    pub window: AnalyticsWindow,
    pub summary: Metrics,
    pub days: Vec<Day>,
    pub groups: Groups,
    pub comparison: Option<Comparison>,
}

#[derive(Default)]
struct Accumulator {
    metrics: Metrics,
    durations: Vec<u64>,
}
struct Decision {
    agent: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    outcome: String,
    attempts: Option<usize>,
    duration: Option<u64>,
}
impl Accumulator {
    fn decision(&mut self, row: &Decision) {
        let r = &mut self.metrics.requests;
        r.total += 1;
        match row.outcome.as_str() {
            "ok" => {
                r.ok += 1;
                if let Some(ms) = row.duration {
                    self.durations.push(ms);
                }
            }
            "cancelled" => r.cancelled += 1,
            _ => r.failed += 1,
        }
        if let Some(attempts) = row.attempts {
            r.attempt_known_requests += 1;
            r.retried_requests += u64::from(attempts > 1);
        }
    }
    fn usage(&mut self, row: &UsageRecord) {
        let u = &mut self.metrics.usage;
        u.records += 1;
        match row.accounting {
            Accounting::Reported => u.reported += 1,
            Accounting::Partial => u.partial += 1,
            Accounting::Estimated => u.estimated += 1,
        }
        let normalized = row.cache_input();
        add_tokens(&mut u.input, normalized);
        add_tokens(&mut u.output, row.output_tokens);
        add_optional(&mut u.cache.read_tokens, row.cache_read_tokens);
        add_optional(&mut u.cache.write_tokens, row.cache_write_tokens);
        if let Some(input) = normalized {
            u.cache.hit_input_tokens = u.cache.hit_input_tokens.saturating_add(input);
            u.cache.hit_read_tokens = u
                .cache
                .hit_read_tokens
                .saturating_add(row.cache_read_tokens.unwrap_or_default());
        } else {
            u.cache.unknown_records += 1;
        }
        if let Some(snapshot) = &row.price_snapshot {
            let price = match snapshot.basis.as_str() {
                "provider-directory" => Some(&mut u.prices.directory),
                "local-declaration" => Some(&mut u.prices.declared),
                "legacy-fixed-estimate" => Some(&mut u.prices.legacy),
                _ => None,
            };
            if let Some(price) = price {
                add_price(price, row.estimate());
            }
        }
        if row.benchmark_snapshot.is_some() {
            add_price(&mut u.prices.benchmark, row.benchmark_estimate());
        }
    }
    fn merge(&mut self, other: Self) {
        let a = &mut self.metrics.requests;
        let b = other.metrics.requests;
        a.total += b.total;
        a.ok += b.ok;
        a.failed += b.failed;
        a.cancelled += b.cancelled;
        a.attempt_known_requests += b.attempt_known_requests;
        a.retried_requests += b.retried_requests;
        self.durations.extend(other.durations);
        let a = &mut self.metrics.usage;
        let b = other.metrics.usage;
        a.records += b.records;
        a.reported += b.reported;
        a.partial += b.partial;
        a.estimated += b.estimated;
        merge_tokens(&mut a.input, b.input);
        merge_tokens(&mut a.output, b.output);
        add_optional(&mut a.cache.read_tokens, b.cache.read_tokens);
        add_optional(&mut a.cache.write_tokens, b.cache.write_tokens);
        a.cache.hit_read_tokens = a
            .cache
            .hit_read_tokens
            .saturating_add(b.cache.hit_read_tokens);
        a.cache.hit_input_tokens = a
            .cache
            .hit_input_tokens
            .saturating_add(b.cache.hit_input_tokens);
        a.cache.unknown_records += b.cache.unknown_records;
        merge_price(&mut a.prices.directory, b.prices.directory);
        merge_price(&mut a.prices.declared, b.prices.declared);
        merge_price(&mut a.prices.legacy, b.prices.legacy);
        merge_price(&mut a.prices.benchmark, b.prices.benchmark);
    }
    fn finish(mut self) -> Metrics {
        let r = &mut self.metrics.requests;
        r.success_rate =
            (r.total > r.cancelled).then(|| r.ok as f64 / (r.total - r.cancelled) as f64);
        self.durations.sort_unstable();
        r.duration.known = self.durations.len() as u64;
        r.duration.missing = r.ok - r.duration.known;
        r.duration.p50_ms = percentile(&self.durations, 50);
        r.duration.p95_ms = percentile(&self.durations, 95);
        let c = &mut self.metrics.usage.cache;
        c.hit_rate =
            (c.hit_input_tokens > 0).then(|| c.hit_read_tokens as f64 / c.hit_input_tokens as f64);
        self.metrics
    }
}
fn percentile(values: &[u64], percent: usize) -> Option<u64> {
    (!values.is_empty()).then(|| values[(values.len() * percent).div_ceil(100).saturating_sub(1)])
}
fn add_optional(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(value));
    }
}
fn add_tokens(total: &mut Tokens, value: Option<u64>) {
    if value.is_some() {
        total.known += 1;
        add_optional(&mut total.tokens, value);
    } else {
        total.missing += 1;
    }
}
fn merge_tokens(total: &mut Tokens, value: Tokens) {
    total.known += value.known;
    total.missing += value.missing;
    add_optional(&mut total.tokens, value.tokens);
}
fn add_price(total: &mut Price, estimate: Option<super::Estimate>) {
    total.priced += 1;
    if let Some(estimate) = estimate {
        total.valued += 1;
        total.usd = Some(total.usd.unwrap_or_default() + estimate.api_equivalent_usd);
        total.partial_valued += u64::from(estimate.partial_usage);
        total.fallback_valued += u64::from(estimate.cache_rate_fallback);
    }
}
fn merge_price(total: &mut Price, value: Price) {
    total.priced += value.priced;
    total.valued += value.valued;
    total.partial_valued += value.partial_valued;
    total.fallback_valued += value.fallback_valued;
    if let Some(usd) = value.usd {
        total.usd = Some(total.usd.unwrap_or_default() + usd);
    }
}
fn key(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn date_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return false;
    }
    let number = |start: usize, end: usize| value[start..end].parse::<u32>().unwrap_or_default();
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}

const WINDOW_COUNTS_SQL: &str = "SELECT
    (SELECT COUNT(*) FROM (SELECT 1 FROM decisions WHERE ts>=?1 AND ts<?2 LIMIT 100001)),
    (SELECT COUNT(*) FROM (SELECT 1 FROM usage WHERE ts>=?1 AND ts<?2 LIMIT 100001))";

impl Ledger {
    pub fn analytics_window(
        &self,
        start: &str,
        end: &str,
        now: u64,
    ) -> Result<AnalyticsWindow, AnalyticsError> {
        if !date_valid(start) || !date_valid(end) || now > i64::MAX as u64 {
            return Err(AnalyticsError::Invalid);
        }
        let (days,start_ts,end_ts,tomorrow):(i64,Option<i64>,Option<i64>,String)=self.conn.query_row(
            "SELECT CAST(julianday(?2)-julianday(?1) AS INTEGER),unixepoch(?1||' 00:00:00','utc'),unixepoch(?2||' 00:00:00','utc'),date(?3,'unixepoch','localtime','+1 day')",
            params![start,end,now],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        let (Some(start_ts), Some(end_ts)) = (start_ts, end_ts) else {
            return Err(AnalyticsError::Invalid);
        };
        if !(1..=31).contains(&days)
            || start_ts < 0
            || end_ts <= start_ts
            || end > tomorrow.as_str()
        {
            return Err(AnalyticsError::Invalid);
        }
        Ok(AnalyticsWindow {
            start: start.into(),
            end: end.into(),
            timezone: "local",
            start_ts: start_ts as u64,
            end_ts: end_ts as u64,
            observed_at: now,
            in_progress: now < end_ts as u64,
            scope: "retained_ledger",
        })
    }
    pub fn analytics(
        &self,
        start: &str,
        end: &str,
        compare_previous: bool,
        context: AnalyticsContext<'_>,
    ) -> Result<AnalyticsReport, AnalyticsError> {
        let transaction = self.conn.unchecked_transaction()?;
        let window = self.analytics_window(start, end, context.now)?;
        let (summary, days, groups) = self.aggregate_window(&window)?;
        let comparison = if compare_previous {
            let previous_start: String = self.conn.query_row(
                "SELECT date(?1,printf('-%d days',CAST(julianday(?2)-julianday(?1) AS INTEGER)))",
                params![start, end],
                |row| row.get(0),
            )?;
            let previous = self.analytics_window(&previous_start, start, context.now)?;
            let (previous_summary, _, _) = self.aggregate_window(&previous)?;
            let gate = if window.in_progress || previous.in_progress {
                Some("open_period")
            } else if context.retention_since > previous.start_ts {
                Some("retention_unknown")
            } else if context.active_inference
                || context
                    .pending_timestamps
                    .iter()
                    .any(|at| *at >= previous.start_ts && *at < window.end_ts)
            {
                Some("pending_settlement")
            } else {
                None
            };
            let requests_change = change(
                summary.requests.total as f64,
                previous_summary.requests.total as f64,
                gate,
            );
            let directory_usd_change = if let Some(reason) = gate {
                Change {
                    rate: None,
                    reason: Some(reason),
                }
            } else if !complete_price(&summary.usage) || !complete_price(&previous_summary.usage) {
                Change {
                    rate: None,
                    reason: Some("incomplete_pricing"),
                }
            } else {
                change(
                    summary.usage.prices.directory.usd.unwrap_or_default(),
                    previous_summary
                        .usage
                        .prices
                        .directory
                        .usd
                        .unwrap_or_default(),
                    None,
                )
            };
            Some(Comparison {
                window: previous,
                summary: previous_summary,
                requests_change,
                directory_usd_change,
            })
        } else {
            None
        };
        transaction.commit()?;
        Ok(AnalyticsReport {
            window,
            summary,
            days,
            groups,
            comparison,
        })
    }
    fn aggregate_window(
        &self,
        window: &AnalyticsWindow,
    ) -> Result<(Metrics, Vec<Day>, Groups), AnalyticsError> {
        let counts: (u64, u64) = self.conn.query_row(
            WINDOW_COUNTS_SQL,
            params![window.start_ts, window.end_ts],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if counts.0 > 100_000 || counts.1 > 100_000 {
            return Err(AnalyticsError::TooLarge);
        }
        let dates = self.local_dates(&window.start, &window.end)?;
        let mut daily: BTreeMap<String, Accumulator> = dates
            .into_iter()
            .map(|date| (date, Accumulator::default()))
            .collect();
        let mut total = Accumulator::default();
        let mut agents: BTreeMap<Option<String>, Accumulator> = BTreeMap::new();
        let mut providers: BTreeMap<Option<String>, Accumulator> = BTreeMap::new();
        let mut models: BTreeMap<Option<String>, Accumulator> = BTreeMap::new();
        let mut stmt=self.conn.prepare("SELECT date(ts,'unixepoch','localtime'),agent,provider,served_model,outcome,attempts,duration_ms FROM decisions WHERE ts>=?1 AND ts<?2")?;
        let rows = stmt.query_map(params![window.start_ts, window.end_ts], |row| {
            let raw: Option<String> = row.get(5)?;
            let attempts = raw
                .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
                .filter(|items| {
                    items.iter().all(|item| {
                        item.get("id")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|id| !id.trim().is_empty())
                    })
                })
                .map(|items| items.len());
            Ok((
                row.get::<_, String>(0)?,
                Decision {
                    agent: row.get::<_, Option<String>>(1)?.and_then(key),
                    provider: row.get::<_, Option<String>>(2)?.and_then(key),
                    model: row.get::<_, Option<String>>(3)?.and_then(key),
                    outcome: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    attempts,
                    duration: row.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (date, row) = row?;
            total.decision(&row);
            if let Some(day) = daily.get_mut(&date) {
                day.decision(&row);
            }
            agents.entry(row.agent.clone()).or_default().decision(&row);
            providers
                .entry(row.provider.clone())
                .or_default()
                .decision(&row);
            models.entry(row.model.clone()).or_default().decision(&row);
        }
        let usage_sql = super::value::USAGE_SELECT.replacen(
            "FROM usage",
            ",date(u.ts,'unixepoch','localtime') FROM usage",
            1,
        );
        let mut stmt = self.conn.prepare(&usage_sql)?;
        let rows = stmt.query_map(params![window.start_ts, window.end_ts], |row| {
            Ok((
                row.get::<_, String>(13)?,
                super::value::read_usage_record(row)?,
            ))
        })?;
        for row in rows {
            let (date, row) = row?;
            if row.price_snapshot.as_ref().is_some_and(|price| {
                !price.applies(&row.provider, &row.model) || price.basis == "user-benchmark"
            }) || row.benchmark_snapshot.as_ref().is_some_and(|price| {
                !price.applies(&row.provider, &row.model) || price.basis != "user-benchmark"
            }) {
                return Err(AnalyticsError::Storage);
            }
            total.usage(&row);
            if let Some(day) = daily.get_mut(&date) {
                day.usage(&row);
            }
            agents
                .entry(key(row.agent.clone()))
                .or_default()
                .usage(&row);
            providers
                .entry(key(row.provider.clone()))
                .or_default()
                .usage(&row);
            models
                .entry(key(row.model.clone()))
                .or_default()
                .usage(&row);
        }
        let summary = total.finish();
        validate_amounts(&summary.usage.prices)?;
        Ok((
            summary,
            daily
                .into_iter()
                .map(|(date, value)| Day {
                    date,
                    metrics: value.finish(),
                })
                .collect(),
            Groups {
                agents: groups(agents),
                providers: groups(providers),
                models: groups(models),
            },
        ))
    }
    fn local_dates(&self, start: &str, end: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt=self.conn.prepare("WITH RECURSIVE days(day) AS(SELECT ?1 UNION ALL SELECT date(day,'+1 day') FROM days WHERE date(day,'+1 day')<?2) SELECT day FROM days")?;
        let rows = stmt.query_map(params![start, end], |row| row.get(0))?;
        rows.collect()
    }
}
fn groups(mut values: BTreeMap<Option<String>, Accumulator>) -> Vec<GroupRow> {
    // Unknown provenance always retains a visible row instead of disappearing
    // into the named groups' overflow bucket.
    let unknown = values.remove(&None);
    let named_limit = if unknown.is_some() { 19 } else { 20 };
    let mut named: Vec<_> = values.into_iter().collect();
    let order = |(ka, a): &(Option<String>, Accumulator),
                 (kb, b): &(Option<String>, Accumulator)| {
        b.metrics
            .requests
            .total
            .cmp(&a.metrics.requests.total)
            .then(b.metrics.usage.records.cmp(&a.metrics.usage.records))
            .then(ka.cmp(kb))
    };
    named.sort_by(order);
    let overflow = named.split_off(named.len().min(named_limit));
    if let Some(unknown) = unknown {
        named.push((None, unknown));
    }
    named.sort_by(order);
    let mut output: Vec<_> = named
        .into_iter()
        .map(|(key, value)| {
            let metrics = value.finish();
            GroupRow {
                key,
                other: false,
                requests: metrics.requests,
                usage: metrics.usage,
            }
        })
        .collect();
    if !overflow.is_empty() {
        let mut other = Accumulator::default();
        for (_, value) in overflow {
            other.merge(value);
        }
        let metrics = other.finish();
        output.push(GroupRow {
            key: None,
            other: true,
            requests: metrics.requests,
            usage: metrics.usage,
        });
    }
    output
}

fn complete_price(value: &Usage) -> bool {
    value.records > 0
        && value.prices.directory.valued == value.records
        && value.prices.directory.partial_valued == 0
        && value.prices.directory.fallback_valued == 0
}
fn change(current: f64, previous: f64, gate: Option<&'static str>) -> Change {
    if let Some(reason) = gate {
        Change {
            rate: None,
            reason: Some(reason),
        }
    } else if previous == 0.0 {
        Change {
            rate: None,
            reason: Some("no_baseline"),
        }
    } else {
        let rate = (current - previous) / previous;
        if rate.is_finite() {
            Change {
                rate: Some(rate),
                reason: None,
            }
        } else {
            Change {
                rate: None,
                reason: Some("incomplete_pricing"),
            }
        }
    }
}
fn validate_amounts(value: &Prices) -> Result<(), AnalyticsError> {
    if [
        &value.directory,
        &value.declared,
        &value.legacy,
        &value.benchmark,
    ]
    .iter()
    .any(|price| price.usd.is_some_and(|value| !value.is_finite()))
    {
        Err(AnalyticsError::Storage)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: u64 = 2_000_000_000;
    fn context() -> AnalyticsContext<'static> {
        AnalyticsContext {
            now: NOW,
            retention_since: 0,
            pending_timestamps: &[],
            active_inference: false,
        }
    }

    #[test]
    fn cap_rejects_either_dataset_without_truncating() {
        for table in ["decisions", "usage"] {
            let ledger = Ledger::open(":memory:").unwrap();
            let at = ledger
                .analytics_window("2024-01-01", "2024-01-02", NOW)
                .unwrap()
                .start_ts;
            ledger.conn.execute_batch(&format!("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100003) INSERT INTO {table}(ts) SELECT {at} FROM n")).unwrap();
            let bounded: (u64, u64) = ledger
                .conn
                .query_row(WINDOW_COUNTS_SQL, params![at, at + 1], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!(
                if table == "decisions" {
                    bounded.0
                } else {
                    bounded.1
                },
                100001
            );
            assert!(matches!(
                ledger.analytics("2024-01-01", "2024-01-02", false, context()),
                Err(AnalyticsError::TooLarge)
            ));
            assert!(ledger.conn.is_autocommit());
        }
    }
    #[test]
    fn exact_cap_and_zero_unknown_group_are_retained() {
        let ledger = Ledger::open(":memory:").unwrap();
        let at = ledger
            .analytics_window("2024-01-01", "2024-01-02", NOW)
            .unwrap()
            .start_ts;
        ledger.conn.execute_batch(&format!("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO decisions(ts) SELECT {at} FROM n")).unwrap();
        let report = ledger
            .analytics("2024-01-01", "2024-01-02", false, context())
            .unwrap();
        assert_eq!(report.summary.requests.total, 100000);
        assert_eq!(report.summary.requests.attempt_known_requests, 0);
        assert_eq!(report.groups.agents[0].key, None);
        assert!(ledger.conn.is_autocommit());
    }
    #[test]
    fn legacy_rows_use_existing_benchmark_without_creating_price_evidence() {
        let ledger = Ledger::open(":memory:").unwrap();
        let at = ledger
            .analytics_window("2024-01-01", "2024-01-02", NOW)
            .unwrap()
            .start_ts;
        ledger.conn.execute("INSERT INTO usage(ts,agent,provider,model,input_tokens,output_tokens,accounting) VALUES(?1,'tool','legacy','glm-4.5-flash',100,10,'reported')",[at]).unwrap();
        let old = ledger.value_summary(at, at + 1).unwrap();
        let report = ledger
            .analytics("2024-01-01", "2024-01-02", false, context())
            .unwrap();
        assert_eq!(
            report.summary.usage.prices.legacy.usd,
            old.api_equivalent_usd
        );
        assert_eq!(report.summary.usage.prices.directory.priced, 0);
        assert_eq!(report.summary.usage.input.tokens, None);
        let rows: u64 = ledger
            .conn
            .query_row("SELECT COUNT(*) FROM usage_details", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
    #[test]
    fn storage_error_is_not_an_empty_result() {
        let ledger = Ledger::open(":memory:").unwrap();
        ledger.conn.execute("DROP TABLE usage_details", []).unwrap();
        assert!(matches!(
            ledger.analytics("2024-01-01", "2024-01-02", false, context()),
            Err(AnalyticsError::Storage)
        ));
        assert!(ledger.conn.is_autocommit());
    }
}

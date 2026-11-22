//! Immutable price evidence and cache-aware API-equivalent estimates.
use std::collections::BTreeMap;

use manifest::model::{ModelFacts, ModelPrice};
use rusqlite::{params, types::Type};
use serde::{Deserialize, Serialize};

use super::{Accounting, Ledger, UsageRecord};

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS usage_details (
    usage_rowid INTEGER PRIMARY KEY, request_id TEXT, cache_read_tokens INTEGER,
    cache_write_tokens INTEGER, input_includes_cache INTEGER, price_json TEXT, benchmark_json TEXT
); CREATE INDEX IF NOT EXISTS idx_usage_details_request ON usage_details(request_id);";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceSnapshot {
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default = "legacy_basis")]
    pub basis: String,
    #[serde(default)]
    pub target_provider: Option<String>,
    #[serde(default)]
    pub target_model: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub source: String,
    pub observed_at: Option<u64>,
    pub price: ModelPrice,
    #[serde(default = "usd")]
    pub currency: String,
    #[serde(default = "per_million")]
    pub unit: String,
}

fn usd() -> String {
    "USD".into()
}
fn per_million() -> String {
    "per-million-tokens".into()
}
impl Default for PriceSnapshot {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: String::new(),
            basis: legacy_basis(),
            target_provider: None,
            target_model: None,
            version: None,
            source: String::new(),
            observed_at: None,
            price: ModelPrice::default(),
            currency: usd(),
            unit: per_million(),
        }
    }
}
fn legacy_basis() -> String {
    "legacy-fixed-estimate".into()
}

impl PriceSnapshot {
    pub fn from_facts(model: &str, facts: &ModelFacts) -> Option<Self> {
        if model.is_empty() {
            return None;
        }
        let price = facts.price.as_ref().filter(|price| price.valid())?.clone();
        Some(Self {
            model: model.into(),
            // Facts are a declarative contract, not proof of where they were fetched.
            // The IO boundary upgrades trusted directory snapshots with provider scope.
            source: "declared".into(),
            basis: "local-declaration".into(),
            observed_at: facts.observed_at,
            price,
            ..Self::default()
        })
    }

    pub fn builtin(model: &str) -> Option<Self> {
        let (input, output) = super::benchmark_price(model)?;
        Some(Self {
            model: model.into(),
            source: "builtin-benchmark-2026-11".into(),
            observed_at: None,
            basis: legacy_basis(),
            version: Some("builtin-benchmark-2026-11".into()),
            price: ModelPrice {
                input_per_million: input,
                output_per_million: output,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
            ..Self::default()
        })
    }

    pub fn benchmark(mapping: &manifest::model::BenchmarkMapping) -> Option<Self> {
        mapping.valid().then(|| Self {
            model: mapping.model.clone(),
            provider: mapping.provider.clone(),
            basis: "user-benchmark".into(),
            target_provider: Some(mapping.target_provider.clone()),
            target_model: Some(mapping.target_model.clone()),
            source: mapping.source.clone(),
            observed_at: Some(mapping.observed_at),
            version: Some(mapping.version.clone()),
            price: mapping.price.clone(),
            currency: usd(),
            unit: per_million(),
        })
    }

    pub(super) fn applies(&self, provider: &str, model: &str) -> bool {
        self.valid()
            && self.model == model
            && (self.provider.is_empty() || self.provider == provider)
    }

    pub(super) fn valid(&self) -> bool {
        if self.model.is_empty()
            || self.currency != "USD"
            || self.unit != "per-million-tokens"
            || !self.price.valid()
        {
            return false;
        }
        match self.basis.as_str() {
            "provider-directory" => {
                !self.provider.is_empty()
                    && !self.source.is_empty()
                    && self.observed_at.is_some()
                    && self.version.as_ref().is_some_and(|s| !s.is_empty())
            }
            "user-benchmark" => {
                !self.provider.is_empty()
                    && self.target_provider.as_ref().is_some_and(|s| !s.is_empty())
                    && self.target_model.as_ref().is_some_and(|s| !s.is_empty())
                    && !self.source.is_empty()
                    && self.version.as_ref().is_some_and(|s| !s.is_empty())
                    && self.observed_at.is_some()
            }
            "local-declaration" | "legacy-fixed-estimate" => true,
            _ => false,
        }
    }
}

pub(super) fn parse_price(
    json: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<PriceSnapshot>> {
    let Some(json) = json else {
        return Ok(None);
    };
    let snapshot: PriceSnapshot = serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })?;
    if !snapshot.valid() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(Some(snapshot))
}

#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub api_equivalent_usd: f64,
    pub cache_rate_fallback: bool,
    pub partial_usage: bool,
    pub cache_value_delta_usd: Option<f64>,
}

impl UsageRecord {
    pub fn price(&self) -> Option<PriceSnapshot> {
        self.price_snapshot.clone()
    }

    pub fn estimate(&self) -> Option<Estimate> {
        self.estimate_with(self.price().as_ref()?)
    }

    pub fn benchmark_estimate(&self) -> Option<Estimate> {
        self.estimate_with(self.benchmark_snapshot.as_ref()?)
    }

    fn estimate_with(&self, snapshot: &PriceSnapshot) -> Option<Estimate> {
        if !snapshot.applies(&self.provider, &self.model)
            || [
                self.input_tokens,
                self.output_tokens,
                self.cache_read_tokens,
                self.cache_write_tokens,
            ]
            .iter()
            .all(Option::is_none)
        {
            return None;
        }
        let read = self.cache_read_tokens.unwrap_or_default();
        let write = self.cache_write_tokens.unwrap_or_default();
        let cached = read.checked_add(write)?;
        if cached > 0 && self.input_includes_cache.is_none() {
            return None;
        }
        let includes = self.input_includes_cache.unwrap_or(true);
        let ordinary = match self.input_tokens {
            Some(input) if includes => input.checked_sub(cached)?,
            Some(input) => input,
            None => 0,
        };
        let price = &snapshot.price;
        let output = self.output_tokens.unwrap_or_default();
        let amount = (ordinary as f64 * price.input_per_million
            + read as f64
                * price
                    .cache_read_per_million
                    .unwrap_or(price.input_per_million)
            + write as f64
                * price
                    .cache_write_per_million
                    .unwrap_or(price.input_per_million)
            + output as f64 * price.output_per_million)
            / 1e6;
        if !amount.is_finite() {
            return None;
        }
        let complete_cache = self.cache_read_tokens.is_some()
            && self.cache_write_tokens.is_some()
            && (read == 0 || price.cache_read_per_million.is_some())
            && (write == 0 || price.cache_write_per_million.is_some());
        let delta = complete_cache
            .then(|| {
                let total = self.input_tokens?;
                let total = if includes {
                    total
                } else {
                    total.checked_add(cached)?
                };
                let baseline = (total as f64 * price.input_per_million
                    + output as f64 * price.output_per_million)
                    / 1e6;
                (baseline - amount).is_finite().then_some(baseline - amount)
            })
            .flatten();
        Some(Estimate {
            api_equivalent_usd: amount,
            cache_rate_fallback: !complete_cache,
            partial_usage: self.accounting != Accounting::Reported
                || self.input_tokens.is_none()
                || self.output_tokens.is_none()
                || self.input_includes_cache.is_none()
                || (self.input_includes_cache == Some(false)
                    && (self.cache_read_tokens.is_none() || self.cache_write_tokens.is_none())),
            cache_value_delta_usd: delta,
        })
    }

    pub(crate) fn cache_input(&self) -> Option<u64> {
        let input = self.input_tokens?;
        let read = self.cache_read_tokens?;
        let total = match self.input_includes_cache? {
            true => {
                if read.checked_add(self.cache_write_tokens.unwrap_or_default())? > input {
                    return None;
                }
                input
            }
            false => input
                .checked_add(read)?
                .checked_add(self.cache_write_tokens?)?,
        };
        (read <= total).then_some(total)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ProviderValue {
    pub provider: String,
    pub records: u64,
    pub priced_records: u64,
    pub api_equivalent_usd: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct ValueSummary {
    pub records: u64,
    pub priced_records: u64,
    pub unpriced_records: u64,
    pub valued_records: u64,
    pub builtin_priced_records: u64,
    pub partial_usage_records: u64,
    pub api_equivalent_usd: Option<f64>,
    pub price_coverage: Option<f64>,
    pub official_priced_records: u64,
    pub official_usd: Option<f64>,
    pub official_valued_records: u64,
    pub official_price_coverage: Option<f64>,
    pub benchmark_priced_records: u64,
    pub benchmark_valued_records: u64,
    pub benchmark_coverage: Option<f64>,
    pub benchmark_usd: Option<f64>,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_reported_records: u64,
    pub cache_unknown_records: u64,
    pub cache_input_tokens: u64,
    pub cache_hit_rate: Option<f64>,
    pub cache_rate_fallback_records: u64,
    pub cache_value_delta_usd: Option<f64>,
    pub cache_value_known_records: u64,
    pub providers: Vec<ProviderValue>,
}

impl Ledger {
    pub fn monthly_value(&self) -> rusqlite::Result<ValueSummary> {
        let (start, end) = self.conn.query_row(
            "SELECT unixepoch('now','localtime','start of month','utc'),
            unixepoch('now','localtime','start of month','+1 month','utc')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        self.value_summary(start, end)
    }

    pub fn value_summary(&self, start: u64, end: u64) -> rusqlite::Result<ValueSummary> {
        let mut statement = self.conn.prepare(USAGE_SELECT)?;
        let rows = statement.query_map(params![start, end], read_usage_record)?;
        summarize_values(rows)
    }
}

pub(super) const USAGE_SELECT: &str = "SELECT u.ts,u.agent,u.provider,u.model,u.input_tokens,u.output_tokens,u.accounting,
            d.cache_read_tokens,d.cache_write_tokens,d.input_includes_cache,d.price_json,d.usage_rowid,d.benchmark_json
            FROM usage u LEFT JOIN usage_details d ON d.usage_rowid=u.rowid WHERE u.ts>=?1 AND u.ts<?2";

pub(super) fn read_usage_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    let model: String = row.get(3)?;
    let mut price_snapshot = parse_price(row.get(10)?, 10)?;
    if row.get::<_, Option<i64>>(11)?.is_none() {
        price_snapshot = PriceSnapshot::builtin(&model);
    }
    if price_snapshot
        .as_ref()
        .is_some_and(|price| price.model != model)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let accounting = match row.get::<_, Option<String>>(6)?.as_deref() {
        Some("reported") => Accounting::Reported,
        Some("estimated") => Accounting::Estimated,
        _ => Accounting::Partial,
    };
    Ok(UsageRecord {
        ts: row.get(0)?,
        agent: row.get(1)?,
        provider: row.get(2)?,
        model,
        input_tokens: row.get(4)?,
        output_tokens: row.get(5)?,
        accounting,
        cache_read_tokens: row.get(7)?,
        cache_write_tokens: row.get(8)?,
        input_includes_cache: row.get(9)?,
        price_snapshot,
        benchmark_snapshot: parse_price(row.get(12)?, 12)?,
        request_id: None,
    })
}

pub(super) fn summarize_values(
    rows: impl IntoIterator<Item = rusqlite::Result<UsageRecord>>,
) -> rusqlite::Result<ValueSummary> {
    let mut summary = ValueSummary::default();
    let mut providers: BTreeMap<String, ProviderValue> = BTreeMap::new();
    let mut hits = 0_u64;
    for row in rows {
        let record = row?;
        summary.records += 1;
        let provider = providers
            .entry(record.provider.clone())
            .or_insert_with(|| ProviderValue {
                provider: record.provider.clone(),
                ..ProviderValue::default()
            });
        provider.records += 1;
        summary.cache_read_tokens = summary
            .cache_read_tokens
            .saturating_add(record.cache_read_tokens.unwrap_or_default());
        summary.cache_write_tokens = summary
            .cache_write_tokens
            .saturating_add(record.cache_write_tokens.unwrap_or_default());
        if let Some(input) = record.cache_input() {
            summary.cache_reported_records += 1;
            summary.cache_input_tokens = summary.cache_input_tokens.saturating_add(input);
            hits = hits.saturating_add(record.cache_read_tokens.unwrap_or_default());
        } else {
            summary.cache_unknown_records += 1;
        }
        if let Some(benchmark) = &record.benchmark_snapshot {
            if !benchmark.applies(&record.provider, &record.model)
                || benchmark.basis != "user-benchmark"
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            summary.benchmark_priced_records += 1;
            if let Some(estimate) = record.benchmark_estimate() {
                summary.benchmark_valued_records += 1;
                summary.benchmark_usd =
                    Some(summary.benchmark_usd.unwrap_or_default() + estimate.api_equivalent_usd);
            }
        }
        if let Some(price) = &record.price_snapshot {
            if !price.applies(&record.provider, &record.model) || price.basis == "user-benchmark" {
                return Err(rusqlite::Error::InvalidQuery);
            }
            if price.basis == "provider-directory" {
                summary.official_priced_records += 1;
            }
            summary.priced_records += 1;
            provider.priced_records += 1;
            if price.source.starts_with("builtin-benchmark") {
                summary.builtin_priced_records += 1;
            }
            if let Some(estimate) = record.estimate_with(price) {
                summary.valued_records += 1;
                summary.partial_usage_records += u64::from(estimate.partial_usage);
                summary.cache_rate_fallback_records += u64::from(estimate.cache_rate_fallback);
                let amount = estimate.api_equivalent_usd;
                if price.basis == "provider-directory" {
                    summary.official_valued_records += 1;
                    summary.official_usd = Some(summary.official_usd.unwrap_or_default() + amount);
                }
                summary.api_equivalent_usd =
                    Some(summary.api_equivalent_usd.unwrap_or_default() + amount);
                provider.api_equivalent_usd =
                    Some(provider.api_equivalent_usd.unwrap_or_default() + amount);
                if let Some(delta) = estimate.cache_value_delta_usd {
                    summary.cache_value_known_records += 1;
                    summary.cache_value_delta_usd =
                        Some(summary.cache_value_delta_usd.unwrap_or_default() + delta);
                }
            }
        } else {
            summary.unpriced_records += 1;
        }
    }
    summary.price_coverage =
        (summary.records > 0).then(|| summary.priced_records as f64 / summary.records as f64);
    summary.official_price_coverage = (summary.records > 0)
        .then(|| summary.official_priced_records as f64 / summary.records as f64);
    summary.benchmark_coverage = (summary.records > 0)
        .then(|| summary.benchmark_priced_records as f64 / summary.records as f64);
    summary.cache_hit_rate =
        (summary.cache_input_tokens > 0).then(|| hits as f64 / summary.cache_input_tokens as f64);
    if summary.records == 0 {
        summary.api_equivalent_usd = Some(0.0);
    }
    if summary
        .benchmark_usd
        .is_some_and(|value| !value.is_finite())
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if summary
        .api_equivalent_usd
        .is_some_and(|value| !value.is_finite())
        || summary
            .cache_value_delta_usd
            .is_some_and(|value| !value.is_finite())
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    summary.providers = providers.into_values().collect();
    summary.providers.sort_by(|a, b| {
        b.api_equivalent_usd
            .unwrap_or(-1.0)
            .total_cmp(&a.api_equivalent_usd.unwrap_or(-1.0))
    });
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(includes: bool) -> UsageRecord {
        UsageRecord {
            ts: 1000,
            provider: "vendor".into(),
            model: "priced".into(),
            input_tokens: Some(if includes { 20 } else { 4 }),
            output_tokens: Some(10),
            accounting: Accounting::Reported,
            cache_read_tokens: Some(12),
            cache_write_tokens: Some(4),
            input_includes_cache: Some(includes),
            price_snapshot: Some(PriceSnapshot {
                model: "priced".into(),
                source: "declared".into(),
                observed_at: Some(900),
                price: ModelPrice {
                    input_per_million: 2.0,
                    output_per_million: 5.0,
                    cache_read_per_million: Some(0.2),
                    cache_write_per_million: Some(3.0),
                },
                ..PriceSnapshot::default()
            }),
            ..UsageRecord::default()
        }
    }

    #[test]
    fn cache_arithmetic_is_protocol_independent_and_unknown_is_not_a_miss() {
        for includes in [false, true] {
            let record = record(includes);
            let estimate = record.estimate().unwrap();
            assert!((estimate.api_equivalent_usd - 72.4 / 1e6).abs() < 1e-12);
            assert!((estimate.cache_value_delta_usd.unwrap() - 17.6 / 1e6).abs() < 1e-12);
            let ledger = Ledger::open(":memory:").unwrap();
            ledger.record(&record).unwrap();
            let mut unknown = record.clone();
            unknown.cache_read_tokens = None;
            unknown.cache_write_tokens = None;
            unknown.model = "unpriced".into();
            unknown.price_snapshot = None;
            ledger.record(&unknown).unwrap();
            let summary = ledger.value_summary(0, 2000).unwrap();
            assert_eq!(summary.records, 2);
            assert_eq!(summary.priced_records, 1);
            assert_eq!(summary.cache_unknown_records, 1);
            assert_eq!(summary.cache_hit_rate, Some(0.6));
        }
    }

    #[test]
    fn stored_prices_survive_restart_and_legacy_writer_remains_compatible() {
        let root = std::env::temp_dir().join(format!(
            "aubridge-value-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("ledger.sqlite");
        let ledger = Ledger::open(&database).unwrap();
        let mut original = record(true);
        ledger.record(&original).unwrap();
        original
            .price_snapshot
            .as_mut()
            .unwrap()
            .price
            .input_per_million = 999.0;
        drop(ledger);
        let ledger = Ledger::open(&database).unwrap();
        assert!(
            (ledger
                .value_summary(0, 2000)
                .unwrap()
                .api_equivalent_usd
                .unwrap()
                - 72.4 / 1e6)
                .abs()
                < 1e-12
        );
        ledger
            .conn
            .execute(
                "INSERT INTO usage VALUES(1001,'old','legacy','glm-4.5-flash',100,10,'reported')",
                [],
            )
            .unwrap();
        let summary = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(summary.records, 2);
        assert_eq!(summary.priced_records, 2);
        assert_eq!(summary.builtin_priced_records, 1);
        assert_eq!(summary.cache_unknown_records, 1);
        ledger
            .conn
            .execute("UPDATE usage_details SET price_json='broken'", [])
            .unwrap();
        assert!(ledger.value_summary(0, 2000).is_err());
        drop(ledger);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreported_cache_has_no_hit_rate_and_a_reported_zero_is_zero() {
        let ledger = Ledger::open(":memory:").unwrap();
        let mut unknown = record(true);
        unknown.cache_read_tokens = None;
        unknown.cache_write_tokens = None;
        ledger.record(&unknown).unwrap();
        assert_eq!(ledger.value_summary(0, 2000).unwrap().cache_hit_rate, None);
        let mut zero = record(true);
        zero.cache_read_tokens = Some(0);
        zero.cache_write_tokens = Some(0);
        ledger.record(&zero).unwrap();
        let summary = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(summary.cache_hit_rate, Some(0.0));
        assert_eq!(summary.cache_reported_records, 1);
        assert_eq!(summary.cache_unknown_records, 1);
    }

    #[test]
    fn absent_cache_rates_are_explicit_estimates_and_unknown_prices_stay_null() {
        let mut record = record(true);
        record
            .price_snapshot
            .as_mut()
            .unwrap()
            .price
            .cache_read_per_million = None;
        let estimate = record.estimate().unwrap();
        assert!(estimate.cache_rate_fallback);
        assert!(estimate.cache_value_delta_usd.is_none());
        record.price_snapshot = None;
        assert!(record.estimate().is_none());
        record.cache_read_tokens = Some(40);
        assert!(record.cache_input().is_none());
    }
    fn benchmark() -> PriceSnapshot {
        PriceSnapshot::benchmark(&manifest::model::BenchmarkMapping {
            provider: "vendor".into(),
            model: "priced".into(),
            target_provider: "chosen-vendor".into(),
            target_model: "chosen-model".into(),
            source: "user provided comparison".into(),
            version: "v1".into(),
            observed_at: 900,
            price: ModelPrice {
                input_per_million: 10.0,
                output_per_million: 20.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        })
        .unwrap()
    }

    #[test]
    fn official_zero_and_user_comparison_are_independent_immutable_evidence() {
        let ledger = Ledger::open(":memory:").unwrap();
        let mut r = record(true);
        let official = r.price_snapshot.as_mut().unwrap();
        official.basis = "provider-directory".into();
        official.provider = "vendor".into();
        official.source = "https://vendor.example/v1/models".into();
        official.version = Some("directory-900".into());
        official.price = ModelPrice::default();
        r.benchmark_snapshot = Some(benchmark());
        ledger
            .record_requests(&[super::super::CompletedRequest {
                decision: super::super::DecisionRecord {
                    ts: r.ts,
                    model: r.model.clone(),
                    provider: r.provider.clone(),
                    served_model: Some(r.model.clone()),
                    price_snapshot: r.price_snapshot.clone(),
                    benchmark_snapshot: r.benchmark_snapshot.clone(),
                    attempts: "[]".into(),
                    outcome: "ok".into(),
                    ..Default::default()
                },
                usage: vec![r.clone()],
            }])
            .unwrap();
        let before = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(before.official_usd, Some(0.0));
        assert!(before.benchmark_usd.unwrap() > 0.0);
        assert_eq!(before.official_price_coverage, Some(1.0));
        assert_eq!(before.benchmark_coverage, Some(1.0));
        r.benchmark_snapshot
            .as_mut()
            .unwrap()
            .price
            .input_per_million = 999.0;
        let after = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(before.benchmark_usd, after.benchmark_usd);
        assert_eq!(before.api_equivalent_usd, after.api_equivalent_usd);
        assert_eq!(ledger.clear_request_history(&[]).unwrap(), 1);
        assert_eq!(
            ledger.value_summary(0, 2000).unwrap().benchmark_usd,
            before.benchmark_usd
        );
    }

    #[test]
    fn explicit_null_is_not_backfilled_and_bad_scopes_rollback() {
        let ledger = Ledger::open(":memory:").unwrap();
        let mut r = record(true);
        r.model = "glm-4.5-flash".into();
        r.price_snapshot = None;
        ledger.record(&r).unwrap();
        let summary = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(summary.priced_records, 0);
        assert_eq!(summary.api_equivalent_usd, None);
        assert_eq!(summary.benchmark_usd, None);
        r.model = "priced".into();
        r.benchmark_snapshot = Some(benchmark());
        r.provider = "other".into();
        assert!(ledger.record(&r).is_err());
        assert_eq!(ledger.value_summary(0, 2000).unwrap().records, 1);
    }

    #[test]
    fn unknown_official_price_can_have_an_explicit_user_benchmark() {
        let ledger = Ledger::open(":memory:").unwrap();
        let mut r = record(true);
        r.price_snapshot = None;
        r.benchmark_snapshot = Some(benchmark());
        ledger.record(&r).unwrap();
        let summary = ledger.value_summary(0, 2000).unwrap();
        assert_eq!(summary.official_price_coverage, Some(0.0));
        assert_eq!(summary.price_coverage, Some(0.0));
        assert_eq!(summary.benchmark_coverage, Some(1.0));
        assert!(summary.benchmark_usd.is_some());
        assert!(r.benchmark_estimate().unwrap().cache_rate_fallback);
        assert!(r
            .benchmark_estimate()
            .unwrap()
            .cache_value_delta_usd
            .is_none());
    }
}

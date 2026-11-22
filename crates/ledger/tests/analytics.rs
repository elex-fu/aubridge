//! Synthetic ledger facts only: no user database, provider or credentials.
use ledger::{
    Accounting, AnalyticsContext, AnalyticsError, CompletedRequest, DecisionFilter, DecisionRecord,
    Ledger, PriceSnapshot, UsageRecord,
};
use manifest::model::ModelPrice;
use serde_json::{json, Value};
const NOW: u64 = 2_000_000_000;
fn context<'a>(pending: &'a [u64]) -> AnalyticsContext<'a> {
    AnalyticsContext {
        now: NOW,
        retention_since: 0,
        pending_timestamps: pending,
        active_inference: false,
    }
}
fn priced(ts: u64, provider: &str, basis: &str) -> UsageRecord {
    UsageRecord {
        ts,
        agent: "tool".into(),
        provider: provider.into(),
        model: "actual".into(),
        input_tokens: Some(100),
        output_tokens: Some(10),
        accounting: Accounting::Reported,
        cache_read_tokens: Some(20),
        cache_write_tokens: Some(0),
        input_includes_cache: Some(true),
        price_snapshot: Some(PriceSnapshot {
            model: "actual".into(),
            provider: provider.into(),
            basis: basis.into(),
            source: "fixture-directory".into(),
            version: Some("fixture-v1".into()),
            observed_at: Some(ts),
            price: ModelPrice {
                input_per_million: 2.0,
                output_per_million: 5.0,
                cache_read_per_million: Some(0.2),
                cache_write_per_million: Some(3.0),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn request(
    ts: u64,
    outcome: &str,
    duration: Option<u64>,
    attempts: Value,
    usage: Vec<UsageRecord>,
) -> CompletedRequest {
    CompletedRequest {
        decision: DecisionRecord {
            ts,
            agent: "tool".into(),
            model: "requested-alias".into(),
            provider: if outcome == "ok" {
                "final".into()
            } else {
                String::new()
            },
            served_model: (outcome == "ok").then_some("actual".into()),
            outcome: outcome.into(),
            duration_ms: duration,
            attempts: attempts.to_string(),
            ..Default::default()
        },
        usage,
    }
}
#[test]
fn calendar_validation_half_open_days_and_history_drilldown() {
    let ledger = Ledger::open(":memory:").unwrap();
    for (start, end) in [
        ("2024-2-01", "2024-02-02"),
        ("2023-02-29", "2023-03-01"),
        ("2024-04-31", "2024-05-01"),
        ("2024-01-01", "2024-01-01"),
        ("2024-01-01", "2024-02-02"),
        ("2099-01-01", "2099-01-02"),
    ] {
        assert!(
            matches!(
                ledger.analytics_window(start, end, NOW),
                Err(AnalyticsError::Invalid)
            ),
            "{start} {end}"
        );
    }
    let window = ledger
        .analytics_window("2024-02-29", "2024-03-02", NOW)
        .unwrap();
    ledger
        .record_requests(&[
            request(window.start_ts, "ok", Some(0), json!([]), vec![]),
            request(window.end_ts - 1, "cancelled", None, json!([]), vec![]),
            request(window.end_ts, "ok", Some(5), json!([]), vec![]),
        ])
        .unwrap();
    let report = ledger
        .analytics("2024-02-29", "2024-03-02", false, context(&[]))
        .unwrap();
    assert_eq!(report.summary.requests.total, 2);
    assert_eq!(report.days.len(), 2);
    assert_eq!(report.days[0].date, "2024-02-29");
    assert_eq!(report.summary.requests.duration.p50_ms, Some(0));
    let page = ledger
        .decision_page_in_range(
            DecisionFilter::default(),
            1,
            "2024-02-29",
            "2024-03-02",
            NOW,
        )
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].ts, window.end_ts - 1);
    let next = ledger
        .decision_page_in_range(
            DecisionFilter {
                before: page.next,
                ..Default::default()
            },
            1,
            "2024-02-29",
            "2024-03-02",
            NOW,
        )
        .unwrap();
    assert_eq!(next.rows[0].ts, window.start_ts);
    assert!(ledger
        .analytics("2024-02-29", "2024-03-02", false, context(&[]))
        .unwrap()
        .comparison
        .is_none());
}
#[test]
fn decisions_usage_prices_and_missing_are_independent() {
    let ledger = Ledger::open(":memory:").unwrap();
    let at = ledger
        .analytics_window("2024-03-01", "2024-03-03", NOW)
        .unwrap()
        .start_ts;
    let full = priced(at, "attempt-source", "provider-directory");
    let mut partial = priced(at + 1, "attempt-source", "local-declaration");
    partial.accounting = Accounting::Partial;
    partial.cache_read_tokens = None;
    partial.cache_write_tokens = None;
    partial.output_tokens = None;
    partial.input_includes_cache = Some(false);
    let mut a = request(
        at,
        "ok",
        Some(10),
        json!([{"id":"a"},{"id":"b"}]),
        vec![full],
    );
    let mut b = request(at + 1, "ok", None, json!([]), vec![partial]);
    // Duplicate request IDs are still two persisted decision facts.
    a.decision.request_id = Some("duplicate".into());
    b.decision.request_id = Some("duplicate".into());
    ledger
        .record_requests(&[
            a,
            b,
            request(at + 2, "invalid_request", Some(999), json!([]), vec![]),
            request(at + 3, "cancelled", None, json!([]), vec![]),
        ])
        .unwrap();
    let unlinked = UsageRecord {
        ts: at + 4,
        agent: "other-tool".into(),
        provider: "orphan".into(),
        model: "unpriced".into(),
        output_tokens: Some(3),
        accounting: Accounting::Estimated,
        ..Default::default()
    };
    ledger.record(&unlinked).unwrap();
    let report = ledger
        .analytics("2024-03-01", "2024-03-03", false, context(&[]))
        .unwrap();
    let r = &report.summary.requests;
    assert_eq!((r.total, r.ok, r.failed, r.cancelled), (4, 2, 1, 1));
    assert_eq!(r.retried_requests, 1);
    assert_eq!(r.duration.known, 1);
    assert_eq!(r.duration.missing, 1);
    assert_eq!(r.duration.p95_ms, Some(10));
    assert!((r.success_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
    let u = &report.summary.usage;
    assert_eq!(
        (u.records, u.reported, u.partial, u.estimated),
        (3, 1, 1, 1)
    );
    assert_eq!(u.input.tokens, Some(100));
    assert_eq!((u.input.known, u.input.missing), (1, 2));
    assert_eq!(u.output.tokens, Some(13));
    assert_eq!(u.cache.hit_read_tokens, 20);
    assert_eq!(u.cache.hit_input_tokens, 100);
    assert_eq!(u.cache.unknown_records, 2);
    assert_eq!(u.prices.directory.priced, 1);
    assert_eq!(u.prices.directory.valued, 1);
    assert_eq!(u.prices.declared.partial_valued, 1);
    assert_eq!(u.prices.declared.fallback_valued, 1);
    let final_group = report
        .groups
        .providers
        .iter()
        .find(|row| row.key.as_deref() == Some("final"))
        .unwrap();
    assert_eq!(final_group.requests.total, 2);
    assert_eq!(final_group.usage.records, 0);
    let attempt_group = report
        .groups
        .providers
        .iter()
        .find(|row| row.key.as_deref() == Some("attempt-source"))
        .unwrap();
    assert_eq!(attempt_group.requests.total, 0);
    assert_eq!(attempt_group.usage.records, 2);
    assert_eq!(report.days[1].metrics.requests.total, 0);
    assert!(report.days[1].metrics.usage.input.tokens.is_none());
    let before = ledger.value_summary(at, at + 10).unwrap();
    ledger.clear_request_history(&[]).unwrap();
    let cleared = ledger
        .analytics("2024-03-01", "2024-03-03", false, context(&[]))
        .unwrap();
    assert_eq!(cleared.summary.requests.total, 0);
    assert_eq!(cleared.summary.usage.records, 3);
    assert_eq!(
        ledger
            .value_summary(at, at + 10)
            .unwrap()
            .api_equivalent_usd,
        before.api_equivalent_usd
    );
}
#[test]
fn comparison_positive_path_and_all_refusal_reasons() {
    let ledger = Ledger::open(":memory:").unwrap();
    let previous = ledger
        .analytics_window("2024-03-01", "2024-03-02", NOW)
        .unwrap();
    let current = ledger
        .analytics_window("2024-03-02", "2024-03-03", NOW)
        .unwrap();
    ledger
        .record_requests(&[
            request(
                previous.start_ts,
                "ok",
                Some(10),
                json!([]),
                vec![priced(previous.start_ts, "p", "provider-directory")],
            ),
            request(
                current.start_ts,
                "ok",
                Some(20),
                json!([]),
                vec![priced(current.start_ts, "p", "provider-directory")],
            ),
            request(
                current.start_ts + 1,
                "ok",
                Some(30),
                json!([]),
                vec![priced(current.start_ts + 1, "p", "provider-directory")],
            ),
        ])
        .unwrap();
    let report = ledger
        .analytics("2024-03-02", "2024-03-03", true, context(&[]))
        .unwrap();
    let comparison = report.comparison.unwrap();
    assert_eq!(comparison.window.start, "2024-03-01");
    assert_eq!(comparison.requests_change.rate, Some(1.0));
    assert_eq!(comparison.directory_usd_change.rate, Some(1.0));
    let retention = AnalyticsContext {
        retention_since: previous.start_ts + 1,
        ..context(&[])
    };
    assert_eq!(
        ledger
            .analytics("2024-03-02", "2024-03-03", true, retention)
            .unwrap()
            .comparison
            .unwrap()
            .requests_change
            .reason,
        Some("retention_unknown")
    );
    let pending = [previous.start_ts];
    assert_eq!(
        ledger
            .analytics("2024-03-02", "2024-03-03", true, context(&pending))
            .unwrap()
            .comparison
            .unwrap()
            .requests_change
            .reason,
        Some("pending_settlement")
    );
    let active = AnalyticsContext {
        active_inference: true,
        ..context(&[])
    };
    assert_eq!(
        ledger
            .analytics("2024-03-02", "2024-03-03", true, active)
            .unwrap()
            .comparison
            .unwrap()
            .requests_change
            .reason,
        Some("pending_settlement")
    );
    let outside = [current.end_ts];
    assert!(ledger
        .analytics("2024-03-02", "2024-03-03", true, context(&outside))
        .unwrap()
        .comparison
        .unwrap()
        .requests_change
        .reason
        .is_none());
    let open = AnalyticsContext {
        now: current.start_ts + 1,
        ..context(&[])
    };
    assert_eq!(
        ledger
            .analytics("2024-03-02", "2024-03-03", true, open)
            .unwrap()
            .comparison
            .unwrap()
            .requests_change
            .reason,
        Some("open_period")
    );
    let no_baseline = ledger
        .analytics("2024-02-29", "2024-03-01", true, context(&[]))
        .unwrap()
        .comparison
        .unwrap();
    assert_eq!(no_baseline.requests_change.reason, Some("no_baseline"));
    assert_eq!(
        no_baseline.directory_usd_change.reason,
        Some("incomplete_pricing")
    );
    ledger
        .record(&UsageRecord {
            ts: current.start_ts + 5,
            model: "unknown".into(),
            output_tokens: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        ledger
            .analytics("2024-03-02", "2024-03-03", true, context(&[]))
            .unwrap()
            .comparison
            .unwrap()
            .directory_usd_change
            .reason,
        Some("incomplete_pricing")
    );
}
#[test]
fn groups_keep_unknown_separate_from_other_and_aggregate_exactly() {
    let ledger = Ledger::open(":memory:").unwrap();
    let at = ledger
        .analytics_window("2024-01-01", "2024-01-02", NOW)
        .unwrap()
        .start_ts;
    let mut rows = vec![];
    for index in 0..25 {
        for _ in 0..2 {
            let mut row = request(at, "ok", Some(index), json!([]), vec![]);
            row.decision.agent = format!("tool-{index:02}");
            rows.push(row);
        }
    }
    rows.push(request(at, "failed", None, json!([]), vec![]));
    rows.last_mut().unwrap().decision.agent = String::new();
    ledger.record_requests(&rows).unwrap();
    let report = ledger
        .analytics("2024-01-01", "2024-01-02", false, context(&[]))
        .unwrap();
    assert_eq!(report.groups.agents.len(), 21);
    let unknown = report
        .groups
        .agents
        .iter()
        .find(|row| row.key.is_none() && !row.other)
        .unwrap();
    assert_eq!(unknown.requests.total, 1);
    assert_eq!(
        report
            .groups
            .agents
            .iter()
            .filter(|row| row.key.is_some())
            .count(),
        19
    );
    let other = report.groups.agents.last().unwrap();
    assert!(other.other);
    assert!(other.key.is_none());
    assert_eq!(other.requests.total, 12);
    assert_eq!(
        report
            .groups
            .agents
            .iter()
            .map(|row| row.requests.total)
            .sum::<u64>(),
        51
    );
    assert_eq!(report.summary.requests.duration.p50_ms, Some(12));
    assert_eq!(report.summary.requests.duration.p95_ms, Some(23));
}

#[test]
fn known_zero_price_and_all_cancelled_are_not_unknown_or_zero_rate() {
    let ledger = Ledger::open(":memory:").unwrap();
    let previous = ledger
        .analytics_window("2024-01-01", "2024-01-02", NOW)
        .unwrap();
    let current = ledger
        .analytics_window("2024-01-02", "2024-01-03", NOW)
        .unwrap();
    for at in [previous.start_ts, current.start_ts] {
        let mut usage = priced(at, "p", "provider-directory");
        usage.price_snapshot.as_mut().unwrap().price = ModelPrice {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cache_read_per_million: Some(0.0),
            cache_write_per_million: Some(0.0),
        };
        ledger
            .record_requests(&[request(at, "cancelled", Some(99), json!([]), vec![usage])])
            .unwrap();
    }
    let report = ledger
        .analytics("2024-01-02", "2024-01-03", true, context(&[]))
        .unwrap();
    assert_eq!(report.summary.requests.success_rate, None);
    assert_eq!(report.summary.requests.duration.known, 0);
    assert_eq!(report.summary.usage.prices.directory.usd, Some(0.0));
    assert_eq!(
        report.comparison.unwrap().directory_usd_change.reason,
        Some("no_baseline")
    );
}

#[test]
fn having_a_price_does_not_make_unknown_cache_attribution_valuable() {
    let ledger = Ledger::open(":memory:").unwrap();
    let at = ledger
        .analytics_window("2024-01-01", "2024-01-02", NOW)
        .unwrap()
        .start_ts;
    ledger
        .record(&priced(at, "p", "provider-directory"))
        .unwrap();
    let mut ambiguous = priced(at + 1, "p", "provider-directory");
    ambiguous.input_includes_cache = None;
    ledger.record(&ambiguous).unwrap();
    let report = ledger
        .analytics(
            "2024-01-01",
            "2024-01-02",
            false,
            AnalyticsContext {
                active_inference: true,
                ..context(&[])
            },
        )
        .unwrap();
    assert!(report.comparison.is_none());
    assert_eq!(report.summary.usage.prices.directory.priced, 2);
    assert_eq!(report.summary.usage.prices.directory.valued, 1);
    assert_eq!(report.summary.usage.input.missing, 1);
    let old = ledger.value_summary(at, at + 2).unwrap();
    assert_eq!(report.summary.usage.prices.directory.usd, old.official_usd);
}

#[test]
fn malformed_attempt_objects_do_not_prove_retry_or_known_attempts() {
    let ledger = Ledger::open(":memory:").unwrap();
    let at = ledger
        .analytics_window("2024-01-01", "2024-01-02", NOW)
        .unwrap()
        .start_ts;
    let values = [
        json!([{}, {}]),
        json!([{"id":""},{"id":"b"}]),
        json!([{"id":4},{"id":"b"}]),
        json!([{"id":" "}]),
        json!(["a", "b"]),
        json!([]),
        json!([{"id":"a"}]),
        json!([{"id":"a"},{"id":"b"}]),
    ];
    let rows: Vec<_> = values
        .into_iter()
        .map(|value| request(at, "ok", Some(1), value, vec![]))
        .collect();
    ledger.record_requests(&rows).unwrap();
    let report = ledger
        .analytics("2024-01-01", "2024-01-02", false, context(&[]))
        .unwrap();
    assert_eq!(report.summary.requests.total, 8);
    assert_eq!(report.summary.requests.attempt_known_requests, 3);
    assert_eq!(report.summary.requests.retried_requests, 1);
}

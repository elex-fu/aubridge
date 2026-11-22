use ledger::{Accounting, CompletedRequest, DecisionFilter, DecisionRecord, Ledger, UsageRecord};
fn request(agent: &str, session: Option<&str>, request: &str, ts: u64) -> CompletedRequest {
    CompletedRequest {
        decision: DecisionRecord {
            ts,agent:agent.into(),session_id:session.map(str::to_owned),request_id:Some(request.into()),
            duration_ms:Some(70),model:"model".into(),outcome:"ok".into(),purpose:Some("compact".into()),
            attempts:r#"[{"id":"free","model":"model","kind":"free_tier","status":429,"reason":"quota"},{"id":"sub","model":"model","kind":"subscription","status":200,"chosen":true}]"#.into(),
            ..Default::default()
        },
        usage: vec![UsageRecord {ts,agent:agent.into(),provider:"free".into(),model:"model".into(),request_id:Some(request.into()),input_tokens:Some(100),output_tokens:Some(10),accounting:Accounting::Partial,..Default::default()},
            UsageRecord {ts,agent:agent.into(),provider:"sub".into(),model:"model".into(),request_id:Some(request.into()),input_tokens:Some(100),output_tokens:Some(10),cache_read_tokens:Some(50),cache_write_tokens:Some(0),input_includes_cache:Some(true),accounting:Accounting::Reported,..Default::default()}],
    }
}
#[test]
fn retries_count_request_and_duration_once_and_retain_unknown_cache() {
    let l = Ledger::open(":memory:").unwrap();
    l.record_requests(&[request("codex", Some("thread"), "r1", 1)])
        .unwrap();
    let s = l.session_summary("codex", "thread").unwrap();
    assert_eq!(s.requests, 1);
    assert_eq!(s.duration_ms, 70);
    assert_eq!(s.free_requests, 1);
    assert_eq!(s.subscription_requests, 1);
    assert_eq!(s.purposes["compact"], 1);
    let free = s.sources.iter().find(|s| s.provider == "free").unwrap();
    assert_eq!(free.cache_unknown_records, 1);
    assert_eq!(free.cache_hit_rate, None);
    assert_eq!(free.api_equivalent_usd, None);
    assert_eq!(free.cache_value_delta_usd, None);
    let sub = s.sources.iter().find(|s| s.provider == "sub").unwrap();
    assert_eq!(sub.cache_hit_rate, Some(0.5));
    assert_eq!(sub.cache_reported_records, 1);
}
#[test]
fn identities_are_namespaced_anonymous_excluded_and_history_clear_preserves_usage() {
    let l = Ledger::open(":memory:").unwrap();
    l.record_requests(&[
        request("codex", Some("same"), "a", 1),
        request("opencode", Some("same"), "b", 2),
        request("codex", None, "c", 3),
    ])
    .unwrap();
    let (first, next) = l.session_page(None, None, None, 1).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].agent, "opencode");
    let (second, end) = l.session_page(None, None, next, 1).unwrap();
    assert_eq!(second[0].agent, "codex");
    assert!(end.is_none());
    let page = l
        .decision_page(
            DecisionFilter {
                session_id: Some("same"),
                agent: Some("codex"),
                request_id: Some("a"),
                ..Default::default()
            },
            20,
        )
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    let before = l.value_summary(0, 10).unwrap();
    l.clear_request_history(&[]).unwrap();
    assert!(l.session_page(None, None, None, 20).unwrap().0.is_empty());
    let after = l.value_summary(0, 10).unwrap();
    assert_eq!(before.records, after.records);
    assert_eq!(before.cache_read_tokens, after.cache_read_tokens);
}
#[test]
fn cache_numerator_uses_only_reported_denominator_and_zero_is_known() {
    let l = Ledger::open(":memory:").unwrap();
    let mut a = request("codex", Some("s"), "a", 1);
    a.usage.remove(0);
    a.usage[0].cache_read_tokens = Some(0);
    let mut b = request("codex", Some("s"), "b", 2);
    b.usage.remove(0);
    b.usage[0].input_tokens = None;
    b.usage[0].cache_read_tokens = Some(900);
    l.record_requests(&[a, b]).unwrap();
    let s = l.session_summary("codex", "s").unwrap();
    let source = s.sources.iter().find(|s| s.provider == "sub").unwrap();
    assert_eq!(source.cache_reported_records, 1);
    assert_eq!(source.cache_unknown_records, 1);
    assert_eq!(source.cache_hit_rate, Some(0.0));
}

#[test]
fn cache_value_preserves_negative_creation_cost_and_metadata_rolls_back() {
    let l = Ledger::open(":memory:").unwrap();
    let mut r = request("codex", Some("s"), "negative", 1);
    r.usage.remove(0);
    let usage = &mut r.usage[0];
    usage.cache_read_tokens = Some(0);
    usage.cache_write_tokens = Some(50);
    usage.price_snapshot=Some(serde_json::from_value(serde_json::json!({"model":"model","source":"declared","observed_at":1,"price":{"input_per_million":2.0,"output_per_million":5.0,"cache_read_per_million":0.2,"cache_write_per_million":3.0}})).unwrap());
    r.decision.parent_session_id = Some("parent".into());
    let snapshot = r.usage[0].price_snapshot.clone();
    l.record_requests(&[r]).unwrap();
    let s = l.session_summary("codex", "s").unwrap();
    assert_eq!(s.parent_session_ids, vec!["parent"]);
    assert!(
        (s.sources
            .iter()
            .find(|s| s.provider == "sub")
            .unwrap()
            .cache_value_delta_usd
            .unwrap()
            - (-50.0 / 1e6))
            .abs()
            < 1e-12
    );
    let mut invalid = request("codex", Some("bad"), "bad", 2);
    invalid.usage[0].price_snapshot = snapshot;
    invalid.usage[0].price_snapshot.as_mut().unwrap().model = "different".into();
    assert!(l.record_requests(&[invalid]).is_err());
    assert_eq!(l.session_summary("codex", "bad").unwrap().requests, 0);
}

#[test]
fn a_failed_free_attempt_without_usage_remains_visible() {
    let l = Ledger::open(":memory:").unwrap();
    let mut r = request("codex", Some("s"), "r", 1);
    r.usage.remove(0);
    l.record_requests(&[r]).unwrap();
    let s = l.session_summary("codex", "s").unwrap();
    assert_eq!(s.requests, 1);
    assert_eq!(s.free_requests, 1);
    assert_eq!(s.subscription_requests, 1);
    assert_eq!(s.sources.len(), 2);
    let free = s.sources.iter().find(|s| s.provider == "free").unwrap();
    assert_eq!(free.requests, 1);
    assert_eq!(free.attempt_records, 1);
    assert_eq!(free.usage_records, 0);
    assert_eq!(free.unreported_attempts, 1);
    assert_eq!(free.cache_unknown_records, 1);
    assert_eq!(free.api_equivalent_usd, None);
    let sub = s.sources.iter().find(|s| s.provider == "sub").unwrap();
    assert_eq!(sub.usage_records, 1);
}

#[test]
fn benchmark_and_session_metadata_round_trip_without_filling_unknown_api_price() {
    let l = Ledger::open(":memory:").unwrap();
    let mut r = request("codex", Some("thread"), "bench-request", 1);
    r.usage.remove(0);
    let snapshot: ledger::PriceSnapshot = serde_json::from_value(serde_json::json!({
        "provider":"sub","model":"model","basis":"user-benchmark","source":"user fixture",
        "target_provider":"target-provider","target_model":"target-model","version":"fixture-v1",
        "observed_at":1,"price":{"input_per_million":1.0,"output_per_million":2.0}
    }))
    .unwrap();
    r.decision.provider = "sub".into();
    r.decision.served_model = Some("model".into());
    r.decision.parent_session_id = Some("parent-thread".into());
    r.decision.benchmark_snapshot = Some(snapshot.clone());
    r.usage[0].benchmark_snapshot = Some(snapshot.clone());
    l.record_requests(&[r]).unwrap();
    let rows = l.recent_decisions(1).unwrap();
    assert_eq!(rows[0].benchmark_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(rows[0].session_id.as_deref(), Some("thread"));
    assert_eq!(rows[0].parent_session_id.as_deref(), Some("parent-thread"));
    assert_eq!(rows[0].purpose.as_deref(), Some("compact"));
    assert!(rows[0].price_snapshot.is_none());
    let summary = l.session_summary("codex", "thread").unwrap();
    assert_eq!(summary.requests, 1);
    let sub = summary
        .sources
        .iter()
        .find(|source| source.provider == "sub")
        .unwrap();
    assert_eq!(sub.cache_read_tokens, 50);
    assert_eq!(sub.usage_records, 1);
    assert!(sub.api_equivalent_usd.is_none());
    assert_eq!(sub.price_coverage, Some(0.0));
    assert!(l.value_summary(0, 10).unwrap().benchmark_usd.is_some());
}

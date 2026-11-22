#[test]
fn quota_policies_are_explicit_supported_group_rules() {
    for name in ["smart", "pace"] {
        assert!(
            router::GroupRule::parse(name).is_some(),
            "missing quota policy {name}"
        );
    }
}

use manifest::quota::{ObservationSource, QuotaWindow};
use manifest::schema::ProviderKind;
use manifest::state::ProviderState;

fn subscription(id: &str, used: f64, reset: u64, at: u64) -> ProviderState {
    let mut source = ProviderState::new(id, ProviderKind::Subscription, vec![id.into()]);
    let window =
        QuotaWindow::new(604_800, used, Some(reset), at, 1, ObservationSource::Api).unwrap();
    source.quota.observe("fixture", vec![window]);
    source
}

fn ordered(rule: &str, sources: &[ProviderState], now: u64) -> Vec<String> {
    let members: Vec<_> = sources.iter().flat_map(|s| s.models.clone()).collect();
    router::candidates_named_group(
        &members,
        router::GroupRule::parse(rule).unwrap(),
        sources,
        now,
        0,
    )
    .into_iter()
    .map(|(source, _)| source.id.clone())
    .collect()
}

#[test]
fn smart_and_pace_differ_and_always_keep_free_sources_first() {
    let sources = vec![
        subscription("late", 20.0, 8200, 1000),
        subscription("soon", 70.0, 4600, 1000),
        ProviderState::new("free", ProviderKind::FreeTier, vec!["free".into()]),
    ];
    assert_eq!(ordered("smart", &sources, 1000), ["free", "soon", "late"]);
    assert_eq!(ordered("pace", &sources, 1000), ["free", "late", "soon"]);
    assert_eq!(
        ordered("sequential", &sources, 1000),
        ["late", "soon", "free"]
    );
    assert_eq!(
        ordered("round_robin", &sources, 1000),
        ["late", "soon", "free"]
    );
    assert_eq!(ordered("free_only", &sources, 1000), ["free"]);
}

#[test]
fn old_unknown_reset_or_full_readings_remain_fallbacks_in_stable_order() {
    for rule in ["smart", "pace"] {
        let mut sources = vec![
            subscription("stale", 1.0, 2000, 600),
            subscription("full", 100.0, 2000, 1000),
            subscription("ready", 20.0, 8200, 1000),
            ProviderState::new(
                "unknown",
                ProviderKind::Subscription,
                vec!["unknown".into()],
            ),
        ];
        assert_eq!(
            ordered(rule, &sources, 1000),
            ["ready", "stale", "unknown", "full"]
        );
        sources[2].quota.windows[0].resets_at = Some(999);
        assert_eq!(
            ordered(rule, &sources, 1000),
            ["stale", "ready", "unknown", "full"]
        );
        let first = subscription("one", 20.0, 8200, 1000);
        let second = subscription("two", 20.0, 8200, 1000);
        assert_eq!(ordered(rule, &[first, second], 1000), ["one", "two"]);
    }
}

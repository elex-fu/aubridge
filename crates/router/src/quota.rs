//! Quota ordering is restricted to ready subscriptions; the free tier stays first.

use std::cmp::Ordering;

use manifest::schema::ProviderKind;
use manifest::state::ProviderState;

#[derive(Clone, Copy)]
pub(crate) enum QuotaPolicy {
    Smart,
    Pace,
}

struct Rank {
    band: u8,
    soon: Option<u64>,
    pace: Option<f64>,
}

fn rank(source: &ProviderState, now: u64) -> Rank {
    let mut main = None;
    let mut exhausted = false;
    for window in source
        .quota
        .windows
        .iter()
        .filter(|window| window.fresh(now))
    {
        exhausted |= window.used_percent >= 100.0;
        if main.is_none_or(|previous: &manifest::quota::QuotaWindow| {
            window.span_secs > previous.span_secs
        }) {
            main = Some(window);
        }
    }
    let Some(main) = main else {
        return Rank {
            band: 1,
            soon: None,
            pace: None,
        };
    };
    let soon = main.resets_at.filter(|at| *at > now);
    let pace = soon.map(|at| (100.0 - main.used_percent) / ((at - now) as f64 / 3600.0));
    Rank {
        band: if exhausted { 2 } else { 0 },
        soon,
        pace,
    }
}

pub(crate) fn prioritize<'a>(
    candidates: &mut [(&'a ProviderState, &'a str)],
    policy: QuotaPolicy,
    now: u64,
) {
    let positions: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (source, _))| {
            source.kind == ProviderKind::Subscription
                && source.enabled
                && source.healthy
                && !source.is_resting(now)
                && !source.sunk
                && !source.is_breaker_open(now)
        })
        .map(|(index, _)| index)
        .collect();
    let mut ordered: Vec<_> = positions
        .iter()
        .map(|&index| {
            let candidate = candidates[index];
            (candidate, rank(candidate.0, now))
        })
        .collect();
    ordered.sort_by(|(_, a), (_, b)| {
        a.band.cmp(&b.band).then_with(|| {
            if a.band == 1 {
                return Ordering::Equal;
            }
            match policy {
                QuotaPolicy::Smart => a.soon.unwrap_or(u64::MAX).cmp(&b.soon.unwrap_or(u64::MAX)),
                QuotaPolicy::Pace => b.pace.unwrap_or(-1.0).total_cmp(&a.pace.unwrap_or(-1.0)),
            }
        })
    });
    for (index, (candidate, _)) in positions.into_iter().zip(ordered) {
        candidates[index] = candidate;
    }
}

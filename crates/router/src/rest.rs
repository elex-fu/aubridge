//! rest/sink 状态转移(纯函数,设计文档 §8.1)。
//!
//! `apply_failure` 把失败决策落到快照上:credit 爬梯
//! (30m→2h→8h→24h→3d→7d,带 Retry-After 的只休 30min 不上梯)、
//! quota 休到 reset、rate 上限 30min、other 60s 起。reset 时刻加
//! 1–30 秒随机 fuzz 防同刻雪崩(CLIProxyAPI 语义)——fuzz 由调用方
//! 注入(`fuzz_secs`),本 crate 保持纯函数可测。

use manifest::state::{AccountRestReason, ProviderState};

use crate::classify::{Bucket, FailureDecision};

const CREDIT_LADDER_SECS: [u64; 6] = [1800, 7200, 28_800, 86_400, 259_200, 604_800];
const OTHER_BASE_SECS: u64 = 60;
const OTHER_MAX_SECS: u64 = 600;

/// 把失败决策应用到快照(返回新快照;原快照不动)。
/// `fuzz_secs`:调用方注入的 0–30 随机秒,加到 rest 时刻上。
pub fn apply_failure(
    state: &ProviderState,
    decision: &FailureDecision,
    now: u64,
    fuzz_secs: u64,
) -> ProviderState {
    let mut next = state.clone();

    if decision.disable {
        next.enabled = false;
        return next;
    }
    if decision.sink {
        next.sunk = true;
    }

    if let Some(bucket) = &decision.bucket {
        next.rest_reason = Some(match bucket {
            Bucket::Credit => {
                let step = (state.credit_ladder_step as usize).min(CREDIT_LADDER_SECS.len() - 1);
                next.credit_ladder_step = (step + 1).min(CREDIT_LADDER_SECS.len() - 1) as u8;
                next.rest_until = Some(now + CREDIT_LADDER_SECS[step] + fuzz_secs);
                AccountRestReason::Credit
            }
            Bucket::QuotaUntil(reset) => {
                next.rest_until = Some(reset.saturating_add(1 + fuzz_secs));
                if let Some(resets_at) = state.resets_at {
                    next.resets_at = Some(resets_at.max(*reset));
                } else {
                    next.resets_at = Some(*reset);
                }
                AccountRestReason::QuotaExhausted
            }
            Bucket::RateFor(secs) => {
                next.rest_until = Some(now + (*secs).min(1800) + fuzz_secs);
                AccountRestReason::RateLimit
            }
            Bucket::Overload => {
                next.rest_until = Some(now + 30 + fuzz_secs);
                AccountRestReason::Other
            }
            Bucket::Other => {
                let secs = OTHER_BASE_SECS.min(OTHER_MAX_SECS);
                next.rest_until = Some(now + secs + fuzz_secs);
                AccountRestReason::Other
            }
        });
    }
    next
}

/// 成功响应后的状态复位:清 rest/sink;额度重置时刻已过则
/// 复位剩余量与 credit 梯(新窗口)。纯函数。
pub fn apply_success(state: &ProviderState, now: u64) -> ProviderState {
    let mut next = state.clone();
    next.rest_until = None;
    next.rest_reason = None;
    next.sunk = false;
    next.credit_ladder_step = 0;
    if state.resets_at.is_some_and(|t| t <= now) {
        // 窗口已重置:剩余量回到未知(等下一次查询/推断)。
        next.resets_at = None;
        next.remaining = None;
    }
    next
}

/// 成功响应的被动采样(§8.2):写入窗口重置时刻(只前移,不回退;
/// 不动 rest/sink)。与 `apply_success` 配套:先复位再采样。
pub fn apply_sample_reset(state: &ProviderState, reset_at: u64) -> ProviderState {
    let mut next = state.clone();
    next.resets_at = Some(state.resets_at.map_or(reset_at, |t| t.max(reset_at)));
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{classify, UpstreamSignal};
    use manifest::schema::ProviderKind;

    fn state() -> ProviderState {
        ProviderState::new("openrouter", ProviderKind::FreeTier, vec!["m".into()])
    }

    #[test]
    fn credit_ladder_progresses() {
        let s = state();
        let mut step0 = s.clone();
        for expected in CREDIT_LADDER_SECS {
            let d = FailureDecision {
                bucket: Some(Bucket::Credit),
                sink: true,
                consumes_retry: true,
                refresh: false,
                disable: false,
            };
            step0 = apply_failure(&step0, &d, 0, 0);
            assert_eq!(step0.rest_until, Some(expected));
        }
        // 封顶:第 7 次仍是 7d。
        assert_eq!(s.credit_ladder_step, 0);
    }

    #[test]
    fn quota_rests_until_reset_plus_fuzz() {
        let d = classify(
            &UpstreamSignal {
                status: 429,
                body: "quota_exceeded".into(),
                reset_at: Some(1000),
                ..Default::default()
            },
            0,
        );
        let next = apply_failure(&state(), &d, 0, 7);
        assert_eq!(next.rest_until, Some(1001 + 7));
        assert!(next.sunk);
        assert_eq!(next.resets_at, Some(1000));
    }

    #[test]
    fn disable_removes_from_candidates() {
        let d = FailureDecision {
            bucket: None,
            sink: false,
            consumes_retry: true,
            refresh: false,
            disable: true,
        };
        let next = apply_failure(&state(), &d, 0, 0);
        assert!(!next.enabled);
    }

    #[test]
    fn success_resets_rest_and_sink() {
        let mut s = state();
        s.sunk = true;
        s.rest_until = Some(100);
        s.credit_ladder_step = 3;
        let next = apply_success(&s, 200);
        assert!(!next.sunk);
        assert_eq!(next.rest_until, None);
        assert_eq!(next.credit_ladder_step, 0);
    }

    #[test]
    fn success_after_window_reset_clears_remaining() {
        let mut s = state();
        s.remaining = Some(0);
        s.resets_at = Some(100);
        let next = apply_success(&s, 101);
        assert_eq!(next.remaining, None);
        assert_eq!(next.resets_at, None);
    }

    #[test]
    fn sample_reset_only_moves_forward() {
        let mut s = state();
        s.resets_at = Some(500);
        // 旧时刻 500,新采样 300:保持 500(不回退)。
        assert_eq!(apply_sample_reset(&s, 300).resets_at, Some(500));
        // 新采样 900:前移。
        assert_eq!(apply_sample_reset(&s, 900).resets_at, Some(900));
        // 未知 → 记录。
        assert_eq!(apply_sample_reset(&state(), 700).resets_at, Some(700));
        // 采样不动 rest/sink。
        let mut r = state();
        r.rest_until = Some(100);
        r.sunk = true;
        let next = apply_sample_reset(&r, 900);
        assert_eq!(next.rest_until, Some(100));
        assert!(next.sunk);
    }
}

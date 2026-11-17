//! 5xx/网络失败熔断器(设计文档 §8.1:5 连败 open,30s 半开,
//! 2 成恢复——model-router 与 CLIProxyAPI 同值)。
//!
//! 状态机:closed(计数)→ open(跳过该源)→ half-open(open_until
//! 已过,放行试探)→ 2 连成恢复 closed;半开失败立即重开。
//! 只计 5xx/408/网络失败——429 等分类桶是额度问题,不是源挂了
//! (magpie 事故:把额度当故障,全部账号休了 5 天)。

use manifest::state::BreakerState;

/// open 阈值(连续失败次数)。
const OPEN_THRESHOLD: u8 = 5;
/// open 时长(半开等待)。
const OPEN_SECS: u64 = 30;
/// 半开恢复所需连续成功数。
const CLOSE_WINS: u8 = 2;

/// 记一次熔断类失败(5xx/408/网络失败)。纯函数。
pub fn record_failure(b: &BreakerState, now: u64) -> BreakerState {
    let mut next = b.clone();
    if b.open_until.is_some() {
        // open/half-open 期失败:重开(半开试探失败立即回 open)。
        next.open_until = Some(now + OPEN_SECS);
        next.half_wins = 0;
        return next;
    }
    next.fail_streak = b.fail_streak.saturating_add(1);
    if next.fail_streak >= OPEN_THRESHOLD {
        next.open_until = Some(now + OPEN_SECS);
        next.half_wins = 0;
    }
    next
}

/// 记一次成功:closed 清计数;half-open 累计到 2 成关闭。纯函数。
pub fn record_success(b: &BreakerState) -> BreakerState {
    let mut next = b.clone();
    if b.open_until.is_some() {
        next.half_wins = b.half_wins.saturating_add(1);
        if next.half_wins >= CLOSE_WINS {
            next.open_until = None;
            next.half_wins = 0;
            next.fail_streak = 0;
        }
        return next;
    }
    next.fail_streak = 0;
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_consecutive_failures_open_30s() {
        let mut b = BreakerState::default();
        for i in 1..=4 {
            b = record_failure(&b, 0);
            assert_eq!(b.open_until, None, "第 {i} 次不应 open");
        }
        b = record_failure(&b, 100);
        assert_eq!(b.open_until, Some(130), "5 连败 open 30s");
        assert_eq!(b.half_wins, 0);
    }

    #[test]
    fn half_open_two_successes_close() {
        // open 后 30s 过去 → half-open;2 连成恢复。
        let mut b = record_failure(&BreakerState::default(), 0);
        b = record_failure(&b, 0);
        b = record_failure(&b, 0);
        b = record_failure(&b, 0);
        b = record_failure(&b, 0);
        assert!(b.open_until.is_some());
        // half-open 成功 ×1:仍 open(记录在案),×2 关闭。
        b = record_success(&b);
        assert_eq!(b.half_wins, 1);
        assert!(b.open_until.is_some());
        b = record_success(&b);
        assert_eq!(b.open_until, None);
        assert_eq!(b.fail_streak, 0);
        assert_eq!(b.half_wins, 0);
    }

    #[test]
    fn half_open_failure_reopens() {
        let mut b = record_failure(
            &record_failure(
                &record_failure(
                    &record_failure(&record_failure(&BreakerState::default(), 0), 0),
                    0,
                ),
                0,
            ),
            0,
        );
        assert_eq!(b.open_until, Some(30));
        // 半开试探失败:重开 30s,成功计数清零。
        b = record_failure(&b, 100);
        assert_eq!(b.open_until, Some(130));
        assert_eq!(b.half_wins, 0);
    }

    #[test]
    fn success_while_closed_resets_streak() {
        let mut b = BreakerState::default();
        b = record_failure(&b, 0);
        b = record_failure(&b, 0);
        b = record_success(&b);
        assert_eq!(b.fail_streak, 0);
        // 清零后再 4 败不 open。
        for _ in 0..4 {
            b = record_failure(&b, 0);
        }
        assert_eq!(b.open_until, None);
    }

    #[test]
    fn streak_saturates_instead_of_overflow() {
        let mut b = BreakerState::default();
        for _ in 0..300 {
            b = record_failure(&b, 0);
        }
        // open 后走重开路径,streak 不再增长,无 panic。
        assert_eq!(b.fail_streak, 5);
    }
}

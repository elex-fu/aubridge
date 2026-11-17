//! 错误分类矩阵(设计文档 §8.1)。
//!
//! 输入是归一化的上游信号(status / body 关键词 / 限额头),
//! 输出结构化决策——不是裸 bool(new-api `PolicyDecision` 模式)。
//! 词表匹配含中文厂商报错(magpie `routing.go`);先判断拒绝属于
//! 账号、模型还是请求内容,再决定 rest(magpie 事故:把"渠道未批准"
//! 当账号故障,全部账号休了 5 天)。

/// 归一化上游信号(由 gateway 从 HTTP 响应提取)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpstreamSignal {
    pub status: u16,
    /// 响应体(小写化后做词表匹配;调用方保证不含凭据)。
    pub body: String,
    /// `Retry-After` 头(秒)。
    pub retry_after: Option<u64>,
    /// 上游自报的额度重置时刻(unix 秒;`resets_at` /
    /// `anthropic-ratelimit-unified-*-reset` 等,由 gateway 解析)。
    pub reset_at: Option<u64>,
    /// 上游自报的剩余量 > 0(如 OpenRouter `remaining`)。
    pub remaining_positive: bool,
    /// 被动采样:窗口用量 ≥ 100%(`x-codex-primary/secondary-used-percent`
    /// 等,§8.2)——真没量,与 quota 词表同语义。
    pub window_exhausted: bool,
    /// 模型级信号(仅该模型不可用,源内其它模型仍可用)。
    pub model_scoped: bool,
    /// 上游明确拒绝当前账号。普通请求拒绝不影响整源。
    pub account_restricted: bool,
}

/// rest 四桶(设计文档 §8.1 四桶表)。
#[derive(Debug, Clone, PartialEq)]
pub enum Bucket {
    /// 402/余额类:爬梯 30m→2h→8h→24h→3d→7d。
    Credit,
    /// 配额/窗口耗尽:休到 reset 时刻。
    QuotaUntil(u64),
    /// 限速:休 retry-after,上限 30 分钟。
    RateFor(u64),
    /// 上游 529，短时休息，仍是服务端故障。
    Overload,
    /// 其余可 rest 错误:60s 起倍增至 10 分钟。
    Other,
}

/// 结构化失败决策。
#[derive(Debug, Clone, PartialEq)]
pub struct FailureDecision {
    pub bucket: Option<Bucket>,
    /// true = 真没量,沉底(sink;429 且仍有量不沉)。
    pub sink: bool,
    /// 换下一个源重试是否消耗重试预算(529 不耗,§8.1)。
    pub consumes_retry: bool,
    /// 触发 token 刷新(401 expired)。
    pub refresh: bool,
    /// 源下架(banned/suspended),不再进候选。
    pub disable: bool,
}

impl FailureDecision {
    /// 不 rest 的失败(形状/网络类):换源但不动状态。
    fn pass_through(consumes_retry: bool) -> Self {
        Self {
            bucket: None,
            sink: false,
            consumes_retry,
            refresh: false,
            disable: false,
        }
    }
}

const CREDIT_WORDS: [&str; 6] = [
    "insufficient funds",
    "credit",
    "balance",
    "欠费",
    "余额不足",
    "payment",
];
const QUOTA_WORDS: [&str; 11] = [
    "insufficient_quota",
    "quota_exceeded",
    "quota_exhausted",
    "quota will reset",
    "quota exceeded",
    "exceeded your",
    "limit reached",
    "daily limit",
    "限额",
    "配额",
    "额度已用",
];
const RATE_WORDS: [&str; 6] = [
    "rate limit",
    "rate_limit",
    "too many requests",
    "频率",
    "太频繁",
    "requests per",
];
/// 分类。纯函数;`now` 用于计算相对秒数的桶时长。
pub fn classify(signal: &UpstreamSignal, now: u64) -> FailureDecision {
    if signal.model_scoped && matches!(signal.status, 400 | 403 | 404) {
        return FailureDecision {
            bucket: Some(Bucket::Other),
            ..FailureDecision::pass_through(true)
        };
    }
    match signal.status {
        401 => FailureDecision {
            bucket: None,
            sink: false,
            consumes_retry: true,
            refresh: true,
            disable: false,
        },
        403 => FailureDecision {
            disable: signal.account_restricted,
            ..FailureDecision::pass_through(true)
        },
        402 => FailureDecision {
            bucket: Some(Bucket::Credit),
            sink: true,
            consumes_retry: true,
            refresh: false,
            disable: false,
        },
        429 => {
            // 明确的账号窗口信号优先于提示用户充值的文案和其他窗口的余量。
            let known_window = signal.window_exhausted
                || signal.body.contains("free-models-per-day")
                || signal.body.contains("account_quota_exceeded")
                || signal.body.contains("account_quota_exhausted");
            let quota = known_window || QUOTA_WORDS.iter().any(|w| signal.body.contains(w));
            let rate = RATE_WORDS.iter().any(|w| signal.body.contains(w));
            let credit = CREDIT_WORDS.iter().any(|w| signal.body.contains(w));

            if credit && !known_window {
                return FailureDecision {
                    bucket: Some(Bucket::Credit),
                    sink: true,
                    consumes_retry: true,
                    refresh: false,
                    disable: false,
                };
            }
            // 429 且仍有配额:被限速不是没量,短 rest 不沉底
            //(矩阵行序:先于 quota/rate 判定)。
            if signal.remaining_positive && !known_window {
                return FailureDecision {
                    bucket: Some(Bucket::RateFor(
                        signal.retry_after.unwrap_or(30).clamp(1, 1800),
                    )),
                    sink: false,
                    consumes_retry: true,
                    refresh: false,
                    disable: false,
                };
            }
            if quota {
                // 真没量:休到重置时刻;sink。
                return FailureDecision {
                    bucket: Some(Bucket::QuotaUntil(signal.reset_at.unwrap_or_else(|| {
                        now + signal.retry_after.unwrap_or(900).clamp(1, 32 * 86_400)
                    }))),
                    sink: true,
                    consumes_retry: true,
                    refresh: false,
                    disable: false,
                };
            }
            if rate || signal.retry_after.is_some() {
                // rate:休到 retry-after,上限 30 分钟(sub2api 最长)。
                let secs = signal.retry_after.unwrap_or(5).clamp(1, 1800);
                return FailureDecision {
                    bucket: Some(Bucket::RateFor(secs)),
                    sink: false,
                    consumes_retry: true,
                    refresh: false,
                    disable: false,
                };
            }
            // 429 无细节、无 reset 头:视为非真实限流,秒级回避不长封
            // (sub2api 语义:这类多为"需开 extra usage"等软限制)。
            FailureDecision {
                bucket: Some(Bucket::RateFor(5)),
                sink: false,
                consumes_retry: true,
                refresh: false,
                disable: false,
            }
        }
        529 => FailureDecision {
            // overloaded:短冷却(30s),不消耗重试预算、不下架。
            bucket: Some(Bucket::Overload),
            sink: false,
            consumes_retry: false,
            refresh: false,
            disable: false,
        },
        408 | 500..=528 | 530..=599 => {
            // 5xx/超时:熔断计数由 gateway 维护,这里不 rest
            // (换源重试,由熔断器决定下架)。
            FailureDecision::pass_through(true)
        }
        _ => FailureDecision::pass_through(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(status: u16, body: &str) -> UpstreamSignal {
        UpstreamSignal {
            status,
            body: body.to_lowercase(),
            ..Default::default()
        }
    }

    #[test]
    fn quota_exceeded_with_reset_sinks_until_reset() {
        let mut s = sig(429, r#"{"error":{"type":"quota_exceeded","remaining":0}}"#);
        s.reset_at = Some(1000);
        let d = classify(&s, 0);
        assert_eq!(d.bucket, Some(Bucket::QuotaUntil(1000)));
        assert!(d.sink);
        assert!(d.consumes_retry);
    }

    #[test]
    fn rate_limited_with_remaining_not_sunk() {
        let mut s = sig(
            429,
            r#"{"error":{"type":"rate_limit_exceeded","remaining":17}}"#,
        );
        s.remaining_positive = true;
        let d = classify(&s, 0);
        assert_eq!(d.bucket, Some(Bucket::RateFor(30)));
        assert!(!d.sink, "被限速不是没量,不沉底");
    }

    #[test]
    fn bare_429_is_soft_avoidance() {
        let d = classify(&sig(429, "upstream says no"), 0);
        assert_eq!(d.bucket, Some(Bucket::RateFor(5)));
        assert!(!d.sink);
    }

    #[test]
    fn sampled_window_exhausted_429_sinks_until_reset() {
        // 被动采样:x-codex used-percent ≥100,无 quota 词表命中
        //(§8.2 窗口语义进分类)。
        let mut s = sig(429, "{\"error\":\"usage limit\"}");
        s.window_exhausted = true;
        s.reset_at = Some(2000);
        let d = classify(&s, 0);
        assert_eq!(d.bucket, Some(Bucket::QuotaUntil(2000)));
        assert!(d.sink);
    }

    #[test]
    fn chinese_vendor_429_wording_detected() {
        let d = classify(&sig(429, "请求频率过高,请稍后再试"), 0);
        assert!(matches!(d.bucket, Some(Bucket::RateFor(_))));
    }

    #[test]
    fn credit_ladder_bucket() {
        let d = classify(&sig(402, "insufficient credit"), 0);
        assert_eq!(d.bucket, Some(Bucket::Credit));
        assert!(d.sink);
    }

    #[test]
    fn overloaded_529_free_retry() {
        let d = classify(&sig(529, "overloaded"), 0);
        assert_eq!(d.bucket, Some(Bucket::Overload));
        assert!(!d.consumes_retry, "529 不消耗重试预算");
        assert!(!d.disable);
    }

    #[test]
    fn auth_401_triggers_refresh() {
        let d = classify(&sig(401, "token expired"), 0);
        assert!(d.refresh);
        assert!(d.bucket.is_none());
    }

    #[test]
    fn forbidden_requires_explicit_account_evidence_to_disable() {
        let mut signal = sig(403, "account has been suspended");
        assert!(!classify(&signal, 0).disable);
        signal.account_restricted = true;
        assert!(classify(&signal, 0).disable);
        signal.model_scoped = true;
        assert!(!classify(&signal, 0).disable);
    }

    #[test]
    fn server_errors_do_not_rest() {
        for st in [500u16, 502, 503, 504, 408] {
            let d = classify(&sig(st, "boom"), 0);
            assert!(d.bucket.is_none(), "status {st} should not rest");
            assert!(d.consumes_retry);
        }
    }

    #[test]
    fn retry_after_capped_at_30min() {
        let mut s = sig(429, "rate limit");
        s.retry_after = Some(7200);
        let d = classify(&s, 0);
        assert_eq!(d.bucket, Some(Bucket::RateFor(1800)));
    }
}

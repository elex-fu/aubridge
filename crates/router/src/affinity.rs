//! 会话亲和(设计文档 §8.4,v1.1 收窄:仅订阅源)。
//!
//! 纯函数:gateway 提取请求头与请求体,这里推导亲和 key、判定粘性
//! 与候选重排。key 协议感知:agent 自报的会话头 > Anthropic
//! `metadata.user_id` > 首条 user 消息 hash 兜底;Claude Code 子 agent
//! 追加独立后缀,子 agent 的粘性不覆盖主会话(magpie 语义)。
//! 粘性看数据不猜:回复 usage 的 cache_read + cache_write ≥ 1024
//! token 才值得粘;冷窗内有效(Anthropic 1h / 常规 5min)。

use manifest::schema::{ProviderKind, WireProtocol};
use manifest::state::ProviderState;
use proto::protocol::MessagesRequest;

/// 值得粘住的 cache 量下限(token;§8.4:小 cache 粘住只剩延迟劣势)。
pub const CACHE_WORTH: u64 = 1024;
/// 粘性最长保留 24h(§8.4 stickKeep;过期整体丢弃)。
pub const STICK_KEEP_SECS: u64 = 24 * 60 * 60;
/// 亲和表条目上限(§8.4 sticksKept;满时淘汰最旧)。
pub const STICKS_KEPT: usize = 512;

/// agent 自报会话头(优先级即列表序;OpenCode、Codex、Claude Code
/// 等客户端本来就给会话命名——原生会话头,magpie 同名语义)。
const SESSION_HEADERS: &[&str] = &[
    "x-opencode-session",
    "x-session-affinity",
    "x-session-id",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
    "x-conversation-id",
    "x-deepseek-harness-session-id",
];

/// 头值截断上限(超长头不是会话标识,防污染亲和表)。
const HEADER_MAX: usize = 128;

/// 冷窗(§8.4):Anthropic 协议源 prompt cache TTL 1h,常规 5min。
/// 过冷即失效——缓存大概率已掉,粘性只剩延迟劣势。
pub fn cold_window_secs(protocol: WireProtocol) -> u64 {
    match protocol {
        WireProtocol::Anthropic => 3600,
        _ => 300,
    }
}

/// 亲和表里的一条粘性(gateway 持有状态,这里只定义形状与决策)。
/// Serialize 供 M2 持久化(ledger 存 json,§8.4 write-behind)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Stick {
    /// 建立粘性的源(缓存所在;替补答话不改写它——backToMain,
    /// 主力恢复后交回)。
    pub main: String,
    /// 最近答话的源(main 不可用时可能是替补)。
    pub last: String,
    /// 最近答话时刻(unix 秒)。
    pub at: u64,
    /// 该次回复的 cache_read + cache_write(token)。
    pub cache: u64,
}

/// 推导亲和 key。`header` 为客户端请求头查找(gateway 传入);无任何
/// 可用标识返回 None(不参与亲和)。
pub fn session_key(
    req: &MessagesRequest,
    header: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // 1) 原生会话头。
    let mut id: Option<String> = None;
    for h in SESSION_HEADERS {
        let v = header(h)
            .map(|v| v.trim().chars().take(HEADER_MAX).collect::<String>())
            .filter(|v| !v.is_empty());
        if let Some(v) = v {
            id = Some(v);
            break;
        }
    }
    // 2) Anthropic metadata.user_id(Claude Code 稳定携带)。
    let id = match id {
        Some(h) => h,
        None => req
            .metadata
            .as_ref()
            .and_then(|m| m.user_id.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_default(),
    };
    // 3) 兜底:首条 user 消息内容 hash(会话累积时首条稳定不变)。
    let id = if id.is_empty() {
        first_user_fingerprint(req)?
    } else {
        id
    };
    // Claude Code 子 agent:同一会话标识下追加独立后缀,子 agent 的
    // 粘性不覆盖主会话(magpie subagentOf)。
    let sub = header("x-claude-code-agent-id")
        .map(|v| v.trim().chars().take(HEADER_MAX).collect::<String>())
        .filter(|v| !v.is_empty());
    Some(match sub {
        Some(sub) => format!("{id}|agent:{sub}"),
        None => id,
    })
}

/// 首条 user 消息指纹:内容序列化字节的 FNV-1a 64。内联实现,不为此
/// 引入散列依赖——亲和 key 只需跨进程/版本稳定的确定性散列,不需要
/// 密码学强度(无碰撞放大面:一次错粘只损失一个冷窗)。
fn first_user_fingerprint(req: &MessagesRequest) -> Option<String> {
    let msg = req.messages.iter().find(|m| m.role == "user")?;
    let bytes = serde_json::to_vec(&msg.content).unwrap_or_default();
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in &bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Some(format!("msg-{h:016x}"))
}

/// 粘性是否仍在冷窗内。
pub fn within_cold_window(entry: &Stick, protocol: WireProtocol, now: u64) -> bool {
    now.saturating_sub(entry.at) <= cold_window_secs(protocol)
}

/// 回复后的粘性合并(§8.4):cache < 1024 不值一粘(返回 None);
/// 已有粘性 → 保留 main(替补答话不改写,backToMain),更新
/// last/时刻/量;新粘 → main = last = provider。
pub fn fold(existing: Option<Stick>, provider: &str, cache: u64, now: u64) -> Option<Stick> {
    if cache < CACHE_WORTH {
        return None;
    }
    Some(match existing {
        Some(mut e) => {
            e.last = provider.to_string();
            e.at = now;
            e.cache = cache;
            e
        }
        None => Stick {
            main: provider.to_string(),
            last: provider.to_string(),
            at: now,
            cache,
        },
    })
}

/// 候选重排(§8.4 决策管线第 3 步):亲和源(main 优先于 last,
/// backToMain)仍在冷窗内、且是正常候选(非 rest / 非 sink / 熔断
/// 未 open / 订阅源)时提到最前。亲和源休息或不在候选中 → 原序
/// (亲和源 rest 时立即放行迁移;表在源恢复后由回复自然回流)。
pub fn apply_affinity<T>(
    candidates: &mut [T],
    states: &[ProviderState],
    entry: &Stick,
    protocol: WireProtocol,
    now: u64,
    provider_id: impl Fn(&T) -> &str,
) {
    if !within_cold_window(entry, protocol, now) {
        return;
    }
    // 亲和不抢救休息中的源:只认正常候选段的订阅源。
    let eligible = |id: &str| {
        states.iter().any(|s| {
            s.id == id
                && s.enabled
                && s.healthy
                && s.kind == ProviderKind::Subscription
                && !s.is_resting(now)
                && !s.sunk
                && !s.is_breaker_open(now)
        })
    };
    for preferred in [&entry.main, &entry.last] {
        if eligible(preferred) {
            if let Some(pos) = candidates
                .iter()
                .position(|item| provider_id(item) == preferred)
            {
                candidates[..=pos].rotate_right(1);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::protocol::{InboundMessage, RequestMetadata};

    fn apply_affinity(
        mut candidates: Vec<String>,
        states: &[ProviderState],
        entry: &Stick,
        protocol: WireProtocol,
        now: u64,
    ) -> Vec<String> {
        super::apply_affinity(
            &mut candidates,
            states,
            entry,
            protocol,
            now,
            String::as_str,
        );
        candidates
    }

    fn msg(role: &str, text: &str) -> InboundMessage {
        InboundMessage {
            role: role.into(),
            content: proto::protocol::InboundContent::Text(text.into()),
        }
    }

    fn req(messages: Vec<InboundMessage>) -> MessagesRequest {
        MessagesRequest {
            model: "m".into(),
            messages,
            system: None,
            max_tokens: 100,
            tools: None,
            tool_choice: None,
            stream: None,
            metadata: None,
            thinking: None,
            output_config: None,
        }
    }

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn session_header_wins_over_metadata_and_hash() {
        let mut r = req(vec![msg("user", "你好")]);
        r.metadata = Some(RequestMetadata {
            user_id: Some("user-abc".into()),
        });
        let key = session_key(&r, headers(&[("x-claude-code-session-id", "sess-1")])).unwrap();
        assert_eq!(key, "sess-1");
    }

    #[test]
    fn metadata_user_id_is_second_choice() {
        let mut r = req(vec![msg("user", "你好")]);
        r.metadata = Some(RequestMetadata {
            user_id: Some("user-abc".into()),
        });
        let key = session_key(&r, headers(&[])).unwrap();
        assert_eq!(key, "user-abc");
    }

    #[test]
    fn first_user_message_hash_is_fallback() {
        // 无头无 metadata:同一首条 user 消息 → 同 key;不同 → 不同 key。
        let a = session_key(
            &req(vec![msg("user", "第一句"), msg("assistant", "答")]),
            headers(&[]),
        );
        let b = session_key(
            &req(vec![
                msg("user", "第一句"),
                msg("assistant", "答"),
                msg("user", "追问"),
            ]),
            headers(&[]),
        );
        assert_eq!(a, b);
        let c = session_key(&req(vec![msg("user", "另一会话")]), headers(&[]));
        assert_ne!(a, c);
    }

    #[test]
    fn no_identity_at_all_is_none() {
        assert!(session_key(&req(Vec::new()), headers(&[])).is_none());
    }

    #[test]
    fn subagent_gets_independent_suffix() {
        let r = req(vec![msg("user", "你好")]);
        // 子 agent:同会话头 + agent 头 → 独立 key(不覆盖主会话粘性)。
        let main = session_key(&r, headers(&[("x-claude-code-session-id", "sess-1")])).unwrap();
        let sub = session_key(
            &r,
            headers(&[
                ("x-claude-code-session-id", "sess-1"),
                ("x-claude-code-agent-id", "agent-7"),
                ("x-claude-code-parent-agent-id", "agent-1"),
            ]),
        )
        .unwrap();
        assert_eq!(sub, format!("{main}|agent:agent-7"));
    }

    #[test]
    fn header_values_are_trimmed_and_bounded() {
        let r = req(Vec::new());
        let key = session_key(&r, headers(&[("x-session-id", "  abc  ")])).unwrap();
        assert_eq!(key, "abc");
        let long = "x".repeat(300);
        let key = session_key(&r, headers(&[("x-session-id", &long)])).unwrap();
        assert_eq!(key.chars().count(), 128);
    }

    #[test]
    fn cold_window_differs_by_protocol() {
        assert_eq!(cold_window_secs(WireProtocol::Anthropic), 3600);
        assert_eq!(cold_window_secs(WireProtocol::Chat), 300);
        assert_eq!(cold_window_secs(WireProtocol::Responses), 300);
    }

    #[test]
    fn fold_below_threshold_never_sticks() {
        assert!(fold(None, "kimi", 1023, 100).is_none());
    }

    #[test]
    fn fold_new_stick_sets_main_and_last() {
        let e = fold(None, "kimi", 4096, 100).unwrap();
        assert_eq!(e.main, "kimi");
        assert_eq!(e.last, "kimi");
        assert_eq!(e.at, 100);
    }

    #[test]
    fn fold_substitute_keeps_main() {
        let e = fold(None, "kimi", 4096, 100).unwrap();
        // 替补答话:main 不改写(backToMain),last/时刻更新。
        let e = fold(Some(e), "chatgpt", 2048, 200).unwrap();
        assert_eq!(e.main, "kimi");
        assert_eq!(e.last, "chatgpt");
        assert_eq!(e.at, 200);
    }

    #[test]
    fn apply_affinity_promotes_sticky_source_to_front() {
        let sub = ProviderState::new("kimi", ProviderKind::Subscription, vec!["m".into()]);
        let free = ProviderState::new("free", ProviderKind::FreeTier, vec!["m".into()]);
        let entry = Stick {
            main: "kimi".into(),
            last: "kimi".into(),
            at: 100,
            cache: 4096,
        };
        let ids = apply_affinity(
            vec!["free".into(), "kimi".into()],
            &[free, sub],
            &entry,
            WireProtocol::Anthropic,
            200,
        );
        // 冷窗内:亲和源越过免费源提最前(cache 命中比免费档延迟值)。
        assert_eq!(ids, vec!["kimi", "free"]);
    }

    #[test]
    fn apply_affinity_prefers_main_over_last() {
        let main = ProviderState::new("kimi", ProviderKind::Subscription, vec!["m".into()]);
        let last = ProviderState::new("chatgpt", ProviderKind::Subscription, vec!["m".into()]);
        let entry = Stick {
            main: "kimi".into(),
            last: "chatgpt".into(),
            at: 100,
            cache: 4096,
        };
        let ids = apply_affinity(
            vec!["chatgpt".into(), "kimi".into()],
            &[last, main],
            &entry,
            WireProtocol::Anthropic,
            200,
        );
        // backToMain:主力恢复后交回。
        assert_eq!(ids, vec!["kimi", "chatgpt"]);
    }

    #[test]
    fn apply_affinity_falls_back_to_last_when_main_resting() {
        let mut main = ProviderState::new("kimi", ProviderKind::Subscription, vec!["m".into()]);
        main.rest_until = Some(1000);
        let last = ProviderState::new("chatgpt", ProviderKind::Subscription, vec!["m".into()]);
        let entry = Stick {
            main: "kimi".into(),
            last: "chatgpt".into(),
            at: 100,
            cache: 4096,
        };
        let ids = apply_affinity(
            vec!["chatgpt".into(), "kimi".into()],
            &[main, last],
            &entry,
            WireProtocol::Anthropic,
            200,
        );
        // 亲和源 rest 时立即放行迁移:main 不抢救,替补提到最前。
        assert_eq!(ids, vec!["chatgpt", "kimi"]);
    }

    #[test]
    fn apply_affinity_expired_cold_window_is_passthrough() {
        let sub = ProviderState::new("kimi", ProviderKind::Subscription, vec!["m".into()]);
        let free = ProviderState::new("free", ProviderKind::FreeTier, vec!["m".into()]);
        let entry = Stick {
            main: "kimi".into(),
            last: "kimi".into(),
            at: 0,
            cache: 4096,
        };
        // 常规冷窗 5min:at=0,now=3601 已过冷 → 原序。
        let ids = apply_affinity(
            vec!["free".into(), "kimi".into()],
            &[free, sub],
            &entry,
            WireProtocol::Chat,
            3601,
        );
        assert_eq!(ids, vec!["free", "kimi"]);
    }

    #[test]
    fn apply_affinity_free_tier_never_promoted() {
        // 免费源即使被记进表(调用方失误)也不重排:仅订阅源亲和。
        let free = ProviderState::new("free", ProviderKind::FreeTier, vec!["m".into()]);
        let entry = Stick {
            main: "free".into(),
            last: "free".into(),
            at: 100,
            cache: 4096,
        };
        let ids = apply_affinity(
            vec!["free".into()],
            &[free],
            &entry,
            WireProtocol::Chat,
            200,
        );
        assert_eq!(ids, vec!["free"]);
    }
}

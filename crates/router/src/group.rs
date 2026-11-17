//! 组合模型 `group/a+b`(变更记录 31 批 2 ④)。
//!
//! 请求内联跨模型兜底:成员在请求里显式列出,零配置零状态。语义
//! 免费优先横跨成员，保留同一梯队内的成员偏好；
//! `free/<model>` 与 group 不叠加——成员里再写
//! free/ 前缀按字面模型处理(不触发免费池语义)。命名分组
//! (ui-design §6.3,变更记录 32-③)复用同一成员匹配语义,规则由
//! `GroupRule` 枚举决定。明确不做:嵌套 group、跨 provider 账号
//! spreading(§3 反目标邻域)。

use std::collections::BTreeSet;

use manifest::{schema::ProviderKind, state::ProviderState};

/// 组语法前缀。
const PREFIX: &str = "group/";

/// 命名分组路由规则(ui-design §6.3,变更记录 32-③):固定枚举,
/// 只选择——没有规则编辑器、没有嵌套(magpie 反面教训)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRule {
    /// 免费优先(默认):所有健康免费源排在订阅前。
    FreeFirst,
    /// 仅免费档：在同组免费模型间切换。
    FreeOnly,
    /// 顺序兜底:严格按声明序(成员内快照序,不做免费优先重排),
    /// 首个答不上换下一个。
    Sequential,
    /// 轮流:每轮换下一个——整表按请求计数
    /// 轮转(成员间/成员内一起轮,尽力语义,不持久)。
    RoundRobin,
}

impl GroupRule {
    /// 解析规则字符串(settings store 存字符串):未知值 None,
    /// 调用方报错(枚举只选择,不猜测)。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "free_first" => Some(Self::FreeFirst),
            "free_only" => Some(Self::FreeOnly),
            "sequential" => Some(Self::Sequential),
            "round_robin" => Some(Self::RoundRobin),
            _ => None,
        }
    }

    /// 序列化名(settings/UI 线格式)。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FreeFirst => "free_first",
            Self::FreeOnly => "free_only",
            Self::Sequential => "sequential",
            Self::RoundRobin => "round_robin",
        }
    }

    pub fn allows_provider(self, kind: ProviderKind) -> bool {
        self != Self::FreeOnly || kind == ProviderKind::FreeTier
    }
}

pub type ModelCandidate<'a> = (&'a ProviderState, &'a str);

/// 内联组与命名组都按源和模型组成候选。
pub fn candidates_named_group<'a>(
    members: &'a [String],
    rule: GroupRule,
    states: &'a [ProviderState],
    now: u64,
    rotation: u64,
) -> Vec<ModelCandidate<'a>> {
    let mut candidates = match rule {
        GroupRule::FreeFirst | GroupRule::FreeOnly => candidates_group(members, states, now),
        GroupRule::RoundRobin => members_in_order(members, states, now),
        GroupRule::Sequential => collect_members(members, |member| {
            states
                .iter()
                .filter(|source| {
                    source.enabled
                        && source.healthy
                        && !source.is_breaker_open(now)
                        && !source.is_resting(now)
                        && !source.sunk
                        && source.remaining != Some(0)
                        && source.model_cooldown(member, now).is_none()
                        && source.models.iter().any(|model| model == member)
                })
                .collect()
        }),
    };
    if rule == GroupRule::FreeOnly {
        candidates.retain(|(source, _)| rule.allows_provider(source.kind));
    }
    if rule == GroupRule::RoundRobin && !candidates.is_empty() {
        let offset = (rotation as usize) % candidates.len();
        candidates.rotate_left(offset);
    }
    candidates
}

/// 解析组语法(纯函数):None = 普通模型;Some(成员) = 组请求。
/// `+` 的 URL 转义形式 `%2B` 等价(agent 配置文件转义场景);空
/// 成员忽略,全空 = 组请求但零成员(候选为空,网关按无候选处理)。
pub fn parse_members(model: &str) -> Option<Vec<String>> {
    // 含 % 才归一化(普通模型零分配)。
    let normalized = if model.contains('%') {
        model.replace("%2F", "/").replace("%2B", "+")
    } else {
        model.to_string()
    };
    let rest = normalized.strip_prefix(PREFIX)?;
    Some(
        rest.split('+')
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// 组请求按字面模型匹配，同一梯队保留成员顺序。
pub fn candidates_group<'a>(
    members: &'a [String],
    states: &'a [ProviderState],
    now: u64,
) -> Vec<ModelCandidate<'a>> {
    let mut candidates = members_in_order(members, states, now);
    prioritize_free(&mut candidates, now);
    candidates
}

pub fn prioritize_free(candidates: &mut [ModelCandidate<'_>], now: u64) {
    candidates.sort_by_key(|(source, _)| {
        source.kind != ProviderKind::FreeTier
            || source.is_resting(now)
            || source.sunk
            || source.remaining == Some(0)
    });
}

fn members_in_order<'a>(
    members: &'a [String],
    states: &'a [ProviderState],
    now: u64,
) -> Vec<ModelCandidate<'a>> {
    collect_members(members, |member| {
        super::plan::candidates_literal(member, states, now)
    })
}

fn collect_members<'a>(
    members: &'a [String],
    sources: impl Fn(&str) -> Vec<&'a ProviderState>,
) -> Vec<ModelCandidate<'a>> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for member in members {
        for source in sources(member) {
            if seen.insert((source.id.as_str(), member.as_str())) {
                candidates.push((source, member.as_str()));
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::schema::ProviderKind;

    fn pairs(candidates: Vec<ModelCandidate<'_>>) -> Vec<(&str, &str)> {
        candidates
            .into_iter()
            .map(|(source, model)| (source.id.as_str(), model))
            .collect()
    }

    fn free(id: &str, models: &[&str]) -> ProviderState {
        ProviderState::new(
            id,
            ProviderKind::FreeTier,
            models.iter().map(|m| m.to_string()).collect(),
        )
    }

    #[test]
    fn parse_plain_model_is_not_group() {
        assert_eq!(parse_members("glm-4.5-flash"), None);
        // free/ 组合模型是既有独立语义,不是 group。
        assert_eq!(parse_members("free/glm-4.5-flash"), None);
    }

    #[test]
    fn parse_group_members_in_declaration_order() {
        let m = parse_members("group/a+b").unwrap();
        assert_eq!(m, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_url_escaped_equivalents() {
        assert_eq!(parse_members("group/a%2Bb").unwrap(), vec!["a", "b"]);
        assert_eq!(parse_members("group%2Fa+b").unwrap(), vec!["a", "b"]);
        // 空成员忽略("group/a+" 与 "group//a" 同义)。
        assert_eq!(parse_members("group/a+").unwrap(), vec!["a"]);
        // 全空 = 零成员组(候选为空)。
        assert!(parse_members("group/").unwrap().is_empty());
    }

    #[test]
    fn group_candidates_concatenate_in_declaration_order() {
        // a 只 m1 供、b 只 m2 供:成员内排序无关,声明序拼接。
        let states = vec![free("src-m2", &["m2"]), free("src-m1", &["m1"])];
        let members: Vec<String> = vec!["m1".into(), "m2".into()];
        assert_eq!(
            pairs(candidates_group(&members, &states, 0)),
            vec![("src-m1", "m1"), ("src-m2", "m2")]
        );
        // 声明序 = 偏好序:反过来写,顺序跟着反。
        let members: Vec<String> = vec!["m2".into(), "m1".into()];
        assert_eq!(
            pairs(candidates_group(&members, &states, 0)),
            vec![("src-m2", "m2"), ("src-m1", "m1")]
        );
    }

    #[test]
    fn source_keeps_distinct_models_and_deduplicates_repeated_member() {
        let states = vec![free("both", &["m1", "m2"]), free("only-m2", &["m2"])];
        let members: Vec<String> = vec!["m1".into(), "m2".into(), "m2".into()];
        assert_eq!(
            pairs(candidates_group(&members, &states, 0)),
            vec![("both", "m1"), ("both", "m2"), ("only-m2", "m2")]
        );
    }

    #[test]
    fn group_member_matching_is_literal_no_free_prefix_semantics() {
        // 成员 "free/x" 按字面模型处理:不触发免费池语义,匹配
        // models 里字面含 "free/x" 的源(现实里几乎必空 = 不支持)。
        let states = vec![free("src", &["x"])];
        let members: Vec<String> = vec!["free/x".into()];
        assert!(candidates_group(&members, &states, 0).is_empty());
    }

    #[test]
    fn rule_parse_and_str_roundtrip() {
        for r in [
            GroupRule::FreeFirst,
            GroupRule::FreeOnly,
            GroupRule::Sequential,
            GroupRule::RoundRobin,
        ] {
            assert_eq!(GroupRule::parse(r.as_str()), Some(r));
        }
        assert_eq!(GroupRule::parse("intent"), None, "未知规则拒绝");
        assert_eq!(GroupRule::parse(""), None);
    }

    #[test]
    fn named_group_free_only_filters_subscription_and_keeps_free_order() {
        let mut plan = free("plan", &["a", "b"]);
        plan.kind = ProviderKind::Subscription;
        let states = [plan, free("free-a", &["a"]), free("free-b", &["b"])];
        let members = ["b".to_string(), "a".to_string()];
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::FreeOnly,
                &states,
                0,
                0
            )),
            vec![("free-b", "b"), ("free-a", "a")]
        );
    }

    #[test]
    fn named_group_sequential_keeps_declaration_and_snapshot_order() {
        // b 成员在前声明;免费优先会重排(剩余量/梯队),顺序兜底
        // 严格按声明序,成员内按快照序。
        let mut big = free("src-big", &["ma"]);
        big.remaining = Some(100);
        let mut small = free("src-small", &["ma", "mb"]);
        small.remaining = Some(1);
        let states = vec![small, big];
        let members: Vec<String> = vec!["mb".to_string(), "ma".to_string()];

        // 免费优先:mb 的 src-small 先(唯一供 mb),ma 段两源
        // 按免费优先(剩余量 desc)。
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::FreeFirst,
                &states,
                0,
                0
            )),
            vec![("src-small", "mb"), ("src-big", "ma"), ("src-small", "ma")]
        );
        // 顺序兜底保留同一成员内的源顺序。
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::Sequential,
                &states,
                0,
                0
            )),
            vec![("src-small", "mb"), ("src-small", "ma"), ("src-big", "ma")]
        );
        // 成员声明序反转 → 顺序兜底跟着反转(不重排)。
        let members: Vec<String> = vec!["ma".to_string(), "mb".to_string()];
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::Sequential,
                &states,
                0,
                0
            )),
            vec![("src-small", "ma"), ("src-big", "ma"), ("src-small", "mb")]
        );
    }

    #[test]
    fn sequential_skips_unhealthy_and_appends_no_sink() {
        let mut dead = free("dead", &["ma"]);
        dead.healthy = false;
        let mut resting = free("resting", &["ma"]);
        resting.rest_until = Some(500);
        let ok = free("ok", &["ma"]);
        let states = vec![dead, resting, ok];
        let members: Vec<String> = vec!["ma".to_string()];
        // 顺序兜底不追加 rest/sink 兜底段("照我的清单来")。
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::Sequential,
                &states,
                0,
                0
            )),
            vec![("ok", "ma")]
        );
    }

    #[test]
    fn round_robin_rotates_by_request_counter() {
        let states = vec![free("a", &["ma"]), free("b", &["ma"]), free("c", &["ma"])];
        let members: Vec<String> = vec!["ma".to_string()];
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::RoundRobin,
                &states,
                0,
                0
            )),
            vec![("a", "ma"), ("b", "ma"), ("c", "ma")]
        );
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::RoundRobin,
                &states,
                0,
                1
            )),
            vec![("b", "ma"), ("c", "ma"), ("a", "ma")]
        );
        // rotation 超长取模。
        assert_eq!(
            pairs(candidates_named_group(
                &members,
                GroupRule::RoundRobin,
                &states,
                0,
                4
            )),
            vec![("b", "ma"), ("c", "ma"), ("a", "ma")]
        );
    }
}

//! 候选循环:免费优先两梯队排序(设计文档 §8 决策管线)。
//!
//! 第 1 梯队 = 免费池供应此模型、未休息、余量 > 0(或未知)的源,
//! 按 (剩余量 desc, 重置时间近的先) 排序;第 2 梯队 = 订阅源。
//! rest 中的源排末位,且只保留 rest 结束最早的一个(`probeOne`,
//! magpie:避免逐个唤醒)。sink 的源沉到最末兜底。

use manifest::schema::ProviderKind;
use manifest::state::ProviderState;

/// 生成本请求的候选序列(源 ID 有序)。纯函数。
pub fn candidates(model: &str, states: &[ProviderState], now: u64) -> Vec<String> {
    let literal = model.strip_prefix("free/").unwrap_or(model);
    ids(candidates_by(states, now, |s| {
        s.supplies(model) && s.model_cooldown(literal, now).is_none()
    }))
}

/// 字面模型匹配的候选(组请求成员用,变更记录 31 ④:free/ 前缀
/// 在 group 成员里不叠加,按字面模型处理)。
pub(crate) fn candidates_literal<'a>(
    model: &str,
    states: &'a [ProviderState],
    now: u64,
) -> Vec<&'a ProviderState> {
    candidates_by(states, now, |s| {
        s.model_cooldown(model, now).is_none() && s.models.iter().any(|m| m == model)
    })
}

/// 自动虚拟模型候选(ui-design §6.1,变更记录 32-②):不按模型
/// 过滤,全部健康启用源进两梯队(免费优先 + 订阅兜底);网关侧
/// 按候选源把请求 model 改写成其首个声明模型。
pub fn candidates_auto(states: &[ProviderState], now: u64) -> Vec<String> {
    ids(candidates_by(states, now, |s| {
        s.models
            .iter()
            .any(|model| s.model_cooldown(model, now).is_none())
    }))
}

fn ids(candidates: Vec<&ProviderState>) -> Vec<String> {
    candidates
        .into_iter()
        .map(|source| source.id.clone())
        .collect()
}

fn candidates_by(
    states: &[ProviderState],
    now: u64,
    supplies: impl Fn(&ProviderState) -> bool,
) -> Vec<&ProviderState> {
    let mut tier1: Vec<&ProviderState> = Vec::new();
    let mut tier2: Vec<&ProviderState> = Vec::new();

    for s in states {
        if !s.enabled || !s.healthy || !supplies(s) {
            continue;
        }
        // 熔断 open 期排除出候选;open_until 已过 = half-open,放行
        // 试探(§8.1:半开失败重开,2 成恢复)。
        if s.is_breaker_open(now) {
            continue;
        }
        // 余量已知为 0 的免费源不进第一梯队(订阅源不受此限:
        // remaining 对订阅语义不同,由 usage 窗口另行表达)。
        let exhausted = s.kind == ProviderKind::FreeTier && s.remaining == Some(0);
        if s.is_resting(now) || s.sunk || exhausted {
            continue;
        }
        match s.kind {
            ProviderKind::FreeTier => tier1.push(s),
            ProviderKind::Subscription => tier2.push(s),
        }
    }

    // 第 1 梯队排序:(剩余量 desc, 重置时间近的先)。
    tier1.sort_by(|a, b| {
        let ra = a.remaining.unwrap_or(u64::MAX);
        let rb = b.remaining.unwrap_or(u64::MAX);
        rb.cmp(&ra).then_with(|| {
            a.resets_at
                .unwrap_or(u64::MAX)
                .cmp(&b.resets_at.unwrap_or(u64::MAX))
        })
    });
    // 第 2 梯队:按用户排序(快照顺序即用户排序)。

    let mut candidates: Vec<_> = tier1.into_iter().chain(tier2).collect();

    // probeOne:rest 中的源只探"结束最早的那一个",排末位
    //(熔断 open 的不探——连 5xx 都在打,不差这一发;sunk 的不探
    // ——sink 兜底段已收它,重复追加会让网关对同一源连试两次)。
    let resting: Vec<&ProviderState> = states
        .iter()
        .filter(|s| {
            s.enabled
                && s.healthy
                && supplies(s)
                && s.is_resting(now)
                && !s.sunk
                && !s.is_breaker_open(now)
        })
        .collect();
    if let Some(earliest) = resting
        .iter()
        .min_by_key(|s| s.rest_until.unwrap_or(u64::MAX))
    {
        candidates.push(earliest);
    }

    // sink 的源沉到最末兜底(真没量,重置后由 registry 复位;
    // 熔断 open 的仍排除——兜底也打不到挂掉的源)。
    for s in states
        .iter()
        .filter(|s| s.sunk && supplies(s) && !s.is_breaker_open(now))
    {
        candidates.push(s);
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(id: &str) -> ProviderState {
        ProviderState::new(id, ProviderKind::FreeTier, vec!["glm-4.5-flash".into()])
    }
    fn sub(id: &str) -> ProviderState {
        ProviderState::new(id, ProviderKind::Subscription, vec!["glm-4.5-flash".into()])
    }

    #[test]
    fn free_first_subscription_fallback() {
        let states = vec![sub("kimi-plan"), free("openrouter"), free("modelscope")];
        let ids = candidates("glm-4.5-flash", &states, 0);
        // 免费在前(快照序),订阅兜底。
        assert_eq!(ids, vec!["openrouter", "modelscope", "kimi-plan"]);
    }

    #[test]
    fn remaining_desc_then_resets_sooner() {
        let mut a = free("openrouter");
        a.remaining = Some(10);
        a.resets_at = Some(1000);
        let mut b = free("modelscope");
        b.remaining = Some(50);
        b.resets_at = Some(2000);
        let mut c = free("zhipu");
        c.remaining = Some(50);
        c.resets_at = Some(900);
        // 同剩余量 50:重置近的(c)先 —— 尽快用掉会重置的。
        assert_eq!(
            candidates("glm-4.5-flash", &[a, b, c.clone()], 0),
            vec!["zhipu", "modelscope", "openrouter"]
        );
    }

    #[test]
    fn resting_sources_probe_one_earliest() {
        let mut a = free("openrouter");
        a.rest_until = Some(500);
        let mut b = free("modelscope");
        b.rest_until = Some(300);
        let c = free("zhipu");
        let ids = candidates("glm-4.5-flash", &[a, b, c], 0);
        // zhipu 正常在前;rest 中只探最早结束的 modelscope。
        assert_eq!(ids, vec!["zhipu", "modelscope"]);
    }

    #[test]
    fn exhausted_free_source_skipped() {
        let mut a = free("openrouter");
        a.remaining = Some(0);
        let c = free("zhipu");
        assert_eq!(candidates("glm-4.5-flash", &[a, c], 0), vec!["zhipu"]);
    }

    #[test]
    fn sunk_source_sinks_to_bottom() {
        let mut a = free("openrouter");
        a.sunk = true;
        let s = sub("kimi-plan");
        // sink 排在订阅兜底之后(兜底再试)。
        assert_eq!(
            candidates("glm-4.5-flash", &[a, s], 0),
            vec!["kimi-plan", "openrouter"]
        );
    }

    #[test]
    fn resting_and_sunk_source_appended_once() {
        // rest + sink 叠加:probeOne 与 sink 兜底曾各追加一次(重复
        // 出现在候选列表,网关会对同一源连试两次)。sink 语义更"沉",
        // 该源只以 sink 兜底身份出现一次;probeOne 只探未沉底的。
        let mut a = free("openrouter");
        a.rest_until = Some(300);
        a.sunk = true;
        let mut b = free("modelscope");
        b.rest_until = Some(500);
        let c = free("zhipu");
        // probeOne 探的是未 sunk 的 modelscope;openrouter 走 sink 兜底。
        assert_eq!(
            candidates("glm-4.5-flash", &[a, b, c], 0),
            vec!["zhipu", "modelscope", "openrouter"]
        );
    }

    #[test]
    fn free_group_model_matches_free_tier_only() {
        let states = vec![sub("kimi-plan"), free("openrouter")];
        // free/<model> 组合模型:订阅源不参与。
        assert_eq!(
            candidates("free/glm-4.5-flash", &states, 0),
            vec!["openrouter"]
        );
        // 直接 id:两者都供应。
        assert_eq!(candidates("glm-4.5-flash", &states, 0).len(), 2);
    }

    #[test]
    fn auto_candidates_ignore_model_filter_two_tiers_kept() {
        // 各源供不同模型:自动虚拟模型不按模型过滤,全部健康源
        // 进两梯队(免费在前,订阅兜底);不供任何"请求模型"无关。
        let mut m = free("modelscope");
        m.models = vec!["Qwen/…".into()];
        let states = vec![sub("kimi-plan"), free("openrouter"), m];
        assert_eq!(
            candidates_auto(&states, 0),
            vec!["openrouter", "modelscope", "kimi-plan"]
        );
        // 两梯队与探针/沉底语义与普通模型一致:rest 只探最早结束的。
        let mut r = free("zhipu");
        r.rest_until = Some(500);
        let ok = free("openrouter");
        assert_eq!(candidates_auto(&[r, ok], 0), vec!["openrouter", "zhipu"]);
    }

    #[test]
    fn disabled_and_unhealthy_excluded() {
        let mut a = free("openrouter");
        a.enabled = false;
        let mut b = free("modelscope");
        b.healthy = false;
        assert!(candidates("glm-4.5-flash", &[a, b], 0).is_empty());
    }

    #[test]
    fn breaker_open_source_excluded_everywhere() {
        let mut a = free("openrouter");
        a.sunk = true; // 即使 sink 兜底,熔断 open 也排除。
        a.rest_until = Some(500); // probeOne 同样不探。
        a.breaker.open_until = Some(10_000);
        let c = free("zhipu");
        assert_eq!(
            candidates("glm-4.5-flash", &[a.clone(), c.clone()], 0),
            vec!["zhipu"]
        );

        // open_until 已过 = half-open:放行试探(排序键相同,快照序)。
        a.breaker.open_until = Some(1);
        a.rest_until = None;
        a.sunk = false;
        assert_eq!(
            candidates("glm-4.5-flash", &[a, c], 2),
            vec!["openrouter", "zhipu"]
        );
    }
}

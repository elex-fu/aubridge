//! key plan 配额查询(契约:docs/quota-plan-design-2026-11-15.md §3.2)。
//!
//! 与 usage.rs(claude/wham)同位同纪律:纯解析可测 + 带缓存拉取器
//! (正缓存 180s / 负缓存 60s / 单飞,§8.2 口径)。只做查询与展示,
//! 不做任何池化调度(§3 反目标)。解析语义照抄 magpie planquota.go
//! 踩过的坑:智谱 envelope 限流当读失败、Kimi 三形状并容、数字
//! 字符串/数值双态、reset 文本或 epoch 秒/毫秒。

use crate::quota::{response_text, QuotaCache, QuotaRead};
use manifest::quota::QuotaIssue;
use serde_json::Value;

/// 归一后的 plan 窗口(util_5h/util_7d 字段口径:百分比 0–100,
/// None = 未知不覆盖;reset 取最短已用窗口)。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PlanWindows {
    pub w5h: Option<f64>,
    pub w7d: Option<f64>,
    pub reset_at: Option<u64>,
    pub reset_5h: Option<u64>,
    pub reset_7d: Option<u64>,
    pub span_5h: Option<u64>,
}

/// 端点鉴权形态:智谱把裸 key 放 `Authorization`(非 Bearer,
/// magpie 实测口径);Kimi 走标准 Bearer。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAuth {
    RawKey,
    Bearer,
}

/// host 分发(manifest.provider.base → (端点, 鉴权)):
/// 智谱/Z.ai 用其配额端点;Kimi 用 `{coding base 去尾}/usages`。
/// 其他 host → None(不在 v1 范围,设计 D1)。
pub fn plan_source(base: &str) -> Option<(String, PlanAuth)> {
    let trimmed = base.trim().trim_end_matches('/');
    let host = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or_default();
    match host {
        "open.bigmodel.cn" | "api.z.ai" => Some((
            format!("https://{host}/api/monitor/usage/quota/limit"),
            PlanAuth::RawKey,
        )),
        "api.kimi.com" | "api.kimi.ai" => {
            // coding base:…/coding/v1 或 …/coding → …/coding/v1;
            // 端点 = base 去尾 + /usages(magpie kimiCodeBase 口径)。
            let coding = if trimmed.ends_with("/coding") {
                format!("{trimmed}/v1")
            } else {
                trimmed.to_string()
            };
            Some((format!("{coding}/usages"), PlanAuth::Bearer))
        }
        _ => None,
    }
}

/// 智谱 `/api/monitor/usage/quota/limit` 响应 → 窗口。
/// envelope:success=false / data 缺失 / 限流码(1302/1303 等) /
/// key 被拒(1000–1005)→ None(免费档 key 无 plan,静默)。
/// 窗口映射:span ≤24h 的最短窗口 → w5h;7 天窗口 → w7d;
/// TIME_LIMIT(MCP 月)与月窗口 v1 不展示(字段就两个,设计 D7)。
pub fn parse_zhipu(body: &str) -> Option<PlanWindows> {
    let v: Value = serde_json::from_str(body).ok()?;
    // envelope 错误码(200 也带码的形状)。
    if let Some(code) = v["error"]["code"].as_i64().or_else(|| v["code"].as_i64()) {
        if (1000..=1005).contains(&code) || code == 1302 || code == 1303 {
            return None;
        }
    }
    if v["success"].as_bool() == Some(false) {
        return None;
    }
    let data = v.get("data").filter(|d| d["limits"].is_array())?;
    let mut w5h = None;
    let mut w7d = None;
    let mut short_reset = None;
    let mut short_span = None;
    let mut week_reset = None;
    for limit in data["limits"].as_array()? {
        if limit["type"].as_str() != Some("TOKENS_LIMIT") {
            continue; // TIME_LIMIT = MCP 月工具调用,不停模型
        }
        let unit = limit["unit"].as_i64()?;
        let span_hours = match unit {
            3 => (limit["number"].as_i64().unwrap_or(1)).max(1) as f64, // 小时
            6 => 24.0 * 7.0,                                            // 周
            _ => continue,
        };
        let used = limit["percentage"].as_f64();
        let reset = limit["nextResetTime"]
            .as_i64()
            .filter(|t| *t > 0)
            .map(|t| (t / 1000) as u64); // 毫秒 → 秒
        if span_hours <= 24.0 {
            let span = (span_hours * 3600.0) as u64;
            if short_span.is_none_or(|previous| span <= previous) {
                w5h = used;
                short_reset = reset;
                short_span = Some(span);
            }
        } else {
            w7d = used;
            week_reset = reset;
        }
    }
    if w5h.is_none() && w7d.is_none() {
        return None; // 无 TOKENS_LIMIT 窗口 = 非 plan key
    }
    Some(PlanWindows {
        w5h,
        w7d,
        reset_at: short_reset.or(week_reset),
        reset_5h: short_reset,
        reset_7d: week_reset,
        span_5h: short_span,
    })
}

/// Kimi `{coding base}/usages` 响应 → 窗口(magpie readKimiCode 的
/// 三形状并容):`usage`(周)+ `limits[]`(各 span 窗口)+
/// `usages.limit_*` share(used_ratio 0–1);share 存在时按 span
/// 覆盖前两者的同 span 窗口。数字字符串或数值;reset 为 RFC3339
/// 文本或 epoch 秒/毫秒。
pub fn parse_kimi(body: &str) -> Option<PlanWindows> {
    let v: Value = serde_json::from_str(body).ok()?;

    // 值 → f64(字符串/数值双态)。
    let num = |value: &Value| -> Option<f64> {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
    };
    // reset → unix 秒(多键名;文本 RFC3339 或 epoch 秒/毫秒)。
    let reset_of = |row: &Value| -> Option<u64> {
        for key in ["resetTime", "resetAt", "reset_at", "reset_time"] {
            let Some(value) = row.get(key) else { continue };
            if let Some(text) = value.as_str() {
                if let Some(secs) = parse_rfc3339(text) {
                    return Some(secs);
                }
            }
            if let Some(n) = num(value).filter(|n| *n > 0.0) {
                return Some(if n > 1e11 {
                    (n / 1000.0) as u64
                } else {
                    n as u64
                });
            }
        }
        None
    };
    // limit/used(或 remaining)→ 窗口(used 百分比)。
    let window = |row: &Value| -> Option<(f64, Option<u64>)> {
        let limit = num(&row["limit"])?;
        if limit <= 0.0 {
            return None;
        }
        let used = match num(&row["used"]) {
            Some(used) => used,
            None => limit - num(&row["remaining"])?,
        };
        Some((used / limit * 100.0, reset_of(row)))
    };

    let mut w5h = None;
    let mut w5h_reset = None;
    let mut w7d = None;
    let mut w7d_reset = None;
    // limits[]:window.duration + timeUnit → span。
    if let Some(limits) = v["limits"].as_array() {
        for entry in limits {
            let detail = if entry["detail"].is_object() {
                &entry["detail"]
            } else {
                &entry["window"]
            };
            let Some((used, reset)) = window(detail) else {
                continue;
            };
            let span_secs = kimi_span_secs(&entry["window"]);
            if span_secs == Some(5 * 3600) {
                w5h = Some(used);
                w5h_reset = reset;
            } else if span_secs == Some(7 * 86400)
                || entry["window"]["timeUnit"]
                    .as_str()
                    .is_some_and(|u| u.contains("WEEK"))
            {
                w7d = Some(used);
                w7d_reset = reset;
            }
        }
    }
    // usage 行(周窗口)。
    if let Some((used, reset)) = window(&v["usage"]) {
        if w7d.is_none() {
            w7d = Some(used);
            w7d_reset = reset;
        }
    }
    // usages share(used_ratio 0–1;按 span 覆盖前面读到的窗口)。
    if v["usages"].is_object() {
        for (key, span) in [("limit_5h", 5 * 3600), ("limit_7d", 7 * 86400)] {
            let row = v["usages"].get(key).or_else(|| v["usages"].get(camel(key)));
            let ratio = row.and_then(|r| num(&r["used_ratio"]).or_else(|| num(&r["usedRatio"])));
            if let Some(ratio) = ratio {
                let used = (ratio * 100.0).clamp(0.0, 100.0);
                let reset = row.and_then(reset_of);
                if span == 5 * 3600 {
                    w5h = Some(used);
                    w5h_reset = reset;
                } else {
                    w7d = Some(used);
                    w7d_reset = reset;
                }
            }
        }
    }
    if w5h.is_none() && w7d.is_none() {
        return None;
    }
    let reset_at = match (w5h, w5h_reset) {
        (Some(_), Some(r)) => Some(r),
        _ => w7d_reset,
    };
    Some(PlanWindows {
        w5h,
        w7d,
        reset_at,
        reset_5h: w5h_reset,
        reset_7d: w7d_reset,
        span_5h: w5h.map(|_| 18_000),
    })
}

/// Kimi limits[].window 的 duration+timeUnit → 秒(未知 → None)。
fn kimi_span_secs(window: &Value) -> Option<i64> {
    let n = window["duration"].as_f64()?;
    let unit = window["timeUnit"].as_str()?;
    let per = if unit.contains("MINUTE") {
        60.0
    } else if unit.contains("HOUR") {
        3600.0
    } else if unit.contains("DAY") {
        86_400.0
    } else if unit.contains("WEEK") {
        604_800.0
    } else if unit.contains("MONTH") {
        2_592_000.0
    } else {
        return None;
    };
    Some((n * per) as i64)
}

/// snake_case → camelCase(limit_5h → limit5h 不适用,limit_month_total
/// → limitMonthTotal 适用;宽容实现)。
fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for c in s.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// RFC3339 UTC(`2026-09-30T05:24:18.44Z`)→ unix 秒。无 chrono
/// 依赖,Howard Hinnant days-from-civil 算法(chatgpt.rs 的逆运算)。
fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let digits =
        |a: usize, b: usize| -> Option<i64> { text.get(a..b).and_then(|s| s.parse::<i64>().ok()) };
    let (y, m, d) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?);
    let hh = digits(11, 13)?;
    let mm = digits(14, 16)?;
    let ss = digits(17, 19)?;
    // 尾部只容 "Z" 或 ".<digits>Z"(厂商实际形状;带时区偏移拒收)。
    let tail = text.get(19..)?;
    let z_ok = tail == "Z" || {
        tail.len() >= 3
            && tail.ends_with('Z')
            && tail.starts_with('.')
            && tail[1..tail.len() - 1].bytes().all(|b| b.is_ascii_digit())
    };
    if !z_ok {
        return None;
    }
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m as u32, d as u32);
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    (secs >= 0).then_some(secs as u64)
}

/// 公历年月日 → epoch 天数(Howard Hinnant days_from_civil)。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Credential-scoped plan readings with their original observation metadata.
pub struct PlanFetcher {
    endpoint: String,
    auth: PlanAuth,
    cache: QuotaCache<PlanWindows>,
}

impl PlanFetcher {
    pub fn new(endpoint: impl Into<String>, auth: PlanAuth) -> Self {
        Self {
            endpoint: endpoint.into(),
            auth,
            cache: QuotaCache::new(),
        }
    }

    pub async fn fetch(&self, client: &reqwest::Client, key: &str) -> Option<PlanWindows> {
        self.read(client, key).await.value
    }

    pub async fn read(&self, client: &reqwest::Client, key: &str) -> QuotaRead<PlanWindows> {
        self.cache.read(key, || self.fetch_once(client, key)).await
    }

    async fn fetch_once(
        &self,
        client: &reqwest::Client,
        key: &str,
    ) -> Result<PlanWindows, QuotaIssue> {
        let request = client
            .get(&self.endpoint)
            .header("Accept", "application/json");
        let request = match self.auth {
            PlanAuth::RawKey => request.header("Authorization", key),
            PlanAuth::Bearer => request.bearer_auth(key),
        };
        let body = response_text(request).await?;
        let envelope: Value =
            serde_json::from_str(&body).map_err(|_| QuotaIssue::InvalidResponse)?;
        let code = envelope["code"]
            .as_i64()
            .or_else(|| envelope["error"]["code"].as_i64());
        match code {
            Some(401 | 403 | 1000..=1005) => return Err(QuotaIssue::Authentication),
            Some(429 | 1302 | 1303) => return Err(QuotaIssue::RateLimited),
            Some(code) if code != 0 && code != 200 => return Err(QuotaIssue::Upstream),
            _ => {}
        }
        if envelope["success"] == false || envelope["error"].is_object() {
            return Err(QuotaIssue::Upstream);
        }
        match self.auth {
            PlanAuth::RawKey => parse_zhipu(&body),
            PlanAuth::Bearer => parse_kimi(&body),
        }
        .ok_or(QuotaIssue::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // fixture = magpie planquota.go 注释记录的真实回包字节
    // (语料纪律:不自造样本;token 类字段本就没有)。

    #[test]
    fn zhipu_plan_windows_and_reset_ms() {
        let windows = parse_zhipu(
            r#"{"success":true,"data":{"level":"pro","limits":[
              {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1758800000000},
              {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":40,"nextResetTime":1758864000000},
              {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":3,"nextResetTime":1758890000000}]}}"#,
        )
        .unwrap();
        assert_eq!(windows.w5h, Some(12.0));
        assert_eq!(windows.w7d, Some(40.0));
        assert_eq!(windows.reset_at, Some(1_758_800_000));
    }

    #[test]
    fn zhipu_envelope_failures_are_none() {
        // 限流码 1302(200 + envelope)→ None(不覆盖已知值)。
        assert!(parse_zhipu(r#"{"error":{"code":1302,"message":"并发数过高"}}"#).is_none());
        // key 被拒 1000–1005 → None。
        assert!(parse_zhipu(r#"{"error":{"code":1003,"message":"api key expired"}}"#).is_none());
        // success=false / 无 data → None。
        assert!(parse_zhipu(r#"{"success":false,"msg":"no plan"}"#).is_none());
        assert!(parse_zhipu(r#"{"success":true}"#).is_none());
        // 真机(2026-11-16,假 key 直打 open.bigmodel.cn):无效 key
        // 回 200 + envelope code 401——envelope 失败一律 None(静默,
        // 负缓存 60s,不覆盖已知值)。
        assert!(
            parse_zhipu(r#"{"code":401,"msg":"令牌已过期或验证不正确","success":false}"#).is_none()
        );
        // 只有 TIME_LIMIT(MCP)→ 无可展示窗口。
        assert!(parse_zhipu(
            r#"{"success":true,"data":{"limits":[
              {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":3,"nextResetTime":1758890000000}]}}"#
        )
        .is_none());
        // 团队 5h 窗口无 number(unit 3 number 0)→ 按 5h 处理。
        let w = parse_zhipu(
            r#"{"success":true,"data":{"limits":[
              {"type":"TOKENS_LIMIT","unit":3,"number":0,"percentage":7,"nextResetTime":1758800000000}]}}"#,
        )
        .unwrap();
        assert_eq!(w.w5h, Some(7.0));
    }

    #[test]
    fn kimi_usage_and_limits_shapes() {
        // 形状一:usage(周)+ limits[](300 分钟短窗 + 5h)。
        let windows = parse_kimi(
            r#"{"usage":{"limit":"100","used":"12","resetTime":"2026-09-30T05:24:18.44Z"},
              "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                "detail":{"limit":"100","remaining":"88","resetTime":"2026-09-30T04:24:18Z"}},
                {"window":{"duration":5,"timeUnit":"TIME_UNIT_HOUR"},
                 "detail":{"limit":"200","used":"50","reset_time":1759200000}}]}"#,
        )
        .unwrap();
        assert_eq!(windows.w5h, Some(25.0), "limits 5h:50/200");
        assert_eq!(windows.w7d, Some(12.0), "usage 周:12/100");
        assert_eq!(
            windows.reset_at,
            Some(1_759_200_000),
            "reset 取 5h 窗口 epoch 秒"
        );
        // 形状二:usages share(used_ratio 0–1)。
        let windows = parse_kimi(
            r#"{"usages":{"limit_5h":{"used_ratio":0.4372,"reset_time":"2026-09-30T05:24:18Z"},
              "limit_7d":{"used_ratio":0.15,"reset_time":1759200000}}}"#,
        )
        .unwrap();
        assert!((windows.w5h.unwrap() - 43.72).abs() < 1e-9);
        assert!((windows.w7d.unwrap() - 15.0).abs() < 1e-9);
        assert_eq!(windows.reset_at, Some(1_790_745_858));
        // 形状三:两者并存 → usages 覆盖同 span 窗口。
        let windows = parse_kimi(
            r#"{"usage":{"limit":"100","used":"12"},
              "usages":{"limit_7d":{"usedRatio":0.66}},
              "limits":[{"window":{"duration":5,"timeUnit":"TIME_UNIT_HOUR"},
                 "detail":{"limit":"100","used":"10"}}]}"#,
        )
        .unwrap();
        assert_eq!(windows.w7d, Some(66.0), "usages 覆盖 usage 周");
        assert_eq!(windows.w5h, Some(10.0));
    }

    #[test]
    fn kimi_rejects_empty_and_unknown() {
        assert!(parse_kimi("{}").is_none());
        assert!(parse_kimi(r#"{"usage":{"limit":"0","used":"0"}}"#).is_none());
        // 只有月窗口 → 字段就两个,v1 不展示 → None。
        assert!(parse_kimi(r#"{"usages":{"limit_month_total":{"used_ratio":0.5}}}"#).is_none());
    }

    #[test]
    fn plan_source_dispatches_by_host() {
        assert_eq!(
            plan_source("https://open.bigmodel.cn/api/paas/v4"),
            Some((
                "https://open.bigmodel.cn/api/monitor/usage/quota/limit".into(),
                PlanAuth::RawKey
            ))
        );
        assert_eq!(
            plan_source("https://api.z.ai/api/coding/v5/"),
            Some((
                "https://api.z.ai/api/monitor/usage/quota/limit".into(),
                PlanAuth::RawKey
            ))
        );
        assert_eq!(
            plan_source("https://api.kimi.com/coding/v1"),
            Some((
                "https://api.kimi.com/coding/v1/usages".into(),
                PlanAuth::Bearer
            ))
        );
        assert_eq!(
            plan_source("https://api.kimi.ai/coding"),
            Some((
                "https://api.kimi.ai/coding/v1/usages".into(),
                PlanAuth::Bearer
            ))
        );
        assert_eq!(plan_source("https://openrouter.ai/api/v1"), None);
        assert_eq!(plan_source("http://127.0.0.1:8080"), None);
    }

    #[test]
    fn rfc3339_parse_matches_known_epoch() {
        // 独立核算(python datetime):2026-09-30T05:24:18Z。
        assert_eq!(parse_rfc3339("2026-09-30T05:24:18Z"), Some(1_790_745_858));
        assert_eq!(
            parse_rfc3339("2026-09-30T05:24:18.44Z"),
            Some(1_790_745_858)
        );
        // 2026 非闰年:2/29 按溢出日照 civil 算法落在 3/1。
        assert_eq!(parse_rfc3339("2026-02-28T00:00:00Z"), Some(1_772_236_800));
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-09-30T05:24:18+08:00"), None);
    }
}

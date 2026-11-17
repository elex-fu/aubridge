//! ledger —— quota ledger(设计文档 §8.3)。
//!
//! 纪律(model-router):**未定价模型记 null,不伪造成本**;记账状态机
//! reported / partial / estimated 自描述可信度。"省了 $X" = 对标模型
//! 单价 × 免费与订阅用量(发布口径),`null` 价格不计入。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// 记账状态(§8.3 状态机)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accounting {
    Reported,
    Partial,
    Estimated,
}

impl Accounting {
    fn as_str(self) -> &'static str {
        match self {
            Accounting::Reported => "reported",
            Accounting::Partial => "partial",
            Accounting::Estimated => "estimated",
        }
    }
}

/// 一条用量记录。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub ts: u64,
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub accounting: Accounting,
}

/// 一条请求完成记录。旧账本的实际模型、用量和耗时可为空。
#[derive(Debug, Clone, Default)]
pub struct DecisionRecord {
    pub ts: u64,
    /// 请求模型(group 语法原样)。
    pub model: String,
    /// 调用方 key 前缀(`aubridge-<agent>` 的 `<agent>`;裸 token = "-")。
    pub agent: String,
    /// 选中源(无候选成功 = 空)。
    pub provider: String,
    /// 尝试序列，只含源、模型、状态、耗时和结构化原因。
    pub attempts: String,
    /// 预检或重试预算跳过的候选。旧记录没有这部分数据。
    pub skipped: Option<String>,
    pub outcome: String,
    pub served_model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub first_content_ms: Option<u64>,
    pub duration_ms: Option<u64>,
}

#[derive(Default)]
pub struct DecisionFilter<'a> {
    pub agent: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub related_provider: Option<&'a str>,
    pub outcome: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub model: Option<&'a str>,
    /// Exclusive cursor ordered by (timestamp, rowid).
    pub before: Option<(u64, i64)>,
}

#[derive(Default)]
pub struct DecisionPage {
    pub rows: Vec<DecisionRecord>,
    pub next: Option<(u64, i64)>,
}

pub struct CompletedRequest {
    pub decision: DecisionRecord,
    /// 切源前已经报告的用量也归实际尝试的源。
    pub usage: Vec<UsageRecord>,
}

/// 对标价格表:模型 → (输入 $/Mtok, 输出 $/Mtok)。
/// 未列出的模型 = null 成本(不伪造)。
pub fn benchmark_price(model: &str) -> Option<(f64, f64)> {
    match model {
        // 同能力档付费对标(设计文档 §8.3 示例)。
        "glm-4.5-flash" | "glm-4.6v-flash" => Some((0.11, 0.11)),
        // Claude 单价(models.dev,§8.3 价格口径,2026-11 采样):
        // 订阅用量按"若走 API 计费"折算;无同类付费对标的模型
        // 以同档 Claude Sonnet 对标。
        "claude-fable-5" | "claude-fable-5-1" => Some((10.0, 50.0)),
        "claude-fable-5-2" => Some((8.0, 40.0)),
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => Some((1.0, 5.0)),
        "claude-haiku-5-5" => Some((0.1, 0.5)),
        "claude-opus-4-5"
        | "claude-opus-4-5-20251101"
        | "claude-opus-4-6"
        | "claude-opus-4-7"
        | "claude-opus-4-8"
        | "claude-opus-5" => Some((5.0, 25.0)),
        "claude-opus-5-5" => Some((4.0, 20.0)),
        "claude-sonnet-4-5" | "claude-sonnet-4-5-20250929" | "claude-sonnet-4-6" => {
            Some((3.0, 15.0))
        }
        "claude-sonnet-5" | "claude-sonnet-5-5" => Some((2.0, 10.0)),
        _ => None,
    }
}

impl UsageRecord {
    /// 已报告数量的对标成本($)。未定价或双方用量均未知时返回 None。
    pub fn benchmark_cost(&self) -> Option<f64> {
        let (pin, pout) = benchmark_price(&self.model)?;
        if self.input_tokens.is_none() && self.output_tokens.is_none() {
            return None;
        }
        let input = self.input_tokens.unwrap_or_default() as f64 * pin / 1e6;
        let output = self.output_tokens.unwrap_or_default() as f64 * pout / 1e6;
        Some(input + output)
    }
}

// ---------------------------------------------------------------------------
// 配额窗口历史(quota-plan-design 2026-11-15 §3.1)
// ---------------------------------------------------------------------------

/// 配额历史保留口径(magpie quota_history.go 同款,写死不做配置,
/// 设计 D2)。
const QUOTA_HIST_KEPT_SECS: u64 = 45 * 24 * 3600;
const QUOTA_HIST_MAX: i64 = 6000;
/// 同周期内读数间隔小于该值 → 移动末点而非新增(节流)。
const QUOTA_HIST_GAP_SECS: u64 = 300;
/// reset 时刻小抖动容忍(周期判定,同 magpie quotaHistSlack)。
const QUOTA_HIST_SLACK_SECS: u64 = 300;

/// 一次读数(内部周期判定用)。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    at: u64,
    left: f64,
    resets_at: Option<u64>,
}

/// 历史里的一个点(admin API 直接序列化给 UI 曲线)。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QuotaPointRow {
    pub provider: String,
    pub window: String,
    pub at: u64,
    /// 剩余百分比 0–100。
    pub left: f64,
    pub resets_at: Option<u64>,
}

/// 周期判定(magpie newCycle 口径):b 读于 a 之后,是否为窗口
/// 重新开始——reset 已过 / reset 大幅移动 / 同 deadline 回灌(剩余
/// +5%)/ 滚动窗口随读数平移不算(移动量 ≈ 读数间隔)。
fn new_cycle(a: &Sample, b: &Sample) -> bool {
    let (ra, rb) = (a.resets_at, b.resets_at);
    if ra.is_none() && rb.is_none() {
        return b.left > a.left + 5.0;
    }
    if ra.is_none() || rb.is_none() {
        return true; // 开始或结束有 reset 信息 → 视为新周期
    }
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    if ra <= b.at {
        return true; // reset 时刻已过
    }
    let moved = rb as i64 - ra as i64;
    let gap = b.at as i64 - a.at as i64;
    let slack = QUOTA_HIST_SLACK_SECS as i64;
    let rolling = moved > 0 && (moved - gap).abs() <= slack;
    if !rolling && moved.abs() <= slack && b.left > a.left + 5.0 {
        return true; // deadline 未动但回灌
    }
    moved > gap + slack || moved < -slack
}

/// SQLite 账本(WAL;路径 ":memory:" 可测)。
pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// 打开(或建库)。父目录缺失时自建——首次运行 `~/.aubridge/`
    /// 不存在,启动不该因此挂掉(engineering-constraints B-7 数据目录)。
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if path != Path::new(":memory:") {
            if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            }
            #[cfg(unix)]
            protect_database_files(path)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let mut conn = Connection::open(path)?;
        if path != Path::new(":memory:") {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        let migration = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        migration.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage (
                ts INTEGER, agent TEXT, provider TEXT, model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, accounting TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage(ts);
            CREATE TABLE IF NOT EXISTS router_state (
                kind TEXT NOT NULL, id TEXT NOT NULL, json TEXT NOT NULL,
                PRIMARY KEY (kind, id)
            );
            CREATE TABLE IF NOT EXISTS decisions (
                ts INTEGER, model TEXT, agent TEXT, provider TEXT,
                attempts TEXT, outcome TEXT, served_model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, skipped TEXT,
                first_content_ms INTEGER, duration_ms INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_decisions_ts ON decisions(ts);
            CREATE TABLE IF NOT EXISTS quota_points (
                provider TEXT NOT NULL, window TEXT NOT NULL,
                at INTEGER NOT NULL, left REAL NOT NULL,
                resets_at INTEGER,
                PRIMARY KEY (provider, window, at)
            );",
        )?;
        for (column, ty) in [
            ("served_model", "TEXT"),
            ("input_tokens", "INTEGER"),
            ("output_tokens", "INTEGER"),
            ("skipped", "TEXT"),
            ("first_content_ms", "INTEGER"),
            ("duration_ms", "INTEGER"),
        ] {
            let exists: bool = migration.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('decisions') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                migration.execute(
                    &format!("ALTER TABLE decisions ADD COLUMN {column} {ty}"),
                    [],
                )?;
            }
        }
        migration.commit()?;
        Ok(Self { conn })
    }

    /// 持久化 router 运行态(write-behind 合并写,§8.1/§8.4):源状态
    /// 与亲和表各一行 upsert。整批一起提交，失败由调用方保留到下一次。
    pub fn save_router_state(
        &self,
        providers: &[(String, manifest::state::ProviderState)],
        sticks: &[(String, router::affinity::Stick)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (id, provider) in providers {
            Self::save_json_state(&tx, "provider", id, serde_json::to_string(provider))?;
        }
        for (key, stick) in sticks {
            Self::save_json_state(&tx, "stick", key, serde_json::to_string(stick))?;
        }
        tx.commit()
    }

    pub fn save_provider_state(
        &self,
        id: &str,
        state: &manifest::state::ProviderState,
    ) -> rusqlite::Result<()> {
        Self::save_json_state(&self.conn, "provider", id, serde_json::to_string(state))
    }

    pub fn delete_provider_state(&self, id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM router_state WHERE kind = 'provider' AND id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn save_provider_catalog(&self, id: &str, json: &str) -> rusqlite::Result<()> {
        Self::save_state(&self.conn, "catalog", id, json)
    }

    pub fn load_provider_catalog(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT json FROM router_state WHERE kind = 'catalog' AND id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
    }

    fn save_json_state(
        conn: &Connection,
        kind: &str,
        id: &str,
        serialized: serde_json::Result<String>,
    ) -> rusqlite::Result<()> {
        let json =
            serialized.map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        Self::save_state(conn, kind, id, &json)
    }

    fn save_state(conn: &Connection, kind: &str, id: &str, json: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO router_state(kind, id, json) VALUES (?1,?2,?3)
             ON CONFLICT(kind, id) DO UPDATE SET json = excluded.json",
            rusqlite::params![kind, id, json],
        )?;
        Ok(())
    }

    /// 载入 router 运行态:源状态 + 亲和表。`known` 为当前注册的源
    /// id——行集与注册集的交集才恢复(配置里删掉的源不复活,新源走
    /// 干净默认态);坏 JSON 行忽略,亲和表超上限(512)不裁剪——裁剪
    /// 由内存侧 record_affinity 决定,落盘的是裁剪后的稳态。
    pub fn load_router_state(
        &self,
        known: &std::collections::BTreeSet<String>,
    ) -> (
        std::collections::BTreeMap<String, manifest::state::ProviderState>,
        std::collections::BTreeMap<String, router::affinity::Stick>,
    ) {
        let mut providers = std::collections::BTreeMap::new();
        let mut sticks = std::collections::BTreeMap::new();
        let Ok(mut stmt) = self.conn.prepare("SELECT kind, id, json FROM router_state") else {
            return (providers, sticks);
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return (providers, sticks);
        };
        for row in rows.flatten() {
            let (kind, id, json) = row;
            match kind.as_str() {
                "provider" => {
                    if !known.contains(&id) {
                        continue;
                    }
                    if let Ok(p) = serde_json::from_str(&json) {
                        providers.insert(id, p);
                    }
                }
                "stick" => {
                    if let Ok(s) = serde_json::from_str(&json) {
                        sticks.insert(id, s);
                    }
                }
                _ => {}
            }
        }
        (providers, sticks)
    }

    /// 记一行(每请求一行,§8.3)。
    pub fn record(&self, r: &UsageRecord) -> rusqlite::Result<()> {
        Self::insert_usage(&self.conn, r)
    }

    fn insert_usage(conn: &Connection, r: &UsageRecord) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO usage VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                r.ts,
                r.agent,
                r.provider,
                r.model,
                r.input_tokens,
                r.output_tokens,
                r.accounting.as_str()
            ],
        )?;
        Ok(())
    }

    /// 决策和用量同批提交，重试不会重复入账。
    pub fn record_requests(&self, rows: &[CompletedRequest]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for row in rows {
            let r = &row.decision;
            tx.execute(
                "INSERT INTO decisions
                 (ts, model, agent, provider, attempts, outcome, served_model, input_tokens,
                  output_tokens, skipped, first_content_ms, duration_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                rusqlite::params![
                    r.ts,
                    r.model,
                    r.agent,
                    r.provider,
                    r.attempts,
                    r.outcome,
                    r.served_model,
                    r.input_tokens,
                    r.output_tokens,
                    r.skipped,
                    r.first_content_ms,
                    r.duration_ms
                ],
            )?;
            for usage in &row.usage {
                Self::insert_usage(&tx, usage)?;
            }
        }
        tx.commit()
    }

    /// 清空已完成请求的明细。队列中的用量仍在同一事务入账。
    pub fn clear_request_history(&self, pending: &[CompletedRequest]) -> rusqlite::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let deleted = tx.execute("DELETE FROM decisions", [])?;
        for usage in pending.iter().flat_map(|request| &request.usage) {
            Self::insert_usage(&tx, usage)?;
        }
        tx.commit()?;
        Ok(deleted + pending.len())
    }

    /// 最近决策(Dashboard"最近决策"区;ts 倒序取前 `limit` 条,
    /// 同秒按写入序新者在前)。
    pub fn recent_decisions(&self, limit: u32) -> rusqlite::Result<Vec<DecisionRecord>> {
        Ok(self.decision_page(DecisionFilter::default(), limit)?.rows)
    }

    pub fn decision_page(
        &self,
        filter: DecisionFilter<'_>,
        limit: u32,
    ) -> rusqlite::Result<DecisionPage> {
        let (before_ts, before_rowid) = match filter.before {
            Some((ts, rowid)) => (ts, Some(rowid)),
            None => (i64::MAX as u64, None),
        };
        // The timestamp bound uses the existing index for older pages. Rowid
        // breaks ties because multiple requests can start in the same second.
        let mut stmt = self.conn.prepare(
            "SELECT rowid, ts, model, agent, provider, attempts, outcome, served_model,
                    input_tokens, output_tokens, skipped, first_content_ms, duration_ms FROM decisions
             WHERE (?1 IS NULL OR agent = ?1)
               AND (?2 IS NULL OR provider = ?2)
               AND (?3 IS NULL OR outcome = ?3
                    OR (?3 = 'failed' AND outcome NOT IN ('ok', 'cancelled')))
               AND ts <= ?4 AND (?5 IS NULL OR (ts, rowid) < (?4, ?5))
               AND (?8 IS NULL OR model = ?8 OR served_model = ?8)
               AND (?9 IS NULL OR provider = ?9 OR EXISTS (
                    SELECT 1 FROM json_each(CASE WHEN json_valid(attempts) THEN
                        CASE WHEN json_type(attempts) = 'array' THEN attempts END END) AS entry
                    WHERE CASE WHEN entry.type = 'object' THEN json_extract(entry.value, '$.id') = ?9 END
                    UNION ALL
                    SELECT 1 FROM json_each(CASE WHEN json_valid(skipped) THEN
                        CASE WHEN json_type(skipped) = 'array' THEN skipped END END) AS entry
                    WHERE CASE WHEN entry.type = 'object' THEN json_extract(entry.value, '$.id') = ?9 END
               ))
               AND (?7 IS NULL OR EXISTS (
                    SELECT 1 FROM json_each(CASE WHEN json_valid(attempts) THEN
                        CASE WHEN json_type(attempts) = 'array' THEN attempts END END) AS attempt
                    WHERE CASE WHEN attempt.type = 'object' THEN
                        json_type(attempt.value, '$.chosen') = 'true'
                        AND json_extract(attempt.value, '$.kind') = ?7
                        AND json_extract(attempt.value, '$.id') = decisions.provider END
               ))
             ORDER BY ts DESC, rowid DESC LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                filter.agent,
                filter.provider,
                filter.outcome,
                before_ts,
                before_rowid,
                i64::from(limit) + 1,
                filter.kind,
                filter.model,
                filter.related_provider,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    DecisionRecord {
                        ts: row.get(1)?,
                        model: row.get(2)?,
                        agent: row.get(3)?,
                        provider: row.get(4)?,
                        attempts: row.get(5)?,
                        outcome: row.get(6)?,
                        served_model: row.get(7)?,
                        input_tokens: row.get(8)?,
                        output_tokens: row.get(9)?,
                        skipped: row.get(10)?,
                        first_content_ms: row.get(11)?,
                        duration_ms: row.get(12)?,
                    },
                ))
            },
        )?;
        let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if rows.len() > limit as usize {
            rows.pop();
            rows.last().map(|(rowid, record)| (record.ts, *rowid))
        } else {
            None
        };
        Ok(DecisionPage {
            rows: rows.into_iter().map(|(_, record)| record).collect(),
            next,
        })
    }

    /// 本机自然月的省 $X。
    pub fn monthly_savings_opt(&self) -> rusqlite::Result<(f64, Vec<(String, f64)>)> {
        let (start, end) = self.conn.query_row(
            "SELECT unixepoch('now','localtime','start of month','utc'),
                    unixepoch('now','localtime','start of month','+1 month','utc')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        self.monthly_savings(start, end)
    }

    /// 月度省 $X 聚合:窗口 [month_start, next_month) 内的对标成本总和,
    /// 以及按源的功臣榜(null 价格不计入,§8.3 口径)。
    pub fn monthly_savings(
        &self,
        month_start: u64,
        next_month: u64,
    ) -> rusqlite::Result<(f64, Vec<(String, f64)>)> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, model, input_tokens, output_tokens FROM usage
             WHERE ts >= ?1 AND ts < ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![month_start, next_month], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<u64>>(2)?,
                row.get::<_, Option<u64>>(3)?,
            ))
        })?;
        let mut total = 0.0;
        let mut per_provider: Vec<(String, f64)> = Vec::new();
        for row in rows {
            let (provider, model, tin, tout) = row?;
            let rec = UsageRecord {
                ts: 0,
                agent: String::new(),
                provider: provider.clone(),
                model,
                input_tokens: tin,
                output_tokens: tout,
                accounting: Accounting::Reported,
            };
            if let Some(cost) = rec.benchmark_cost() {
                total += cost;
                if let Some(entry) = per_provider.iter_mut().find(|(p, _)| *p == provider) {
                    entry.1 += cost;
                } else {
                    per_provider.push((provider, cost));
                }
            }
        }
        per_provider.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok((total, per_provider))
    }

    // -------------------------------------------------------------------
    // 配额窗口历史(quota-plan-design 2026-11-15 §3.1;机制照抄 magpie
    // quota_history.go 的周期判定/节流/折叠,存储改 SQLite)
    // -------------------------------------------------------------------

    /// 一次利用率读数入账:取 (provider, window) 末点,同周期且
    /// 平坦或 <5min → 移动末点(UPDATE);否则新增一点。写入路径
    /// 顺带 prune(45 天 + 每窗口 6000 点,口径写死,设计 D2)。
    pub fn note_quota_point(
        &self,
        provider: &str,
        window: &str,
        left: f64,
        resets_at: Option<u64>,
        now: u64,
    ) -> rusqlite::Result<()> {
        let left = left.clamp(0.0, 100.0);
        let next = Sample {
            at: now,
            left,
            resets_at,
        };
        let tx = self.conn.unchecked_transaction()?;
        let last: Option<Sample> = tx
            .query_row(
                "SELECT at, left, resets_at FROM quota_points
                 WHERE provider = ?1 AND window = ?2
                 ORDER BY at DESC LIMIT 1",
                rusqlite::params![provider, window],
                |row| {
                    Ok(Sample {
                        at: row.get(0)?,
                        left: row.get(1)?,
                        resets_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        if let Some(last) = last {
            if now >= last.at && !new_cycle(&last, &next) {
                // 节流/折叠:更近读数移动末点而非新增(与 magpie addPoint
                // 同义,平坦判定取两点——缓慢下降段 Δ>0.05 不会被折叠)。
                // reset 用 COALESCE 合并:util 采样与 reset 采样可能分批到达。
                if (left - last.left).abs() < 0.05 || now - last.at < QUOTA_HIST_GAP_SECS {
                    tx.execute(
                        "UPDATE quota_points SET at = ?3, left = ?4,
                         resets_at = COALESCE(?5, resets_at)
                         WHERE provider = ?1 AND window = ?2 AND at = ?6",
                        rusqlite::params![provider, window, now, left, resets_at, last.at],
                    )?;
                    tx.commit()?;
                    return Ok(());
                }
            }
        }
        tx.execute(
            "INSERT INTO quota_points (provider, window, at, left, resets_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (provider, window, at) DO UPDATE
             SET left = excluded.left, resets_at = excluded.resets_at",
            rusqlite::params![provider, window, now, left, resets_at],
        )?;
        Self::prune_quota_points(&tx, now)?;
        tx.commit()
    }

    fn prune_quota_points(conn: &Connection, now: u64) -> rusqlite::Result<()> {
        let cut = now.saturating_sub(QUOTA_HIST_KEPT_SECS) as i64;
        conn.execute("DELETE FROM quota_points WHERE at < ?1", [cut])?;
        // 每窗口只留最新 QUOTA_HIST_MAX 点(按 at 倒序排名)。
        conn.execute(
            "DELETE FROM quota_points WHERE at IN (
                SELECT at FROM (
                    SELECT at, ROW_NUMBER() OVER (
                        PARTITION BY provider, window ORDER BY at DESC) AS rank
                    FROM quota_points
                ) WHERE rank > ?1)",
            [QUOTA_HIST_MAX],
        )?;
        Ok(())
    }

    /// 读历史(UI 曲线):`since` 起的点 + 每窗口 `since` 前最近一点
    /// (曲线触及范围边缘,magpie 同款),按 (provider, window, at) 升序。
    pub fn quota_history(
        &self,
        provider: Option<&str>,
        since: u64,
    ) -> rusqlite::Result<Vec<QuotaPointRow>> {
        let mut rows = self.read_points(
            "SELECT provider, window, at, left, resets_at FROM quota_points
             WHERE (?1 IS NULL OR provider = ?1) AND at >= ?2
             ORDER BY provider, window, at",
            provider,
            since,
        )?;
        rows.extend(self.read_points(
            "SELECT p.provider, p.window, p.at, p.left, p.resets_at
             FROM quota_points p
             WHERE (?1 IS NULL OR p.provider = ?1)
               AND p.at = (SELECT MAX(q.at) FROM quota_points q
                           WHERE q.provider = p.provider
                             AND q.window = p.window AND q.at < ?2)",
            provider,
            since,
        )?);
        rows.sort_by(|a, b| (&a.provider, &a.window, a.at).cmp(&(&b.provider, &b.window, b.at)));
        rows.dedup_by(|a, b| a.provider == b.provider && a.window == b.window && a.at == b.at);
        Ok(rows)
    }

    fn read_points(
        &self,
        sql: &str,
        provider: Option<&str>,
        since: u64,
    ) -> rusqlite::Result<Vec<QuotaPointRow>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params![provider, since], |row| {
                Ok(QuotaPointRow {
                    provider: row.get(0)?,
                    window: row.get(1)?,
                    at: row.get(2)?,
                    left: row.get(3)?,
                    resets_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(unix)]
fn protect_database_files(path: &Path) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;

    // SQLite creates its WAL and shared-memory files from the main file's mode.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => restrict_database_file(path)?,
        Err(error) => return Err(error),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        if let Err(error) = restrict_database_file(&PathBuf::from(name)) {
            if error.kind() != ErrorKind::NotFound {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_database_file(path: &Path) -> std::io::Result<()> {
    use std::fs::{self, File, Permissions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let entry = fs::symlink_metadata(path)?;
    if !entry.is_file() || entry.nlink() != 1 {
        return Err(io::Error::other("账本路径需要独立的普通文件"));
    }
    if entry.mode() & 0o777 == 0o600 {
        return Ok(());
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if opened.nlink() != 1 || (opened.dev(), opened.ino()) != (entry.dev(), entry.ino()) {
        return Err(io::Error::other("账本文件已变化"));
    }
    file.set_permissions(Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(model: &str, tin: u64, tout: u64) -> UsageRecord {
        UsageRecord {
            ts: 1000,
            agent: "claude-code".into(),
            provider: "zhipu".into(),
            model: model.into(),
            input_tokens: Some(tin),
            output_tokens: Some(tout),
            accounting: Accounting::Reported,
        }
    }

    #[test]
    fn open_creates_missing_parent_dir() {
        // 首次运行 ~/.aubridge/ 不存在:open 自建父目录,不挂启动。
        let dir = std::env::temp_dir()
            .join("aubridge-ledger-open")
            .join("deep")
            .join("nesting");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
        let db = dir.join("ledger.sqlite");
        let l = Ledger::open(db.to_str().unwrap()).unwrap();
        l.record(&rec("glm-4.5-flash", 1, 1)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let files = [
                db,
                dir.join("ledger.sqlite-wal"),
                dir.join("ledger.sqlite-shm"),
            ];
            for path in &files {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            let reopened = Ledger::open(&files[0]).unwrap();
            assert!(reopened.monthly_savings(0, 2000).unwrap().0 > 0.0);
            for path in &files {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let link = dir.join("linked.sqlite");
            std::os::unix::fs::symlink(&files[0], &link).unwrap();
            assert!(Ledger::open(link).is_err());
        }
        drop(l);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("aubridge-ledger-open"));
    }

    #[test]
    fn records_and_aggregates_monthly_savings() {
        let l = Ledger::open(":memory:").unwrap();
        l.record(&rec("glm-4.5-flash", 1_000_000, 500_000)).unwrap();
        l.record(&rec("glm-4.5-flash", 1_000_000, 500_000)).unwrap();
        // 未定价模型:null 成本,不计入(不伪造)。
        l.record(&rec("unknown-model", 9_999_999, 9_999_999))
            .unwrap();

        let (total, per) = l.monthly_savings(0, 2000).unwrap();
        // 2 × (1M×0.11 + 0.5M×0.11) = 2 × 0.165 = 0.33
        assert!((total - 0.33).abs() < 1e-9, "total = {total}");
        assert_eq!(per.len(), 1, "未定价源不进功臣榜");
        assert_eq!(per[0].0, "zhipu");
        assert!((per[0].1 - 0.33).abs() < 1e-9);
    }

    #[test]
    fn window_excludes_other_months() {
        let l = Ledger::open(":memory:").unwrap();
        let mut r = rec("glm-4.5-flash", 1_000_000, 0);
        r.ts = 500;
        l.record(&r).unwrap();
        let (total, _) = l.monthly_savings(1000, 2000).unwrap();
        assert_eq!(total, 0.0);
    }

    #[test]
    fn unpriced_model_cost_is_null() {
        assert!(rec("nope", 1, 1).benchmark_cost().is_none());
        assert!(rec("glm-4.5-flash", 1_000_000, 0)
            .benchmark_cost()
            .is_some());
    }

    #[test]
    fn claude_prices_match_models_dev_snapshot() {
        // 采样自 models.dev(2026-11);调价时同步改表与本用例。
        let cases = [
            ("claude-fable-5", 10.0, 50.0),
            ("claude-haiku-4-5", 1.0, 5.0),
            ("claude-haiku-5-5", 0.1, 0.5),
            ("claude-opus-4-8", 5.0, 25.0),
            ("claude-sonnet-5", 2.0, 10.0),
        ];
        for (model, pin, pout) in cases {
            assert_eq!(benchmark_price(model), Some((pin, pout)));
        }
        // 未知别名不伪造(§8.3 null-cost 纪律)。
        assert!(benchmark_price("claude-unknown-9").is_none());
    }

    #[test]
    fn reported_usage_cost_preserves_unknown_quantities() {
        let cases = [
            ("glm-4.5-flash", Some(1000), None, Some(0.00011)),
            ("glm-4.5-flash", None, Some(2000), Some(0.00022)),
            ("glm-4.5-flash", Some(1000), Some(2000), Some(0.00033)),
            ("glm-4.5-flash", None, None, None),
            ("glm-4.5-flash", Some(0), None, Some(0.0)),
            ("glm-4.5-flash", None, Some(0), Some(0.0)),
            ("unpriced", Some(1000), None, None),
        ];
        for (model, input, output, expected) in cases {
            let mut usage = rec(model, 0, 0);
            usage.input_tokens = input;
            usage.output_tokens = output;
            usage.accounting = Accounting::Partial;
            match (usage.benchmark_cost(), expected) {
                (Some(actual), Some(expected)) => {
                    assert!((actual - expected).abs() < 1e-12);
                }
                (actual, expected) => assert_eq!(actual, expected, "{usage:?}"),
            }
            assert_eq!((usage.input_tokens, usage.output_tokens), (input, output));
            assert_eq!(usage.accounting, Accounting::Partial);
        }
    }

    #[test]
    fn reported_usage_cost_includes_existing_partial_rows_after_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "aubridge-ledger-partial-usage-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE usage (ts INTEGER, agent TEXT, provider TEXT, model TEXT,
                 input_tokens INTEGER, output_tokens INTEGER, accounting TEXT);
             INSERT INTO usage VALUES
                 (999, 'claude-code', 'before', 'glm-4.5-flash', 1000, NULL, 'partial'),
                 (1000, 'claude-code', 'partial', 'glm-4.5-flash', 3000, NULL, 'partial'),
                 (1500, 'claude-code', 'complete', 'glm-4.5-flash', 1000, 1000, 'reported'),
                 (2000, 'claude-code', 'after', 'glm-4.5-flash', NULL, 1000, 'partial');",
        )
        .unwrap();
        drop(old);

        for _ in 0..2 {
            let ledger = Ledger::open(&path).unwrap();
            let (total, ranked) = ledger.monthly_savings(1000, 2000).unwrap();
            let mut stmt = ledger
                .conn
                .prepare("SELECT input_tokens, output_tokens, accounting FROM usage ORDER BY ts")
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                rows,
                vec![
                    (Some(1000), None, "partial".to_string()),
                    (Some(3000), None, "partial".to_string()),
                    (Some(1000), Some(1000), "reported".to_string()),
                    (None, Some(1000), "partial".to_string()),
                ]
            );
            assert!((total - 0.00055).abs() < 1e-12, "total = {total}");
            assert_eq!(ranked.len(), 2);
            assert_eq!(ranked[0].0, "partial");
            assert!((ranked[0].1 - 0.00033).abs() < 1e-12);
            assert_eq!(ranked[1].0, "complete");
            assert!((ranked[1].1 - 0.00022).abs() < 1e-12);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_request_history_rolls_back_when_pending_usage_cannot_be_saved() {
        let ledger = Ledger::open(":memory:").unwrap();
        let request = |ts| CompletedRequest {
            decision: DecisionRecord {
                ts,
                model: "glm-4.5-flash".into(),
                ..Default::default()
            },
            usage: vec![UsageRecord {
                ts,
                agent: "claude-code".into(),
                provider: "fixture".into(),
                model: "glm-4.5-flash".into(),
                input_tokens: Some(12),
                output_tokens: Some(4),
                accounting: Accounting::Reported,
            }],
        };
        ledger.record_requests(&[request(1)]).unwrap();
        ledger.conn.execute_batch(
            "CREATE TRIGGER fail_usage BEFORE INSERT ON usage BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        ).unwrap();
        let pending = [request(2)];
        assert!(ledger.clear_request_history(&pending).is_err());
        assert_eq!(ledger.recent_decisions(20).unwrap()[0].ts, 1);
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_usage")
            .unwrap();
        assert_eq!(ledger.clear_request_history(&pending).unwrap(), 2);
        assert!(ledger.recent_decisions(20).unwrap().is_empty());
        assert!((ledger.monthly_savings(0, 10).unwrap().0 - 32.0 * 0.11 / 1e6).abs() < 1e-12);
    }

    #[test]
    fn decisions_roundtrip_and_recent_limit() {
        let l = Ledger::open(":memory:").unwrap();
        let mk = |ts: u64, model: &str, provider: &str| CompletedRequest {
            decision: DecisionRecord {
                ts,
                model: model.into(),
                agent: "claude-code".into(),
                provider: provider.into(),
                attempts: r#"[{"id":"a","status":429},{"id":"b","status":200}]"#.into(),
                outcome: "ok".into(),
                first_content_ms: Some(0),
                duration_ms: Some(20),
                ..DecisionRecord::default()
            },
            usage: Vec::new(),
        };
        // 同秒两条(写入序决定先后)。
        l.record_requests(&[mk(100, "group/a+b", "b"), mk(100, "m", "a")])
            .unwrap();
        l.record_requests(&[mk(200, "m2", "c")]).unwrap();

        let recent = l.recent_decisions(20).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].ts, 200, "ts 倒序");
        // 同秒内:后写入的在前。
        assert_eq!(recent[1].provider, "a");
        assert_eq!(recent[2].provider, "b");
        assert_eq!(recent[1].agent, "claude-code");
        assert!(recent[1].attempts.contains("429"));
        assert_eq!(recent[1].first_content_ms, Some(0));
        assert_eq!(recent[1].duration_ms, Some(20));

        let two = l.recent_decisions(2).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].ts, 200);
    }

    #[test]
    fn decision_page_propagates_read_errors_and_recovers() {
        let ledger = Ledger::open(":memory:").unwrap();
        let first_page = || ledger.decision_page(DecisionFilter::default(), 2);
        let empty = first_page().unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.next, None);

        ledger
            .conn
            .execute_batch(
                "INSERT INTO decisions (ts, model, agent, provider, attempts, outcome, input_tokens, skipped)
                 VALUES (99, 'old', 'claude', 'source', '[]', 'ok', 4, NULL),
                        (100, 'probe', 'claude', 'source', 'legacy', 'ok', 3, '{'),
                        (100, 'second', 'claude', 'source', '[]', 'ok', 2, NULL),
                        (100, 'first', 'claude', 'source', '[]', 'ok', 1, NULL);",
            )
            .unwrap();

        let assert_pages = || {
            let first = first_page().unwrap();
            assert_eq!(first.rows.len(), 2);
            assert_eq!(first.rows[0].model, "first");
            assert_eq!(first.rows[1].model, "second");
            assert_eq!(first.next, Some((100, 3)));
            let second = ledger
                .decision_page(
                    DecisionFilter {
                        before: first.next,
                        ..DecisionFilter::default()
                    },
                    2,
                )
                .unwrap();
            assert_eq!(second.rows.len(), 2);
            assert_eq!(second.rows[0].model, "probe");
            assert_eq!(second.rows[0].input_tokens, Some(3));
            assert_eq!(second.rows[0].attempts, "legacy");
            assert_eq!(second.rows[0].skipped.as_deref(), Some("{"));
            assert_eq!(second.rows[1].model, "old");
            assert_eq!(second.next, None);
        };
        assert_pages();

        ledger
            .conn
            .execute(
                "UPDATE decisions SET input_tokens = 'bad' WHERE model = 'probe'",
                [],
            )
            .unwrap();
        assert!(matches!(
            first_page(),
            Err(rusqlite::Error::InvalidColumnType(8, _, _))
        ));
        assert!(ledger.recent_decisions(2).is_err());
        ledger
            .conn
            .execute_batch(
                "UPDATE decisions SET input_tokens = 3 WHERE model = 'probe';
                 ALTER TABLE decisions RENAME TO saved_decisions;",
            )
            .unwrap();
        assert!(first_page().is_err());
        ledger
            .conn
            .execute_batch("ALTER TABLE saved_decisions RENAME TO decisions;")
            .unwrap();
        assert_pages();
    }

    #[test]
    fn older_decisions_keep_history_and_expose_unknown_fields() {
        let dir =
            std::env::temp_dir().join(format!("aubridge-ledger-migration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE decisions (ts INTEGER, model TEXT, agent TEXT, provider TEXT,
                 attempts TEXT, outcome TEXT);
             INSERT INTO decisions VALUES (1, 'm', 'claude-code', 'a', '[]', 'ok');",
        )
        .unwrap();
        drop(old);
        for _ in 0..2 {
            let ledger = Ledger::open(&path).unwrap();
            let rows = ledger.recent_decisions(20).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].provider, "a");
            assert!(rows[0].served_model.is_none());
            assert!(rows[0].input_tokens.is_none());
            assert!(rows[0].output_tokens.is_none());
            assert!(rows[0].first_content_ms.is_none());
            assert!(rows[0].duration_ms.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn pstate(id: &str) -> manifest::state::ProviderState {
        let mut p = manifest::state::ProviderState::new(
            id,
            manifest::schema::ProviderKind::FreeTier,
            vec!["m".into()],
        );
        p.rest_until = Some(9_999);
        p.util_5h = Some(42.5);
        p
    }

    #[test]
    fn router_state_write_rolls_back_a_partial_batch() {
        let l = Ledger::open(":memory:").unwrap();
        l.conn
            .execute_batch(
                "CREATE TRIGGER reject_stick BEFORE INSERT ON router_state
            WHEN new.kind = 'stick' BEGIN SELECT RAISE(FAIL, 'fixture write failure'); END;",
            )
            .unwrap();
        let providers = [("free".into(), pstate("free"))];
        let sticks = [(
            "session".into(),
            router::affinity::Stick {
                main: "free".into(),
                last: "free".into(),
                at: 1,
                cache: 2048,
            },
        )];
        assert!(l.save_router_state(&providers, &sticks).is_err());
        let known = std::collections::BTreeSet::from(["free".into()]);
        let (stored, affinity) = l.load_router_state(&known);
        assert!(stored.is_empty() && affinity.is_empty());
        l.conn.execute_batch("DROP TRIGGER reject_stick").unwrap();
        l.save_router_state(&providers, &sticks).unwrap();
        let (stored, affinity) = l.load_router_state(&known);
        assert_eq!(stored["free"].rest_until, Some(9_999));
        assert_eq!(affinity["session"].main, "free");
    }

    #[test]
    fn router_state_roundtrips() {
        let l = Ledger::open(":memory:").unwrap();
        let stick = router::affinity::Stick {
            main: "prov-a".into(),
            last: "prov-b".into(),
            at: 123,
            cache: 2048,
        };
        l.save_router_state(
            &[("prov-a".into(), pstate("prov-a"))],
            &[("sess-1".into(), stick.clone())],
        )
        .unwrap();
        // upsert:第二次写同一 id 覆盖,不产生重复行。
        l.save_router_state(
            &[("prov-a".into(), pstate("prov-a"))],
            &[("sess-1".into(), stick.clone())],
        )
        .unwrap();

        let known = std::collections::BTreeSet::from(["prov-a".to_string(), "prov-b".to_string()]);
        let (providers, sticks) = l.load_router_state(&known);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers["prov-a"].rest_until, Some(9_999));
        assert_eq!(providers["prov-a"].util_5h, Some(42.5));
        assert_eq!(sticks["sess-1"], stick);
    }

    #[test]
    fn router_state_drops_unknown_providers_and_bad_rows() {
        let l = Ledger::open(":memory:").unwrap();
        l.save_router_state(
            &[("gone".into(), pstate("gone"))],
            &[(
                "sess-1".into(),
                router::affinity::Stick {
                    main: "gone".into(),
                    last: "gone".into(),
                    at: 1,
                    cache: 4096,
                },
            )],
        )
        .unwrap();
        // 手工塞坏 JSON 行(模拟旧版本 schema / 手改文件)。
        l.conn
            .execute(
                "INSERT INTO router_state(kind, id, json) VALUES ('provider','p2','not json')",
                [],
            )
            .unwrap();
        l.conn
            .execute(
                "INSERT INTO router_state(kind, id, json) VALUES ('stick','s2','{')",
                [],
            )
            .unwrap();

        let (providers, sticks) =
            l.load_router_state(&std::collections::BTreeSet::from(["p2".to_string()]));
        // "gone" 不在注册集 → 不复活;坏 JSON → 忽略。亲和表无
        // known 过滤(main 指向已删源时由候选循环自然让位)。
        assert!(providers.is_empty());
        assert_eq!(sticks.len(), 1, "合法 stick 行保留: {sticks:?}");
    }

    fn points(l: &Ledger, provider: &str) -> Vec<QuotaPointRow> {
        l.quota_history(Some(provider), 0).unwrap()
    }

    #[test]
    fn quota_history_throttles_and_folds_within_cycle() {
        let l = Ledger::open(":memory:").unwrap();
        // 同周期(reset 在读数未来,与真实接线一致——reset 从
        // ProviderState 快照带出,同周期内恒有值):三个读数间隔
        // <5min → 移动末点,只留一点。
        l.note_quota_point("zhipu", "5h", 90.0, Some(5000), 1000)
            .unwrap();
        l.note_quota_point("zhipu", "5h", 85.0, Some(5000), 1200)
            .unwrap();
        l.note_quota_point("zhipu", "5h", 85.0, Some(5000), 1400)
            .unwrap();
        let rows = points(&l, "zhipu");
        assert_eq!(rows.len(), 1, "节流折叠为末点: {rows:?}");
        assert_eq!(rows[0].at, 1400);
        assert_eq!(rows[0].left, 85.0);
        assert_eq!(rows[0].resets_at, Some(5000));
        // 间隔 ≥5min 且不平坦 → 新增点。
        l.note_quota_point("zhipu", "5h", 40.0, Some(5000), 1700)
            .unwrap();
        assert_eq!(points(&l, "zhipu").len(), 2);
        // 平坦段(Δ<0.05)即使超 5min 也移动末点。
        l.note_quota_point("zhipu", "5h", 40.0, Some(5000), 3000)
            .unwrap();
        l.note_quota_point("zhipu", "5h", 40.02, Some(5000), 4000)
            .unwrap();
        let rows = points(&l, "zhipu");
        assert_eq!(rows.len(), 2, "平坦折叠: {rows:?}");
        assert_eq!(rows[1].at, 4000);
    }

    #[test]
    fn quota_history_new_cycle_on_reset_passed() {
        let l = Ledger::open(":memory:").unwrap();
        // reset 时刻已过 → 新周期(即使读数间隔 <5min)。
        l.note_quota_point("kimi-code", "5h", 10.0, Some(1000), 1000)
            .unwrap();
        l.note_quota_point("kimi-code", "5h", 95.0, Some(4600), 1200)
            .unwrap();
        let rows = points(&l, "kimi-code");
        assert_eq!(rows.len(), 2, "reset 过期即新周期: {rows:?}");
        assert_eq!(rows[1].left, 95.0);
        assert_eq!(rows[1].resets_at, Some(4600));
    }

    #[test]
    fn quota_history_refill_with_unchanged_deadline_starts_cycle() {
        let l = Ledger::open(":memory:").unwrap();
        // deadline 未动(±5min)但剩余回升 >5% → 回灌,新周期。
        l.note_quota_point("kimi-code", "7d", 20.0, Some(100_000), 1000)
            .unwrap();
        l.note_quota_point("kimi-code", "7d", 90.0, Some(100_300), 2000)
            .unwrap();
        assert_eq!(points(&l, "kimi-code").len(), 2);
        // 滚动窗口:reset 平移量 ≈ 读数间隔且 <5min → 不是新周期,
        // 节流移动末点并把 reset 刷新为平移后的值(COALESCE 语义)。
        l.note_quota_point("zhipu", "5h", 90.0, Some(10_000), 1000)
            .unwrap();
        l.note_quota_point("zhipu", "5h", 88.0, Some(10_200), 1200)
            .unwrap();
        let rows = points(&l, "zhipu");
        assert_eq!(rows.len(), 1, "滚动平移不是新周期: {rows:?}");
        assert_eq!(rows[0].resets_at, Some(10_200));
    }

    #[test]
    fn quota_history_reset_only_one_side_is_new_cycle() {
        let l = Ledger::open(":memory:").unwrap();
        // 前点无 reset、后点有(被动采样先来 util 后来 reset)→ 新周期。
        l.note_quota_point("zhipu", "5h", 50.0, None, 1000).unwrap();
        l.note_quota_point("zhipu", "5h", 48.0, Some(9_000), 2000)
            .unwrap();
        // 同上反向:有 → 无。
        l.note_quota_point("kimi-code", "5h", 50.0, Some(9_000), 1000)
            .unwrap();
        l.note_quota_point("kimi-code", "5h", 48.0, None, 2000)
            .unwrap();
        assert_eq!(points(&l, "zhipu").len(), 2);
        assert_eq!(points(&l, "kimi-code").len(), 2);
        // 两侧都无 reset:5min 内照常节流;剩余回升 >5 才算回灌。
        l.note_quota_point("modelscope", "5h", 40.0, None, 1000)
            .unwrap();
        l.note_quota_point("modelscope", "5h", 44.0, None, 1100)
            .unwrap();
        assert_eq!(points(&l, "modelscope").len(), 1, "节流先于回灌判定");
        l.note_quota_point("modelscope", "5h", 50.0, None, 3000)
            .unwrap();
        assert_eq!(points(&l, "modelscope").len(), 2, "+10 回灌即新周期");
    }

    #[test]
    fn quota_history_prunes_old_points_and_per_window_cap() {
        let l = Ledger::open(":memory:").unwrap();
        let now = 10_000_000u64;
        // 45 天前的点在下一次写入时被清掉。
        l.note_quota_point("zhipu", "5h", 90.0, None, now - QUOTA_HIST_KEPT_SECS - 60)
            .unwrap();
        l.note_quota_point("zhipu", "5h", 80.0, None, now).unwrap();
        let rows = points(&l, "zhipu");
        assert_eq!(rows.len(), 1, "45 天外的点被 prune: {rows:?}");
        assert_eq!(rows[0].at, now);
        // 每窗口超 6000 点:同周期平坦读数只折叠为末点,改用不同
        // reset 时刻强制分周期插入。
        let base = now;
        for i in 0..(QUOTA_HIST_MAX as u64 + 10) {
            // reset 持续前移 → 每点都是新周期(绕过节流)。
            l.note_quota_point(
                "kimi-code",
                "7d",
                50.0,
                Some(base + 100_000 + i * 10_000),
                base + i * 10,
            )
            .unwrap();
        }
        let count: i64 = l
            .conn
            .query_row(
                "SELECT COUNT(*) FROM quota_points WHERE provider = 'kimi-code'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, QUOTA_HIST_MAX, "每窗口上限 6000: {count}");
    }

    #[test]
    fn quota_history_query_edges_and_reopen() {
        let dir =
            std::env::temp_dir().join(format!("aubridge-ledger-quota-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.sqlite");
        {
            let l = Ledger::open(&path).unwrap();
            for (at, left) in [(1000u64, 90.0f64), (2000, 70.0), (3000, 60.0)] {
                l.note_quota_point("zhipu", "5h", left, Some(10_000), at)
                    .unwrap();
            }
            l.note_quota_point("zhipu", "7d", 95.0, Some(100_000), 1000)
                .unwrap();
            l.note_quota_point("kimi-code", "5h", 80.0, None, 2500)
                .unwrap();
            // since=2500:zhipu/5h 带 2000 的触边点;zhipu/7d 只有
            // 触边点(1000 < 2500);kimi-code/5h 原样。
            let rows = l.quota_history(Some("zhipu"), 2500).unwrap();
            let w5h: Vec<u64> = rows
                .iter()
                .filter(|r| r.window == "5h")
                .map(|r| r.at)
                .collect();
            assert_eq!(w5h, vec![2000, 3000], "触边点带出: {rows:?}");
            let w7d: Vec<u64> = rows
                .iter()
                .filter(|r| r.window == "7d")
                .map(|r| r.at)
                .collect();
            assert_eq!(w7d, vec![1000]);
            // 全量(无 provider 过滤)按 (provider, window, at) 升序。
            let all = l.quota_history(None, 0).unwrap();
            let keys: Vec<(String, String)> = all
                .iter()
                .map(|r| (r.provider.clone(), r.window.clone()))
                .collect();
            assert_eq!(
                keys,
                vec![
                    ("kimi-code".into(), "5h".into()),
                    ("zhipu".into(), "5h".into()),
                    ("zhipu".into(), "5h".into()),
                    ("zhipu".into(), "5h".into()),
                    ("zhipu".into(), "7d".into())
                ]
            );
        }
        // 重开库:表结构幂等,历史仍在。
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.quota_history(None, 0).unwrap().len(), 5);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// ---------------------------------------------------------------------------
// 分享卡片(§8.3:口径必须可见)
// ---------------------------------------------------------------------------

/// 生成分享卡片文本。口径标注是发布纪律(§8.3),不可省略。
pub fn share_card(total: f64, per_provider: &[(String, f64)]) -> String {
    let top: Vec<String> = per_provider
        .iter()
        .take(3)
        .map(|(p, _)| p.clone())
        .collect();
    let heroes = if top.is_empty() {
        "暂无用量".to_string()
    } else {
        top.join(" / ")
    };
    format!(
        "本月省了 ${total:.1} · 功臣:{heroes}(对标口径，按已报告用量，部分记录仅计入已知部分，明细可查)"
    )
}

#[cfg(test)]
mod share_tests {
    use super::*;

    #[test]
    fn card_names_top_three_and_caliber() {
        let per = vec![
            ("modelscope".to_string(), 12.0),
            ("zhipu".to_string(), 8.5),
            ("kimi-plan".to_string(), 3.2),
            ("openrouter".to_string(), 0.5),
        ];
        let card = share_card(34.7, &per);
        assert!(card.contains("本月省了 $34.7"));
        assert!(card.contains("modelscope / zhipu / kimi-plan"));
        assert!(!card.contains("openrouter"), "只取前 3");
        assert!(card.contains("对标口径"), "口径必须可见(§8.3)");
        assert!(card.contains("按已报告用量"));
        assert!(card.contains("部分记录仅计入已知部分"));
    }

    #[test]
    fn empty_usage_card_is_honest() {
        let card = share_card(0.0, &[]);
        assert!(card.contains("暂无用量"));
        assert!(card.contains("本月省了 $0.0"));
    }
}

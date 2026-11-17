# 配额窗口历史 + key plan 查询端点 · 技术设计

- 日期:2026-11-15
- 状态:已实施；代码、回放与 ChatGPT 真实历史曲线通过；智谱/Kimi 有效 key 的 Plan 查询仍待验（分项证据见 §9）
- 上游依据:magpie `internal/provider/quota_history.go`(周期判定/节流/保留口径)
  与 `internal/provider/planquota.go`(端点清单/解析语义),机制参考非逐行照抄
- 关联文档:`docs/product-design.md`(§8.2 主动查询、§8.3 账本、变更记录)、
  `docs/ui-design.md`(§6.2 Provider 页)

## 1. 背景与目标

magpie 能力评估(2026-11-15)确定的两项 P1 引入,共用一个会合点:

```
x-codex 头 / anthropic unified 头(既有被动采样)
wham 主动查询(既有) / plan 端点查询(新)
        ↓ 归一为百分比
record_utilization / apply_reset_sample   ← 唯一咽喉
        ↓                        ↓
  ProviderState(路由/rest,不变)   ledger.quota_points(新,历史)
                                      ↓
                        GET /admin/quota-history → Provider 页曲线
```

1. **配额窗口历史**:每次利用率读数顺带采样为时间序列(零额外外呼),
   Provider 页画当前周期曲线;v1 只展示,不改路由行为。
2. **key plan 查询端点**:智谱(open.bigmodel.cn / api.z.ai)与 Kimi Code
   (api.kimi.com / api.kimi.ai)的 plan 窗口查询,结果并入
   `util_5h`/`util_7d`/`resets_at`——与 429 推断互补,让 rest 判定与
   Provider 页余量展示更准。

非目标:不做账号池/跨 key 调度(§3 反目标);不做 rest 精度的历史反哺
(数据先积累,反哺是独立变更);不做 magpie 的跨机合并(davsync)、
jsonl 存储、窗口名称解析。

## 2. magpie 参考与取舍

照抄(机制):
- `QuotaPoint{at, left%, resets_at}` 纯数字四元组(截尾 start 不推导——
  aubridge 窗口 span 编译期已知,不需要按名称猜 span);
- 周期判定 `newCycle`(reset 已过 / reset 大幅移动 / 同 deadline 回灌
  +5% / 滚动窗口随读数平移不误判,±5min slack);
- 5min 节流(更近读数移动末点而非新增)+ 平坦段折叠;
- 保留 45 天 / 每窗口 6000 点(写死)。

不抄(结构):
- jsonl + filememo/lastgood 内存缓存——rusqlite(WAL)一张表直接解决;
- `windowSpan` 正则名称解析——aubridge 窗口是 `util_5h`/`util_7d` 两个
  编译期已知字段,周期判定只依赖 resets_at,不依赖 span;
- plan 端点的 team 分支(ZhipuTeam type=2)、StepFun/Volcengine/Tencent
  通道、多 key fan-out——单 key 单账号语义,无池化。

plan 端点语义照抄(踩过的坑):
- 智谱 `/api/monitor/usage/quota/limit`:**裸 key 作 `Authorization`**
  (非 Bearer);envelope `success=false` 或 data 缺失 = 无 plan
  (免费档 key 无窗口,静默);200 + code 1302/1303 = 限流 → 当读失败,
  不覆盖已知值;code 1000–1005 = key 被拒;
- Kimi `{coding base}/usages`:**Bearer**;三种形状并容
  (`usage`+`limits[]` / `usages.limit_*` share / 两者并存取 usages),
  数字字符串或数值,reset 为 RFC3339 文本或 epoch 秒/毫秒;
- 单飞 + 正缓存 180s / 负缓存 60s(与 wham/usage 同口径,§8.2)。

## 3. 模块设计

### 3.1 ledger:quota_points 表(能力 1 存储)

```sql
CREATE TABLE IF NOT EXISTS quota_points (
    provider TEXT NOT NULL, window TEXT NOT NULL,  -- "5h" | "7d"
    at INTEGER NOT NULL,        -- unix 秒(读数时刻)
    left REAL NOT NULL,         -- 剩余百分比 0–100
    resets_at INTEGER,          -- unix 秒,可空
    PRIMARY KEY (provider, window, at)
);
CREATE INDEX IF NOT EXISTS idx_quota_points_at ON quota_points(at);
```

```rust
/// 一次读数入账(周期判定 + 节流 + 折叠,见 §2 机制):
/// 取 (provider, window) 末点;同周期且 <5min 或平坦段 → UPDATE 末行;
/// 否则 INSERT。写入路径顺带 prune(45 天 + 6000 点)。
pub fn note_quota_point(&self, provider: &str, window: &str,
    left: f64, resets_at: Option<u64>, now: u64) -> rusqlite::Result<()>;

/// 读历史(UI 曲线):窗口过滤 + 取范围前一点(曲线触及边缘)。
pub fn quota_history(&self, provider: Option<&str>,
    since: u64) -> Vec<QuotaPointRow>;
```

`new_cycle(prev: QuotaPointRow, next) -> bool` 为纯函数(测试口径
直接对齐 magpie newCycle 的判定分支)。

### 3.2 credentials/src/planquota.rs(能力 2 解析与拉取)

与 usage.rs(claude/wham)同位同纪律:纯解析可测 + 带缓存拉取器。

```rust
/// plan 窗口查询结果:归一为 (used_percent, resets_at) 两个窗口。
pub struct PlanWindows { pub w5h: Option<f64>, pub w7d: Option<f64>,
                         pub reset_at: Option<u64> }  // reset 取最短窗口
pub fn parse_zhipu(body: &str) -> Option<PlanWindows>;   // 裸 Authorization
pub fn parse_kimi(body: &str) -> Option<PlanWindows>;    // Bearer
pub struct PlanFetcher { /* endpoint + cache:正 180s/负 60s/单飞 */ }
impl PlanFetcher {
    pub fn new(endpoint: impl Into<String>, auth: PlanAuth) -> Self;
    pub async fn fetch(&self, client: &reqwest::Client, key: &str)
        -> Option<PlanWindows>;
}
/// host → (端点, 鉴权形态) 分发(manifest.provider.base 的 host 匹配)。
pub fn plan_source(base: &str) -> Option<(String, PlanAuth)>;
// open.bigmodel.cn / api.z.ai → /api/monitor/usage/quota/limit,裸 key
// api.kimi.com / api.kimi.ai(base 含 /coding)→ {coding base 去尾}/usages,Bearer
```

解析纪律:serde 从宽(字符串/数值双态、多 reset 键名);
解析失败/envelope 无 plan → None(负缓存,静默);智谱窗口映射:
span ≤24h 的最短窗口 → `w5h`,7 天窗口 → `w7d`,TIME_LIMIT(MCP)忽略;
Kimi:5h → `w5h`,7d → `w7d`,月窗口 v1 不展示(字段就两个,不扩)。

### 3.3 gateway 接线

- `record_utilization`(state.rs 咽喉):锁内读 `state.util_5h/7d +
  resets_at` 快照,锁外调 `ledger.note_quota_point`(ledger Mutex 独立于
  manifests/providers 锁序;尽力语义——未挂 ledger 或写失败不影响转发,
  与 router_state write-behind 同口径)。
- `sample_plan_quotas(&self)`(新,仿 `sample_wham_usage`):遍历
  manifests,`plan_source(&m.provider.base)` 命中且内存 keys 有该源 key
  → `PlanFetcher.fetch` → `record_utilization` + `apply_reset_sample`。
  触发点 = `dashboard_route` 与 `providers_route`(wham 同款,缓存
  180s 保证频繁刷新不放大外呼)。
- `providers_overview` 行增 `util_5h`/`util_7d`/`resets_at`(读
  providers ProviderState;曲线之外的余量即时值)。

### 3.4 admin API

```
GET /admin/quota-history?provider=<id>&days=<n>   (admin key 鉴权)
  → { "items": [ { "provider": "kimi-code", "window": "5h",
        "points": [ { "at": 1763..., "left": 88.0, "resets_at": 1763... } ] } ] }
```

days 缺省 7,非法/超 45 → 45(保留上限口径)。

### 3.5 UI(crates/smoke/web/src/Providers.tsx)

- 源行 `row-meta` 加利用率徽标(接上既有未接线的 `fmtUtil`:
  `5h 42% / 7d 15%`);
- detail-panel 新增"额度"section:展开源行时按需拉
  `GET /admin/quota-history?provider=<id>`,渲染 SVG 折线
  (当前周期的 left% 曲线 + 直线均匀消耗参考线);无历史显示
  "暂无窗口读数"。不新增页面(拍板 3)。

## 4. 安全与纪律对照

- 历史只存纯数字(provider id、窗口名、百分比、unix 秒)——无 token、
  无 prompt、无请求内容(与账本 usage 表同隐私口径);
- plan 查询是**读操作**:key 只发往该源 manifest `permissions.net` 白名单
  内的 host(智谱/Kimi 端点均在);不做任何写、不做池化调度
  (rest 决策仍由 429/被动采样驱动,plan 读数只让字段更准);
- `quota-history` 在 admin 面(回环 + admin token);
- ledger 写入尽力语义,失败不影响请求转发路径。

## 5. 测试计划

- **ledger 单测**:节流(<5min 移动末点)、平坦折叠、周期判定分支
  (reset 过/移动/回灌/滚动平移)、prune 双口径、查询取范围前一点、
  重开库幂等;
- **planquota 单测**:fixture 用 magpie 注释记录的真实回包字节
  (智谱 limits 样例、Kimi 三形状、字符串数字、epoch 秒/毫秒 reset)、
  envelope 限流/key 拒/无 plan → None、host 分发边界;
- **集成测(gateway/tests/)**:① record_utilization 路径落历史
  (quota-history 路由读回 + admin key 401 口径);② 假 upstream
  (本地 axum 回 plan JSON)→ sample_plan_quotas → providers_overview
  util 字段 + ProviderState 落盘;③ 未挂 ledger 的 state 零影响;
- **真机验收**:zhipu / kimi-code 真 key 查询(读数入历史),UI 曲线渲染。

## 6. 依赖与分层

- 无新依赖(ledger 已用 rusqlite,credentials 已用 reqwest/serde);
- 新文件 `credentials/src/planquota.rs`(≤300 行级)与
  `docs/` 设计文档;ledger/state/lib/Providers.tsx 均为增量修改;
- 解析层(credentials)不 import gateway;端点分发键 = manifest base
  host(既有"配额采样器分发键"约定,product-design §8.2)。

## 7. 落地顺序

1. ledger:quota_points + note/查询 + 单测;
2. credentials/planquota.rs:解析器 + Fetcher + 单测;
3. state.rs:咽喉接线 + sample_plan_quotas + providers_overview 增强;
4. lib.rs:quota-history 路由 + 触发点;
5. Providers.tsx:徽标 + 额度 section 曲线 + ui-design.md 增补;
6. product-design.md 变更记录 36;
7. 真机验收(§5)。

## 8. 决策记录

| # | 决策 | 理由 |
|---|---|---|
| D1 | 端点范围 = 智谱(open.bigmodel.cn/api.z.ai)+ Kimi Code;zen 端点先探测后决定;MiniMax 等不在 §7 白名单不做 | 拍板 2026-11-15;zen go/v1/usage 是订阅口径,免费档是否有同形状端点需实测,结论回填本文档 |
| D2 | 保留 45 天 / 6000 点照抄,写死 | 拍板 2026-11-15;不做配置(sub2api 教训) |
| D3 | 曲线落 Provider 页源行展开,不新增页面 | 拍板 2026-11-15(仪表盘已并入 Agents 汇总卡,逐源 UI 的家是 Provider 页;等效"快照行展开折线") |
| D4 | v1 历史只展示,不反哺 rest 精度 | 拍板 2026-11-15;数据先积累,反哺是独立变更 |
| D5 | 记点只在 util 有值时(record_utilization 路径),resets_at 从 ProviderState 快照顺带带出 | 咽喉单点;apply_reset_sample 不单独记点(无 left 可画) |
| D6 | 存储用 ledger 新表,不照抄 jsonl | rusqlite WAL 已在;无 500KB 重解析问题 |
| D7 | 窗口名固定 "5h"/"7d"(ProviderState 字段名),不解析厂商窗口名 | aubridge 窗口编译期已知;周期判定只依赖 resets_at |

## 9. 阶段验收记录(2026-11-16)

隔离数据目录起 headless 网关(PLUGINS_DIR 重定向,不碰生产 App 的
flock 与 keychain),环境变量注入 fixture key:

- **端点真实形状直测**:智谱 `open.bigmodel.cn/api/monitor/usage/
  quota/limit` 对无效 key 回 **HTTP 200 + envelope
  `{"code":401,"msg":"令牌已过期或验证不正确","success":false}`**
  (magpie 注释的 envelope 失败形状实证;已作真实字节进单测);
  Kimi `api.kimi.com/coding/v1/usages` 对无效 key 回 401。两者都
  走 None 负缓存——网关不崩、providers 路由正常返回、失败不覆盖。
- **负缓存**:`/admin/providers` 二次调用 12ms(无外呼)。
- **历史路由**:`quota_points` 直插 3 点 → `GET /admin/quota-history
  ?provider=&days=7` 按 (provider, window) 分组返回、at 升序、
  形状与 UI 类型一致。
- **真实 key 读数**与**曲线人工目检**留待生产壳升级后观察(真实
  key 在生产 keychain,debug 二进制因 cdhash ACL 读不了;UI 曲线
  组件 tsc/vite build 已过,渲染待桌面壳目检)。

### 2026-11-17 升级前安装版复核

- 昨晚的安装包已包含额度历史与 cc-switch 导入，两个鉴权管理接口实测均为
  HTTP 200，不能继续沿用 2026-11-16 白天「安装版仍缺接口」的判断。
- 真实桌面模型源页显示 ChatGPT 利用率，历史接口含 ChatGPT 的 5h、7d
  窗口读数；这证明既有 wham 读数进入历史，不替代新增智谱/Kimi Plan 查询验收。
- 当前桌面页明确显示智谱与 Kimi 凭据未设置，OpenCode 标准 auth 文件不存在。
  本次未导入其他应用凭据、未创建原生登录文件。官方有效 key 读数和原生
  OpenCode 登录/续期仍为独立待验项；已有单点历史也不构成完整曲线目检通过。

### 2026-11-17 已签名源码基线的安装与验收

- 完整源码基线为 Git `e09b983522a56645f95ae3345d2418abe1d4a55b`，包含
  269 个受跟踪文件、10 个 workspace crate。离线 Git bundle 已恢复成干净检出，
  锁定依赖的 workspace metadata 校验通过。
- 使用 `scripts/release-macos.py build` 从该干净提交构建，前端、fmt、严格
  Clippy 和全目标工作区测试通过：25 个测试目标、434 passed / 0 failed /
  0 ignored。候选包内嵌 revision、工具链信息，回执记录全部文件哈希。
- 初次 npm 安装遇到 ETIMEDOUT，离线重试因缺少 Vite 缓存以 ENOTCACHED
  失败，两份日志保留。采用本机已经启用的 HTTPS 代理作进程内 npm 配置后，
  锁文件安装和全部门禁完成；没有修改系统代理或依赖锁文件。
- 旧包此前严格签名校验失败。新包完整 ad-hoc 签名、strict verify 和全量
  manifest 比对通过。安装器在旧实例仍运行时的拒绝覆盖对照通过；随后经
  原生 Cmd+Q 正常退出，保留完整旧包并安装，新运行 PID 57987，端口 62760。
- `/healthz`、鉴权 `/admin/quota-history`、鉴权 `/admin/import/cc-switch`
  均 HTTP 200。真实原生窗口可读取 9 条 cc-switch 候选，仅查看并取消，未导入。
- 本机真实 ChatGPT 7d 历史已积累两个点。CUA 展开来源并滚动到额度区后，
  AX 与截图均确认真实剩余曲线、两个端点及虚线参考可见；没有注入合成历史。
  5h 仍只有一点，不将它算作完整曲线。此结果验证 wham → ledger → 原生 UI，
  不替代智谱/Kimi Plan 端点的真实有效凭据查询。
- 安装前后 aubridge 配置、Codex config/auth、Claude settings 的字节 SHA 均
  一致；当前登录来源仍为 Codex，share_login=false。智谱/Kimi key 未设置、
  OpenCode 原生 auth 文件不存在，两项独立外部条件仍待补齐，未冒称验收完成。
- 本机回执（发布包及运行材料不随源码分发）：
  `.local/releases/e09b983522a5-20261117T054439Z/` 下的 `receipt.json`、
  `installation.json`、`acceptance.json`、`running-install-guard.json` 及门禁日志。
  该基线之后的本次提交仅整理验收文档，生产源码与已安装包使用的提交一致。

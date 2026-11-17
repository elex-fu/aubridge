# cc-switch 配置导入 · 技术设计

- 日期:2026-11-15
- 状态:已实施(2026-11-15,真机验收通过)
- 上游依据:本机 `~/.cc-switch/cc-switch.db` 只读实测(2026-11-15)
  + magpie `internal/provider/importapps.go`(算法参考,非逐行照抄)
- 关联文档:`docs/product-design.md`(变更记录)、`docs/ui-design.md` §6.2
  (Provider 页)、`docs/crate-layout.md` §1

## 1. 背景与目标

cc-switch 是用户已在用的 Claude Code / Codex 供应商切换器,第三方中继
(relay)配置都存在它那里。aubridge 让这些源"零手工录入"进入候选表:

- 读 cc-switch 的 provider 配置(只读),转换为 aubridge 自定义源
  (`CustomProvider` + keyring);
- 交互 = Provider 页"从 cc-switch 导入"按钮 → 候选列表 → **勾选即新增**
  (拍板 2026-11-15:不做 magpie 的 replace / key-of 三模式,冲突即报错);
- **Codex 行 Responses 协议 v1 一并支持**(拍板 2026-11-15)。

非目标:不写回 cc-switch(单向导入);不自动发现(用户点开才读,
magpie 同款边界——cc-switch 列表里可能含 aubridge 自己的行);不导入
cc-switch 的用量/技能/代理日志。

## 2. 数据源事实(实测)

- cc-switch 3.x:SQLite `~/.cc-switch/cc-switch.db`,providers 表
  `(id, app_type, name, settings_config, website_url, category, sort_index, …)`,
  主键 `(id, app_type)`。**本机 9 行**:claude×4、claude-desktop×1、
  codex×3、gemini×1。
- `settings_config` 是**各 agent 的原生配置 JSON**:
  - claude / claude-desktop 行:`env.ANTHROPIC_BASE_URL` +
    `env.ANTHROPIC_AUTH_TOKEN|ANTHROPIC_API_KEY`,模型名散布在
    `ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`、
    `ANTHROPIC_SMALL_FAST_MODEL`;
  - codex 行:`auth.OPENAI_API_KEY` + `config`(内嵌 TOML 字符串:
    顶层 `model_provider` / `model`,`[model_providers.<id>]` 表的
    `base_url` / `wire_api` / `experimental_bearer_token`);
  - gemini 行:`env.GEMINI_API_KEY`。
- 本机关键行:

  | 行 | app_type | base | 处置 |
  |---|---|---|---|
  | Kimi For Coding | claude | `http://127.0.0.1:62760` | **skip:回环地址(实测该端口就是 aubridge 生产网关,导入即自环)** |
  | Local | claude | `http://192.168.31.191:15721` | 可导入(局域网网关非回环,net 白名单自动加 host) |
  | Longcat | claude | `https://api.longcat.chat/anthropic` | 可导入,anthropic 协议 |
  | Longcat | codex | `https://api.longcat.chat/openai/v1` | 可导入,**responses 协议**(`wire_api="responses"`) |
  | 各 Official / default | — | 无 base 无 key | skip:官方登录行 |

- 旧版 cc-switch(config.json,JSONC)**v1 不支持**:本机已是 db 版,
  不为不存在的存量做双格式(需要时再补)。

## 3. magpie 参考与取舍

照抄:per-app 解析规则、官方登录行 skip 语义("agent's own sign-in")、
SQLite `mode=ro` 只读、用户主动打开才读的边界。

不抄(并写死为决策):

- **merge(同 key 同 host 多行合一)**——aubridge 的 CustomProvider 是
  单协议单 base,Longcat 两条 → 两个独立源;多端点合并是 manifest 结构
  变更,不为导入引入。
- **导入后多协议端点合并、website 补全**——website v1 不导(Provider 页
  不展示,信息价值低)。
- **自环检测**是 magpie 没有的、本机真实存在的问题(见 §2),必须做。

## 4. 模块设计

### 4.1 L1 解析模块 `credentials/src/ccswitch.rs`

与 `credentials/src/opencode.rs`(同样只读解析别的 app 的凭据)同层
同纪律:

```rust
/// 一条候选:可导入(provider 有值)或被跳过(skip 有原因)。
pub struct CcSwitchCandidate {
    pub reference: String,   // "claude/0a77…" — app_type/id,POST 回传凭据
    pub name: String,        // cc-switch 里的显示名
    pub source: String,      // "Claude Code" / "Codex" / "Gemini CLI"…
    pub skip: Option<String>,
    pub provider: Option<ImportedProvider>,
}

pub struct ImportedProvider {
    pub id: String,          // 见 §4.2 id 规则
    pub base: String,
    pub protocol: String,    // "anthropic" | "chat" | "responses"
    pub auth: String,        // 恒 "bearer"(两类行的 token 都走
                             //   Authorization: Bearer)
    pub kind: String,        // 恒 "subscription"(relay 付费 key)
    pub models: Vec<String>, // env / TOML 收集,去重保序
    pub key: String,         // → keyring;不出现在 GET 应答
}

/// 只读打开(mode=ro),读 providers 表按 app_type+sort_index 排序。
/// db 缺失 → Ok(vec![])(上层显示"未找到 cc-switch 配置")。
pub fn read_candidates(db: &Path) -> io::Result<Vec<CcSwitchCandidate>>;
```

解析规则(每行独立判定,坏行 skip 不中断整表):

| app_type | base | key | protocol | models |
|---|---|---|---|---|
| claude / claude-desktop | `env.ANTHROPIC_BASE_URL`(缺省不补官方地址:无 base 即 skip;补 `/v1` 段——Claude Code 请求走 `{base}/v1/messages`,aubridge 的 Anthropic 协议路径是 `{base}/messages`,2026-11-15 实测 longcat:补前 404、补后 401(=路径存在)) | `env.ANTHROPIC_AUTH_TOKEN` ∥ `ANTHROPIC_API_KEY` | `anthropic` | 5 个 ANTHROPIC_MODEL env,去重保序 |
| codex | TOML `model_providers.<model_provider>.base_url` | `auth.OPENAI_API_KEY` ∥ TOML `experimental_bearer_token` | `wire_api == "chat"` → `chat`,否则 **`responses`** | TOML 顶层 `model` |
| 其他(gemini 等) | — | — | — | skip:"v1 仅导入 claude/codex 行"(gemini 官方 key 行本机也无可导内容) |

skip 规则(优先级从高到低):

1. **回环 base**(host 是 127.0.0.1/localhost/[::1])→ skip
   "本地代理端点(可能自环,如经 aubridge 转发),不导入"。
   ——不做动态端口比对:回环 base 在 claude 行的语义就是本地转发,
   一律挡下比猜端口稳;确有本地模型源需求时是独立新需求。
2. 无 base 且无 key → skip "官方登录行,订阅请在 aubridge 内登录"。
3. 有 key 无 base → skip(不猜默认端点,与 magpie 对 claude 行
   `base=="https://api.anthropic.com"` 的补全不同:cc-switch 行写
   无 base 的 token 几乎只见于官方行,补全反而导入一个错端点)。

TOML 解析复用 `toml` crate(agents crate 已用,workspace 树内依赖,
见 §7);不做 magpie 式手写 parseTOML。

### 4.2 id 生成

`id = slug(name)`:小写、空白与非法字符折成 `-`(Kimi For Coding →
`kimi-for-coding`,Longcat → `longcat`)。同批或与既有源冲突时:
claude 行保原名 slug,codex 行追加 `-codex`(本机 Longcat 双行即
`longcat` + `longcat-codex`),仍冲突则追加序号。POST 时
`add_custom_provider` 的既有 id 冲突校验兜底(冲突行失败带原因,
其余行照常成功——勾选即新增语义)。

### 4.3 Gateway 装配与 admin API

- `GatewayState::with_cc_switch_db(PathBuf)`(既有 builder 模式,
  测试注入临时 db);缺省 `~/.cc-switch/cc-switch.db`(HOME 解析,
  与 runtime.rs 同风格)。
- 路由(admin key 鉴权,同 `/admin/providers` 口径):

```
GET /admin/import/cc-switch
  → { "items": [ { reference, name, source, skip?,
                   provider?: { id, base, protocol, auth, kind, models } } ] }
    // key 永不回显(与 providers_overview 的 key 红线同口径)

POST /admin/import/cc-switch   body { "refs": ["claude/0a77…", …] }
  → { "added": [id…],
      "failed": [ { "reference", "error" } ] }   // 逐行结果
```

POST 处理(**每次重读 db**,不信任 GET 快照——两次点击之间
cc-switch 可能已改):对每个 ref 重跑解析 → `state.add_custom_provider
(&CustomProvider)`(既有路径:registry 校验 + config.json 字段级
合并原子写 + 候选表热更新)→ `state.set_provider_key(&id, &key)`
(既有路径:keyring 写入)。顺序:先加源再写 key——key 失败时源以
missing-credential 状态可见,UI KeyForm 可补救;反向会留孤儿 key。
解析层 skip 的 ref 进 `failed`(带原 skip 原因)。

### 4.4 UI(crates/smoke/web/src/Providers.tsx)

Provider 页头部"从 cc-switch 导入"按钮(仅当 GET 返回非空 items 时
可用)→ 弹层:候选表(名称 / 来源 / base / 协议 / 模型数,skip 行
灰显带原因、不可勾选)→ 勾选提交 → 展示逐行 added/failed →
刷新源表。样式复用既有 Dialog 与 Provider 行表格;无新组件库依赖。
ui-design.md §6.2 增补一段操作流。

## 5. 凭据与安全纪律(红线对照)

- db **只读**打开(`mode=ro` URI,与 ledger/探测脚本同纪律);
- key 全程不出现在 GET 应答、日志、config.json——只在 POST 内存中
  短暂经过,落点 keyring(`set_provider_key`,字段级路径);
- config.json 写入走既有字段级合并原子写,不整体覆盖;
- 不写 cc-switch 任何文件;不探测其代理端口;
- GET/POST 均在 admin 面(回环 + admin token),不新增暴露面。

## 6. 测试计划

- **单测**(credentials/ccswitch.rs):fixture 用本机真实行的**结构
  字节**(token 替换为占位符,base/模型名保留——语料纪律:不自造
  样本):claude 行解析、codex TOML responses/chat 两分支、官方行
  skip、回环 skip、无 base 有 key skip、gemini 行 skip、坏 JSON 行
  skip 不中断、id 冲突后缀、排序稳定性。
- **集成测**(gateway/tests/):临时 rusqlite 建表插行 →
  `with_cc_switch_db` 注入 → GET 断言 items 与"key 不回显" →
  POST 勾选 refs → 断言 config.json 出现 CustomProvider(字段级合
  并不抹既有字段)、MemoryStore 有 key、路由快照热更新;POST 未知
  ref / id 冲突 → failed 带原因;db 缺失 → GET 空列表。
- **验收**(真机):对真 `~/.cc-switch/cc-switch.db` 跑 GET,预期
  Longcat×2 可导、Kimi/官方行 skip;勾选 Longcat 双行导入后
  `aubridged provider test longcat` 与 `provider test longcat-codex`
  (诊断 CLI,变更记录 34)分别以 anthropic/responses 协议连通。

## 7. 依赖与分层

- `credentials` crate 新增 `rusqlite`(workspace 已有,ledger 在用,
  bundled)与 `toml`(workspace 树内已有,agents 在用)——**无树外
  新依赖**,不触发 §13 同级评审,但在 product-design 变更记录里
  显式记录两依赖的第二个消费者。
- 分层:ccswitch.rs 是纯解析(IO 边界 L1,contract = CcSwitchCandidate),
  与 opencode.rs 同位;协议类型不入 credentials(输出的是 manifest
  既有字符串枚举,serde 从宽由 registry::load 兜底校验)。
- db 读取是同步小查询(providers 表几行),直接在 admin 路由内同步
  调用,与 settings/keyring 的既有同步 IO 同风格,不引 spawn_blocking
  (需要时再加)。

## 8. 决策记录

| # | 决策 | 理由 |
|---|---|---|
| D1 | 交互 = 勾选即新增,冲突行失败带原因 | 拍板 2026-11-15;replace/key-of 是为存量迁移设计的复杂度,aubridge v1 没有这个存量 |
| D2 | codex 行 responses 协议一并支持 | 拍板 2026-11-15;wire_api 解析极廉,不支持反而要加 skip 规则 |
| D3 | 不做 magpie 式多行 merge | CustomProvider 单协议单 base;merge 需要 manifest 结构变更 |
| D4 | 回环 base 一律 skip(含自环) | 本机 Kimi 行实测即 aubridge 自身;动态端口比对脆,一律挡下稳 |
| D5 | 无 base 有 key 的行不补默认端点 | 此形态实际只见于官方登录行;补全 = 导入错端点 |
| D6 | 仅支持 db 版 cc-switch,旧 config.json 不做 | 本机与主流已是 db 版;双格式是投机灵活性 |
| D7 | POST 每次重读 db | GET 快照与提交之间配置可能已变;重读代价可忽略 |
| D8 | key 只经 keyring,GET 不回显 | 与 provider key 红线同口径 |

## 9. 落地顺序

1. `credentials/src/ccswitch.rs` + 单测(纯解析,先行可测);
2. `GatewayState::with_cc_switch_db` + GET/POST 路由 + 集成测;
3. Providers.tsx 导入弹层 + ui-design.md 增补;
4. product-design.md 变更记录 35(本设计的落地记录 + 依赖第二消费者
   备案);
5. 真机验收(§6),含诊断 CLI 联测。

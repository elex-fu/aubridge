# crate 分包与编译加速设计 v2

> 状态:已定(M0,v2 按职责分工与插件化重设)
> 上游文档:`docs/product-design.md` §5/§6.2/§9
> 约束来源:`CLAUDE.md`(最小改动、拒绝过度设计)

## 1. 分包原则

1. **职责即 crate = 协作单元**。一个 crate 一个明确职责、一个 owner
   (CODEOWNERS);crate 公开 API 就是分工合同,跨 crate 改合同需
   受影响方评审,crate 内部自由。
2. **三层拓扑,纯核 + 脏边界**(六边形):决策逻辑(L0)无 IO、
   无重依赖、秒级 check、可任意并行开发;重依赖(L1/L2)只在
   边界,各归一个叶子 crate,互不传染。
3. **插件化 = 数据合同,不是运行时插件**。设计文档 §9:v1 插件是
   纯声明式 manifest。`manifest` 就是插件 SDK:内置 preset
   与第三方插件**同 schema、同装载路径**。不建 plugin-sdk /
   plugin-runtime crate;wasmtime 版(v2 若需要自定义配额探测)仅为
   预留位,不预建。
4. **依赖严格单向、无环**:L0 ← L1 ← L2;同层不互相依赖
   (translate 不依赖 router,registry 不依赖 ledger)。
5. **L1 接口纪律**:IO crate 的公开 API 不得泄漏其重依赖类型
   (如返回自己的 `ProbeReport` 而非 `reqwest::Response`),避免
   重链经由接口扩散到依赖方。
6. 新 crate 只在出现新依赖重量边界或职责边界时建;`cargo build
   --timings` 显示恶化先测量再拆(crate 级阈值见 §4)。

## 2. 目标结构(L0 合同与纯逻辑 → L2 壳)

**命名**:crate 名不带 `aubridge-` 前缀(工作区内部名,不发布到
crates.io,前缀是冗余);目录名 = crate 名。

```
crates/
  ── L0 合同与纯逻辑(零 IO,并行秒级,任何人可同时改)─────────
  proto        线协议类型 + SSE 编解码                [serde, serde_json]
  manifest     插件/源的声明模型与校验(合同):       [serde]
               Provider/QuotaSource/RestPolicy schema,
               内置 preset = 内置 manifest
  translate    翻译层:IR、chat↔anthropic、sanitize、
               F-1~F-10 回归                          [proto]
  router       候选循环/rest/sink/错误分类/亲和:     [manifest]
               纯决策,输入 = manifest 的
               ProviderState 快照,输出 = 执行计划
  (语料 fixtures 先放 translate/tests/fixtures;M1 gateway
   集成测试成为第二消费者时再抽 crate——单消费者不建 crate)

  ── L1 IO 边界(各引各的重依赖,层内并行编译)────────────────
  registry     源注册表:manifest 装载 + preset +
               quota 探活/usage 查询                  [manifest, reqwest]
  credentials  keyring + OAuth 导入/刷新/写回         [keyring, reqwest]
  ledger       账本:用量记账、价格、省 $X 口径        [rusqlite, proto]
  agents       agent wiring 三件套(stash/applied/
               drift、原子写)                         [纯文件 IO]

  ── L2 组装壳(薄)────────────────────────────────────────────
  gateway      axum 网关:组装 L0+L1,路由/中间件/
               holdWriter/patience、上游转发流        [L0+L1, axum, reqwest]
  smoke        Tauri 冒烟壳(无窗口;M4 进化为桌面壳)  [gateway, tauri(-wry)]

  预留(v2+,不预建):
  plugin-probe  wasmtime 配额探测模块(仅当内置探测器
                覆盖不了某源时立项,见设计文档 §9)
```

依赖图(箭头 = 依赖方向):

```
proto ── translate ──┐
manifest ── router ──┤            L0(并行,check < 2s/crate)
   │  registry(reqwest) │ credentials(keyring) │ ledger(rusqlite) │ agents(纯文件)
   └────────────┬────────────────────────────────┘   L1(并行)
            gateway(axum+reqwest)                     L2
            └─ tauri 壳:smoke(M0/M1 冒烟)→ M4 进化为 src-tauri
               (同一壳的演进,不是长期并存的两个 tauri crate)
```

## 3. 职责与协作分工(crate = 工作包)

| crate | 职责 | owner 角色 | 对外合同要点 |
|---|---|---|---|
| proto | 线协议忠实建模 | 协议 | serde 从宽;不引运行时 |
| manifest | 源声明 schema + 校验 | 源/插件 | 内置与第三方同路径;校验失败给可读错误 |
| translate | 翻译保真 | 协议 | 事件序列契约;F-1~F-10 全绿 |
| router | 调度决策 | 调度 | 纯函数:快照 → 执行计划;无锁无 IO |
| registry | 源装载与探活 | 源/插件 | 不泄漏 reqwest 类型 |
| credentials | 凭据 | 安全 | 只读默认;写回 opt-in;字段级合并 |
| ledger | 计量 | 计量 | null-cost 纪律;记账状态机 |
| agents | 接线 | 集成 | 三件套语义;卸载无尸体 |
| gateway | 组装 | 集成 | holdWriter/patience;安全清单 |
| smoke / src-tauri | 壳 | 集成 | 壳内不写业务逻辑 |

协作纪律:

- L0 合同(proto/manifest)变更 → 通知所有 L1 owner 评审;
- 每个 crate 自带单元测试;跨 crate 集成测试只放 gateway;
- PR 尽量单 crate;跨 crate 的合同变更拆成"合同 PR + 各适配 PR"。

## 4. 体积与拆分阈值(codex 校准后)

- 文件/函数上限沿用 `CLAUDE.md` §5(≤3,000 行 / ≤500 行)。
- **拆分的真实边界是契约与所有权(§1),行数只是气味指标**:codex
  的 protocol crate 2.2 万行不拆、core 18 万行靠纪律不靠拆——
  行数从来不驱动拆分。
- crate 级气味触发线(达到先测量,不自动拆):生产代码 > 15,000 行,
  或冷缓存 `cargo check -p <crate>` > 5s 且由本 crate 自身导致 →
  `cargo build --timings` 定位后,按 §1 的职责/重量边界拆。
- **单消费者不建 crate**:只有当一个模块出现第二个消费 crate 时才
  抽独立 crate(fixtures 规则的推广)。

## 4.5 codex 对照校准(2026-11-03,~/play/codex 实测)

| codex 事实 | 数据 | 对本方案的裁定 |
|---|---|---|
| 155 个 crate,但 core 18 万行 / tui 23 万行单 crate 不拆 | 巨型核心靠纪律控制("resist adding to it"),不靠拆分 | 我们的 gateway 是薄组装壳;不担心出现 core 型巨物 |
| 真正拆出的 crate = 契约面(protocol 2.2 万行、app-server-protocol、ext/extension-api)+ 平台件(sandbox 系)+ 团队所有权(ext/*) | 契约先于实现稳定,契约 crate 值得早建 | 保留 proto/manifest 早建 ✅ |
| model-provider(3.2k 行)、execpolicy 与 core 分离 | 决策逻辑独立成 crate 便于测试 | 保留 router 纯函数化 ✅ |
| utils/string(560 行)、utils/elapsed(71 行) | 微 crate 是拆过头的反例 | 我们无此问题;新增"单消费者不建 crate"规则,砍掉 fixtures crate |
| codex 平均 ~7k 行/crate | 拆分粒度参照 | 本方案 9 crate 目标(全项目估 2 万行)收敛合理 |

裁定结果:目标结构从 11 crate 收敛到 9(proto/manifest/translate/
router/registry/credentials/ledger/agents/gateway)+ tauri 壳演进
(smoke → src-tauri)。

## 5. 编译加速(已生效配置)

### 5.1 profile 与 config(已落地)

- `[profile.dev.package."*"] opt-level = 2`:依赖一次性 O2 缓存,
  dev-run 可自测;
- `split-debuginfo = "unpacked"`(macOS 链接提速)、`incremental = true`;
- release `lto = "thin"` + `codegen-units = 1`;
- `.cargo/config.toml` alias:`ck` / `tg` / `smoke`。

### 5.2 依赖裁剪

- `tauri` default-features=false(smoke 无窗口);
- `reqwest` default-features=false(回环无 TLS);
- `axum` 最小 feature;`[workspace.dependencies]` 统一版本,
  `cargo tree -d` 复核重复。

### 5.3 分层带来的加速

- L0 四 crate 并行 check,单 crate < 2s:日常改翻译/调度的循环
  不碰任何重依赖;
- L1 各自的重依赖编译一次进共享缓存,互不触发;
- tauri 链只在 smoke/src-tauri 自身或 gateway 接口变化时重编译
  (薄壳纪律:壳内零业务逻辑);
- 监控:季度跑 `cargo build --timings`,整链变化 > 30s 需在 PR 说明。

## 6. 落地节奏(按里程碑建 crate,不预建)

| 时点 | 建 | 说明 |
|---|---|---|
| M0(当前) | proto ✅、translate ✅(API 契约) | 语料放 translate/tests/fixtures(单消费者) |
| M1 | manifest、router、registry、agents、gateway、smoke | router 先只做"免费优先 + rest/sink";gateway 内含临时上游转发,接口稳后如需再抽 `upstream` |
| M2 | ledger、credentials | router 补亲和/错误分类全矩阵 |
| M4 | src-tauri(由 smoke 进化) | 桌面壳 |

当前状态(M0 完成后):

- proto ✅(协议类型含 tools/tool_choice/stream_options、SSE 编解码
  含 `frame_anthropic`);
- translate ✅(响应侧状态机 + 请求翻译 + thinking 整流 + count 双路径;
  F-1~F-11 + golden + SSE 往返共 13 测试全绿;语料 synthetic 待 M1 录制
  器替换);
- smoke ✅(Tauri+axum 同进程冒烟 PASS:退出码 0,SSE 翻译回环;
  `cargo smoke` 一键复验);
- clippy 0 警告,`cargo ck` 全链 <1s(不含 tauri 链);
- 修正:tauri 需保留默认 feature(wry)——无窗口应用仍用 Wry runtime,
  `Builder<W>` 没有无 wry 的默认实现;
- 其余 crate 按上表节奏创建。

M1 进度(2026-11-03):manifest ✅(3 测试)、router ✅(22 测试:
候选循环/错误分类矩阵/rest 四桶)、registry ✅(4 测试:preset 与
第三方 manifest 同路径)、gateway ✅(3 集成测试:回放服务器驱动
429 failover/状态转移/key 校验)、agents ✅(Claude Code 三件套,
3 测试)。smoke 改走真实 gateway 全链路 PASS。全仓 50 测试、
clippy 0 警告。

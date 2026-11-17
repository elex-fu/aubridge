# aubridge 设计方案 v1.1(2026-11-03)

# aubridge 免费额度代理客户端 · 设计文档

- 日期:2026-11-03
- 状态:草案(v1.1,待评审)
- 项目名:`aubridge`(已定)
- 技术栈决策:Rust + Tauri 2(已确认)
- UI 信息架构与操作流:见 `docs/ui-design.md`(与本文同级权威;
  冲突时先改其一再改另一)

## 变更记录(v1 → v1.1)

37. **Agent 高级配置 + 真实 CLI 测试(已实施 2026-11-16)**:
    Claude Code 与 Codex 的 Agents 行新增高级配置弹层和固定命令
    测试。① **接口**:`GET/POST /admin/agents/{id}/config` 读写在位
    Agent 的客户端配置,`POST /admin/agents/{id}/test` 发起一次真实
    CLI 探针;仅 admin key 可访问,未知 Agent 400。② **Claude
    Code**:读写 `~/.claude/settings.json` 的 `model`、`env`、
    `permissions.defaultMode` 等字段,角色映射用
    `ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU,FABLE}_MODEL[_NAME]`,
    1M 以模型名 `[1M]` 后缀声明;密钥类环境变量读取时打码为
    `••••••••`,保存时原样回填。③ **Codex**:只编辑受管
    `aubridge.config.toml` 的顶层字段(模型、推理强度/摘要、
    verbosity、审批、沙箱、web_search、上下文/输出上限);
    `[model_providers.aubridge]` 的地址、回环 token 与 `wire_api`
    仍由 aubridge 管理。④ **并发编辑**:两端读取都返回内容
    revision,保存时不一致即拒绝,避免 UI 旧快照覆盖外部编辑。
    ⑤ **测试**:命令与提示词是后端常量,前端不能拼接参数。
    Claude Code 固定 `claude -p --strict-mcp-config
    --no-session-persistence --output-format text`,Codex 固定
    `codex --profile aubridge exec --skip-git-repo-check
    --ephemeral --sandbox read-only --color never`;均在临时工作
    目录、清空标准输入、150 秒超时、16K 输出截断,并脱敏配置中的
    凭据;同一时间只允许一个测试。二进制可用
    `AUBRIDGE_CLAUDE_BIN` / `AUBRIDGE_CODEX_BIN` 覆盖,否则搜索
    PATH 与常见安装目录。⑥ **UI**:Claude Code/Codex 的「编辑」
    打开高级配置(参数、完整 JSON、测试验证三个页签;角色映射与
    请求覆盖在参数页),「测试」直达测试页;OpenCode 保留原简单
    模型弹层。测试可能消耗订阅额度,UI 明示。

36. **配额窗口历史 + key plan 查询端点(已实施 2026-11-16)**:
    方案 `docs/quota-plan-design-2026-11-15.md`(magpie 能力评估的
    P1 两项,共用 `record_utilization`/`apply_reset_sample` 咽喉)。
    ① **窗口历史**:ledger 新表 `quota_points`(纯数字——
    provider/窗口名/剩余百分比/unix 秒,无凭据无 prompt),读数
    照抄 magpie 周期判定(reset 过期/大幅移动/同 deadline 回灌
    ±5%阈值/滚动平移不算)+ 5min 节流移动末点 + 平坦折叠,
    保留 45 天/每窗口 6000 点写死;`GET /admin/quota-history
    ?provider=&days=`(缺省 7,超 45 收敛 45)按 (provider,
    window) 分组返回;Provider 页源行加"用量"徽标与"额度"
    section 展开折线(单 series 剩余百分比 + 虚线均匀消耗参考,
    复用既有 CSS token,懒挂载)。v1 只展示不反哺 rest(数据先
    积累,设计 D4)。② **plan 端点**:`credentials/planquota.rs`
    解析智谱 `open.bigmodel.cn|api.z.ai /api/monitor/usage/
    quota/limit`(**裸 key 作 Authorization**,envelope 限流码
    当读失败、1000–1005 = key 拒、TIME_LIMIT 忽略)与 Kimi
    `{coding base}/usages`(Bearer,usage+limits+usages 三形状
    并容,数字字符串/数值双态,reset RFC3339 文本或 epoch
    秒/毫秒);拉取器与 wham 同构(正缓存 180s/负缓存 60s/
    单飞);`sample_plan_quotas` 由 providers/dashboard 路由触发,
    结果并入 `util_5h`/`util_7d`/`resets_at`——rest 决策仍由
    429 路径驱动(只查询与展示,不做池化调度,红线维持)。
    fixtures 用 magpie 注释记录的真实回包字节。Zen go/v1/usage
    端点(订阅口径)留待实测后决定(设计 D1),不在本批。

35. **cc-switch 配置导入(已实施 2026-11-15)**:
    `GET/POST /admin/import/cc-switch`(方案:
    `docs/cc-switch-import-design-2026-11-15.md`)。只读解析
    `~/.cc-switch/cc-switch.db` providers 表(claude 行 env →
    Anthropic 协议,base 补 `/v1` 段对齐 Claude Code 请求路径;codex
    行 TOML → chat/responses 按 `wire_api`);勾选即新增:逐行走既有
    `add_custom_provider`(config.json 字段级合并)+ `set_provider_key`
    (keyring),key 永不回显、POST 每次重读 db。回环 base 一律 skip
    (防自环:cc-switch 行可能指向 aubridge 自身)。credentials crate
    成为 rusqlite / toml 的第二消费者(ledger / agents 之后,无树外
    新依赖)。
    - 验收中发现并修复既有缺陷:`set_provider_key` / `clear_provider_key`
      原持有 manifests 读锁跨 OS 钥匙串 IO——钥匙串授权等待会连锁阻塞
      provider 增删,网关整面停摆(状态锁内不做 IO,回归测试
      `keyring_io_holds_no_state_locks` 固化)。真机验收:两条 Longcat
      源分别以 anthropic / responses 协议 `provider test` 连通。

34. **诊断 CLI 子命令(已实施 2026-11-15)**:`aubridged status` /
    `providers` / `provider test <id> [model]` 三枚只读命令。命令行读
    数据目录会话文件 `gateway-session.json`,中继到**正在运行**的网关
    admin API(healthz / providers_overview / provider check)——不自建
    GatewayState,避免与生产网关争数据目录锁(沿用 33 的共用口径)。
    桌面壳与 headless 同样发布会话文件。边界:命令行只做验证诊断,
    不做配置编辑、key 管理、暂停/恢复控制(管理操作归 UI,30 的
    "CLI/TUI 形态"非目标维持不变);`provider models` / `usage` 类
    诊断留待需要时增补,不预建。

33. **单机 v1 的 Claude Code 首发链路**。Tauri 和命令行共用生产网关及数据目录锁。
    Tauri 通过本地 IPC 取得实际端口和当次管理 token。管理 token 由系统随机数生成，
    管理权限与推理权限分开。Provider key 使用平台钥匙串。内置 ChatGPT 通道独占
    Codex OAuth，连接器固定目标地址并拒绝重定向。
    - 候选先筛就绪凭据、冷却和请求能力。免费优先分组按全组排序，亲和在各梯队内生效。
      声明式源可选 `models_without_tools` 字段标注已知不支持工具调用的模型。
      OpenRouter 的 key 预算按 USD 单独展示。免费请求的窗口由限流响应判断。
      [OpenRouter 当前 key 接口](https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key)
    - 首段有效内容发送前保留换源权。HTTP 200 后的空流、断流和超时仍进入候选循环。
      首段内容后发生断流则结束当前流。显式 `stream:false` 聚合为 Messages JSON。
      `count_tokens` 在原生上游可用时转发，否则按实际目标协议估算。
    - 请求结束后合并落盘决策和尝试用量。断流和客户端取消各自记录，
      缺失用量保留为空。月度对标统计按本机自然月汇总。

32. **五页 UI 改版 + 管理 CRUD 面(已实施 2026-11-05)**:
    按 `docs/ui-design.md` v1.1 落地完整批次(后端四批 + 前端
    五页,admin CRUD 均带集成测试):

    **后端批次(均已测试)**:
    ① **Provider/keys 面**:`/admin/providers` GET/POST/DELETE
    (自定义源落 config.json,字段级合并原子写)+ `/admin/keys`
    POST/DELETE(key 只回布尔,值走 keyring,永不回显);
    ② **Agents 改版 + 自动虚拟模型**:router 别名 `aubridge`
    写死(请求此名 → 全部健康启用源,per-req 改写 model 为
    源首声明模型);AgentWiring 加 set_model/current_model
    + `POST /admin/agents/{id}/model`;agents_route 加
    model/token/models 字段(模型下拉 = 自动置首 + 字面 +
    分组名;token = aubridge-<id>);
    ③ **模型分组 store**:credentials settings 加
    `model_groups`(name/members/rule,字段级合并);GroupRule
    枚举 free_first(缺省)/sequential(声明序不重排)/
    round_robin(整表轮转,尽力语义);`GET/POST/DELETE
    /admin/model-groups`;命名分组解析优先级 group/ 前缀
    先判、非前缀查表、`aubridge` 永不查表;
    ④ **代理页**:serving RwLock<bool> 软开关(off 时推理
    路径 503 `网关已暂停`,进程不退,admin 面照常,不持久化
    重启回默认开);`GET/POST /admin/gateway`(端点 +
    admin_token 回显——调用方已过 admin 校验,与 provider
    key 红线不同口径 + 最近 20 条请求);decision_row 共用
    投影替换 dashboard 内联闭包。

    **前端五页(单次 npm build 通过)**:App 五页导航
    (Agents|Provider|模型|代理|Plugin,默认 Agents,
    go 跨页跳转通道);Agents 页 = 汇总三卡(本月省
    $/源健康/最近请求·成功率)+ 引导卡(§7 状态驱动)+
    agent 行表(接线 switch/model badge/编辑弹层:模型
    下拉 + token 复制);Provider 页 = 源表 + 行内 KeyForm +
    添加卡片流 + 可反代 Agents + 凭据回写区(原设置页
    并入底部;**2026-11-06 用户拍板改为独立"设置"页签,最右**——
    ui-design §5 同步改 6 页,App/Providers 已重构,npm build
    通过);模型页 = 对外模型单表(自动行内置不可删 +
    字面模型行同名多源计数/规则 select 即时 upsert + 跨模型
    合并分组行/编辑/删除)+ 新建分组表单;代理页 = 状态
    badge + 停止/运行软开关 + 接入方式(endpoint + token
    复制,endpoint 只展示回环不提供监听地址修改,§8 红线)+
    历史请求表(失败行"去设置 Provider"跳转)。

    **实测结果(2026-11-05)**:gateway 集成测试 34(新增
    model_groups_admin_crud_and_persistence /
    named_group_routing_rewrites_member_model_and_survives_reload /
    gateway_serving_switch_and_access_info 等);工作区 fmt/
    clippy 零警告;`npm run build` tsc 零错。

    **明确不做(防过度设计)**:[+ 添加 Agent] 按钮(无后端
    路由,检测驱动自动出现,不造);监听地址修改(回环写
    死);规则自定义/嵌套(枚举三选)。

31. **magpie 能力差距评审与补充批次(批 1 + 批 2 均已实施)**
    (2026-11-04，对照 yetone/magpie，除源广度外):逐项评审结论
    分三档——**补**(本批开发)、**缓**(方案定稿后下一批)、
    **不做**(非目标，防过度设计)。总则：无新 crate、依赖方向
    不变、正确行为写死不做配置。

    **批 1(已实施 2026-11-04,三项，均为小步)**:
    ① **HTTP client 复用**(性能):`upstream.rs` 每请求
    `reqwest::Client::new()` 丢连接池/TLS 会话;改为
    `GatewayState` 持有一个 client(`with_client` 构造注入，
    默认 `Client::builder().pool_max_idle_per_host(…)` 建一个),
    upstream 取用。无架构变化，纯修复。
    ② **§11 预算基线**(性能):无测量基线=预算空文。补一个
    集成测试：本地回放上游 + N 次串行 `/v1/messages`，断言
    转发 p99 ≤ 30ms(§11 预算，回环环境)——是测试不是基准
    框架，CI 可跑即可;不建 perf harness、不上Criterion。
    ③ **agent 接线产品化**:即变更记录 30 原案,本批第 3 项,
    一并实施(见 30 号记录的实测结果)。

    **批 1 实测结果(2026-11-04)**:①③ 见变更记录 30(共享
    client + admin 面 + Agents 页);② perf 集成测试
    `forward_p99_within_budget`(回放上游 + 100 次计量,预热 10)
    debug 构建实测 p99 ≈ 1ms、p50 ≈ 0.7ms,预算 30ms 余量充足;
    工作区 fmt/clippy 零警告、测试全绿(gateway 集成 26,含
    agents admin 5 + perf 1;agents 15)。

    **批 2(已实施 2026-11-04,两项)**:
    ④ **跨模型兜底(routing group 最小版)**(能力深度):
    magpie 的 group 是"多模型当一个用，配额耗尽跨模型迁移"。
    最小版 = **请求内联 union 语法**:`model = "group/a+b"`
    (或经 URL 转义的等价形式)在请求里显式列成员，**零配置
    零状态**——router 加纯函数 `candidates_group(members,
    states, now)` = 各成员 candidates 按声明序拼接去重(成员
    内仍走现有免费优先排序);upstream 尝试循环按"该候选供应
    哪个成员"改写请求的 model 字段。语义限制写明：成员顺序 =
    偏好顺序，不做跨成员再排序;`free/<model>` 与 group 不叠加
    (group 成员里再写 free/ 前缀按字面模型处理，不支持)。
    **明确不做**:user-defined group 配置、嵌套 group、跨
    provider 账号 spreading——那是多账号池语义(§3 反目标
    邻域)，需要时另起设计评审。
    ⑤ **请求决策日志**(可观测):B-10 默认仅错误日志，失败时
    看不见"为什么走 A 没走 B"。最小闭环：ledger 加
    `decisions` 表(ts/model/agent key 前缀/选中源/尝试序列/
    结局)，write-behind 合批写(复用现有脏集机制，一条请求
    一行，量小);Dashboard 加"最近决策"区(最近 20 条)。
    不做全文 dump/脱敏回放(magpie debug dump 版)——那是
    独立设计。

    **批 2 实测结果(2026-11-04)**:④ router 纯函数落
    `router::group`(parse_members/candidates_group/member_of
    8 单测,plan 拆出 candidates_literal 字面匹配变体,非组
    请求路径零变化);gateway 循环内按候选源改写 model(组请求
    = 声明序首个字面供应成员),集成测试
    `group_model_fails_over_and_rewrites_model_per_candidate`
    断言 prov-b 收到的 body model=="m-b"(字面成员,非 group
    语法原文)。⑤ ledger `decisions` 表 + `recent_decisions`
    (ts 倒序,同秒按写入序);gateway `record_decision` 合批
    write-behind(哨兵 `@decisions`,上限 1024 丢最旧),status
    0 = 未发出(凭据/网络),预检失败 = 状态码,成功 = 200;
    /admin/dashboard 返回 `recent_decisions`(attempts 解析回
    数组);Dashboard.tsx 加"最近决策"表(vite 资产已重建)。
    集成测试 `decision_log_records_failover_attempts_and_outcome`
    断言尝试序列 `[prov-a:429, prov-b:200]`、agent 前缀、
    outcome ok。工作区 fmt/clippy 零警告、测试全绿(gateway
    集成 28;router 65;ledger 9)。

    **不做(逐项理由，防过度设计)**:
    - 多账号池/额度转发：§3 反目标，不变;
    - privacy masking(magpie 有):未立项；凭据遮蔽若要做是
      独立设计(正则集维护是长期负担)，本批不动;
    - TUI/CLI 形态、Docker/Termux:形态扩张非能力缺口，桌面壳
      是既定产品形态(§1);
    - admin token 可配、自定义 provider UI:插件路径
      (manifest 数据合同)已覆盖自定义端点，UI 加表单 = 为
      低频操作造持久复杂度，不做;
    - Gemini/Responses 原生入面:v1.1 既定节奏(§4)，不提前;
    - ~~gemini CLI 接线器入 UI~~ → 随 30 的 trait 注册表自然
      获得入口(注册一行，零额外成本)，并入批 1。

    **架构变化汇总**:L0 `router` 加一个纯函数(④);L1 `ledger`
    加一张表(⑤);L2 `gateway` state 加共享 client + agents
    路径注入(①③)——三层各动一处，无新 crate、无依赖方向变化、
    无运行时新依赖(§5 表不增行)。

    **随提供方/订阅方增长的扩展点评估**(2026-11-04 增补，
    与 30 的 trait 评审同源):源的"定义"已数据化(manifest
    插件)，会乘法扩张的是三个行为轴——① **订阅凭据通道**
    (chatgpt.rs + upstream 调用点 + credentials 的
    oauth/stash/settings/usage 整链，ChatGPT 定制):第二个
    订阅方出现时抽 `SubscriptionChannel` trait
    (ready/refresh/build_request/sample_signals);② **配额
    采样器**(usage.rs wham 是 Codex 专属形状;分发键已存在 =
    manifest `quota.source` 字段，届时按它注册 `UsageSampler`);
    ③ **对标价格表**(ledger benchmark_price 硬编码 match，
    线性增长单表，不抽象，碍事时走数据迁移)。translate 的
    `Translator` 枚举已是对的行为面，不加抽象。router 核心
    (classify/rest/熔断)吃抽象信号，厂商无关，不动。
    **规则:每轴由第二个真实实现驱动抽象，不预建注册表。**


30. **agent 接线产品化(§8.6,已实施 2026-11-04)**:接线器
    三件套(Claude Code / Codex CLI / OpenCode 的
    `apply`/`disconnect`/`wired`,agents crate,M2 已落地)至今
    无产品化入口——README 仍让用户手改 `settings.json`/`config.toml`。
    对照 magpie Agents 页(核心交互),补一条最小闭环:
    ① **admin 面**(gateway,admin token 校验与插件/设置路由同
    口径):`GET /admin/agents` 返回 `{configured, agents: [{id,
    name, wired, detected, drift, hint}]}`;`POST /admin/agents/
    {id}/enable` body `{"on": bool}` 接线/断开。未接线注入
    (configured=false)时返回空表,UI 展示"未配置"——与设置页
    同口径。`drift` 仅 Claude Code 有语义(clean/replaced/
    unwired,applied 记录);`hint` 承载用户侧残余动作(codex
    受管块用 `env_key = AUBRIDGE_TOKEN`,接线后需
    `export AUBRIDGE_TOKEN=aubridge-codex`;Claude Code/OpenCode
    把 env/key 写进各自配置文件,无残余动作)。
    ② **状态宿主**(GatewayState,**实施时修正**:不持有 trait
    对象集合,持 `agents: Option<agents::AgentsPaths>` +
    `port: RwLock<u16>`(默认 3427,`serve()` 绑定后回写实际
    端口——§4 端口回退时接线 base_url 必须写实际值,这是接线
    器需要 port 而非组装时定死的唯一原因);接线器按需现建
    `agents::wirings(&paths, self.port(), models)`——admin 面
    低频,构造四个小结构零成本,而原案
    `Option<Vec<Box<dyn AgentWiring>>>` 会在组装时把端口焊死,
    与"端口绑定后才权威"冲突)。OpenCode 模型
    列表 = 网关聚合模型(`models()`),接线时 `(id, id)` 注入。
    **接线器抽象形态(评审修正,2026-11-04)**:抽象层是
    **trait + 注册表**,不是模板方法。`agents` crate 定义
    `AgentWiring`(id/name/apply/disconnect/wired/detected,
    drift/hint 带默认实现),`wirings(paths, port, models)`
    构建全部接线器——**新 agent 的扩展点 = 一个 impl 模块 +
    注册表一行**,gateway 无 match 分支,UI 行天然数据驱动。
    **不抽象"改写算法骨架"**:四家的配置格式(JSON/TOML)、
    key 传递(配置内 env/shell env/配置内 apiKey)、stash 粒度
    (子树快照/整文件字节)各异,codex 的整文件 stash 使
    "stash→改写→原子写"骨架不成立;硬抽模板的 hook 面比四个
    手写实现之和还大(伪抽象)。真共享的原子写/JSON 读写/mtime
    保持 crate 级 free helper。gemini 接线器(M2 已有 impl)随
    注册表自然获得入口,一并进本批。
    ③ **组装层**:aubridged 注入默认路径(`~/.claude/settings.json`
    /`~/.codex/config.toml`/`~/.config/opencode/opencode.json`,
    stash 落 `~/.aubridge/agents/`);smoke 冒烟壳不注入(自检
    进程不该碰用户真实配置文件),agents 页在壳内显示"未配置"。
    ④ **UI**:smoke/web 新增第四个页签"Agents"——每行
    名称/已安装(detected)/接线开关(失败 setErr 不回滚,与
    插件页同模式)/drift 警告/hint 提示。
    ⑤ **安全口径不变**:接线 = stash 后备改写、断开 = 原样还原
    (agents crate 既有纪律,§8.6);改写只发生在用户点开关时,
    GET 纯读;接线器幂等。
    ⑥ **验证**(已实施 2026-11-04):gateway 集成测试 5 项
    (注入临时路径:四 agent wire→状态→unwire 全往返、用户
    字段保留、drift replaced、未知 agent 400、无 admin key 401、
    端口回退回写)+ agents 注册表测试 5 项(trait 全往返/幂等/
    id 注册表/detected/drift)。**往返测试抓出真实 bug 并修复**:
    claude/opencode/gemini 的 apply 缺幂等守卫,二次 apply 会把
    接线态 stash 成"原值",断开无法还原用户文件——补齐与 codex
    同口径的 `wired()` 早退守卫。UI 经无头浏览器复核:Agents 页
    四行渲染正常,aubridged(临时路径注入)+ 真实页面点 codex
    开关 → 在位 + hint 透出,再点 → config.toml 原样还原。
    不做(明确非目标):CLI/TUI 形态;接线前的 agent 安装检测
    (只检测配置文件存在,不探 PATH)。

29. **CORS 落地修 UI 空壳**(2026-11-04,§6 cors.go 语义):UI 截图
    验收暴露真实 bug——网关守卫放行带 key 的浏览器请求,但响应
    无 `Access-Control-Allow-Origin`,桌面壳/浏览器页跨源 fetch
    读不到响应,三页全显示"网关未就绪 / TypeError: Failed to
    fetch"(守卫与 CORS 各自都对,合起来缺一环)。修复(security.rs
    guard):① 预检 OPTIONS(无 Authorization 头,不能走 key 校验)
    直接 204,按 `access-control-request-headers` 声明回显放行;
    ② 通过守卫的浏览器响应回显 Origin + `Vary: Origin`;③ 拒绝
    响应(403)不回显,不帮恶意站点探测。集成测试
    `cors_preflight_passes_and_origin_echoed_with_key` 三段断言
    (预检 204+回显/带 key 200+回显/无 key 403 不回显)。UI 三页
    经无头浏览器截图复核:仪表盘活数据(分享卡片 + chatgpt 利用率
    0%/78% 实时)/插件页/设置页均正常渲染(178 测试全绿)。

28. **Ledger::open 首次运行自愈 + Codex 反代理真实验收**
    (2026-11-04):① **修首次启动挂死**(验收暴露的真实 bug):
    `~/.aubridge/` 目录不存在时 `Ledger::open` 报"unable to open
    database file",aubridged 直接退出——首次运行体验即挂。修复:
    open 对非 `:memory:` 路径先 `create_dir_all` 父目录(B-7 数据
    目录的 IO 责任归 ledger crate);单测 `open_creates_missing_parent_dir`
    用三层不存在的嵌套目录验证(177 测试全绿)。② **Codex 订阅源
    反代理能力真实验收通过**(scripts/accept-codex.sh,exit=0):
    本地 `~/.codex/auth.json` OAuth 形状齐备(access/refresh/
    account_id,指纹只出 sha256 前 12 位、不打印 token);直测
    `/v1/messages` → Responses 协议翻译 → Anthropic SSE 事件流
    (message_start→…→message_stop,应答源 chatgpt);真实
    `claude -p` 会话经 aubridge → chatgpt.com(经本地代理
    127.0.0.1:7890)完成 Read 工具调用循环并回传文件内容——
    OAuth 凭据读取 + 401 代刷新链路在真实上游验证有效。
    验收全程 share_login 默认关闭(只读),auth.json 未被写回。

27. **router 去重修复 + README 基座**(2026-11-04):① **修候选
    重复追加**(integration.rs 头注释记录的技术债):源同时
    resting+sunk 时,probeOne 段(不查 sunk)与 sink 兜底段
    (不查 resting)各追加一次,候选列表出现重复——网关会对
    同一源连试两次。修复:probeOne 过滤 sunk(sink 语义更"沉",
    该源以 sink 兜底身份出现一次;probeOne 只探未沉底的);
    先加失败用例 `resting_and_sunk_source_appended_once` 再修
    (176 测试全绿)。② **仓库 README.md** 落地(公开 v0.1.0
    前置):§2 立场段原文("发布时原文使用")+ 源码快速上手
    (前端 npm build → aubridged 网关 → smoke 桌面壳 → Claude
    Code 接线 env)+ 文档索引 + 状态段。③ **消暗分歧**:gateway
    集成测试头注释原计划"fixtures 满足第二消费者条件,抽 crate"——
    与 crate-layout §4.5(2026-11-03 codex 校准"砍掉 fixtures
    crate")冲突;按后者裁定改为 include_str 相对路径跨 crate
    引用,不建 crate,头注释已同步。

26. **M4 React 前端工程化落地**(2026-11-04,§5 前端栈
    React+TS+Vite):① **工程**:`crates/smoke/web/` Vite 工程
    (React 19 + TypeScript strict),`vite build` 产物直落
    `crates/smoke/ui/`(tauri.conf.json `frontendDist` 不变,
    新增 `beforeBuildCommand: npm run build`——tauri-cli v2 自动
    检测 web/package.json 为前端根并在此目录执行 hook;
    cargo 侧无需知道前端工具链,`generate_context!` 照常读 ui/。
    环境要求 tauri-cli **v2**(v1 CLI 读不懂 v2 conf,已从 1.6.6
    升到 2.12.4)。
    ② **组件化**:原 240 行手写 index.html 拆为
    App(页签)/Dashboard/Plugins/Settings 组件 + `api.ts`
    (fetch 封装与线类型)+ `format.ts`;行为逐一保真:零遥测
    (§2,数据全部来自本机网关,页面无外部请求)、开关失败回滚、
    风险披露常驻、stash 备份显示与一键还原、invalid 插件校验
    详情;样式原样迁移(深色主题)。③ **构建纪律**:
    `web/node_modules/` 与 `crates/smoke/ui/`(构建产物)入
    .gitignore;**新 clone / CI 编译 smoke 前必须先在
    `crates/smoke/web/` 下 `npm install && npm run build`**
    (frontendDist 缺失时 `generate_context!` 编译失败)——
    CI 门禁需加前端构建步。
    验证:`tsc` 零错误、`cargo tauri build` 全链路
    (npm build → cargo release → bundle dmg/app 4.8/12.1 MiB)、
    `aubridge smoke: PASS`、workspace fmt/clippy 零警告、
    175 测试全绿。

25. **§7 写回 opt-in 落地:订阅写回 UI**(2026-11-04):M2 里程碑
    剩余代码项收口。① **行为对齐(修偏差)**:ChatGptChannel 刷新
    原先无条件写回 `~/.codex/auth.json`,偏离 §7"默认只读"——收敛
    到 `share_login` 开关后面;关闭(默认)时刷新产物只更新内存
    缓存,用户 CLI 文件分毫不动(401 兜底刷新与重试不受影响)。
    ② **开关三件套**:`credentials::settings`(config.json,
    `{"share_login":bool}`,字段级合并 + temp+rename 原子写——config
    是共享文档,端口字段由 aubridged 写入,互不覆盖);
    `credentials::stash`(开启时先完整备份 auth.json 进
    `~/.aubridge/stash/`,mtime + sha256 元信息;一键还原 = 校验
    sha256 后原子写回 + 内存缓存失效重读);风险披露文案单点常量
    (§7:"同 token 双客户端并发使用可能触发厂商异常检测")。
    ③ **admin 面**:`GET/POST /admin/settings` + `POST
    /admin/settings/restore`(admin key 空间,§6);开启时备份失败
    则开关不动——不留"无备份却改用户文件"的窗口。④ **壳 UI**:
    smoke 第三页"设置"(开关 + 风险披露 + 备份状态 + 一键还原)。
    ⑤ **§5 清单外新依赖** `sha2`(RustCrypto,纯 Rust):stash 完整性
    校验 + §7 凭据指纹既定口径(sha256(access+refresh))的实现基座。
    ⑥ **已知局限(记入,不静默)**:开关关闭时刷新产物仅存内存,
    进程重启后重读盘上旧 token;若刷新发生 refresh token 轮换而未写
    回,盘上旧值可能已被厂商作废,需重新 codex login——§7 原文
    "只存 aubridge 自己的 keyring"待 keyring 接入(用户门控项)后
    消除。credentials 34 测试 + gateway 25 集成测试(默认只读不动盘/
    开启即备份 + 写回/一键还原/admin 鉴权)全绿。

24. **§8.1/§8.4 router 运行态 write-behind 持久化落地**(2026-11-04,
engineering-constraints B-8 M2 语义收口):① ledger 新增 `router_state`
表((kind,id) 主键,json 行),`save_router_state`/`load_router_state`
两个 API——provider(§8.1 rest/sink/爬梯/熔断/利用率)与 stick(§8.4
亲和表)同库持久化;坏 JSON 行读侧忽略(持久化是尽力语义,不拖垮
转发路径);载入按当前注册源 id 过滤——配置里删掉的源不复活,新源
走干净默认态。② 网关 write-behind:状态变更(apply_failure/success/
decision/reset_sample/breaker/record_utilization/record_affinity/
插件并入)只标脏集,不逐请求写库;`serve()` 起 5s 周期 flush 任务
合并落盘(脏集空 = 空操作),`flush_router_state()` 公开供测试/停机
显式调用;未挂 ledger 的网关(测试/未接线)标脏是空操作,行为与
M1 完全一致。③ `with_ledger` 挂载即载入:只恢复路由决策态,
kind/models 以当前 manifest 为权威(重启间隙改配置不回退);插件
重新并入时标脏覆盖库里的 stale 行。④ `aubridged` 打开
`~/.aubridge/ledger.sqlite`(与 config.json/plugins 同根)挂账本——
月度省 $X 与 router 状态共用一个 SQLite 文件。ledger 7 测试(roundtrip
upsert/坏行忽略/未知 id 剔除)+ gateway 23 集成测试(重启重建网关:
sink/rest/粘性不丢、models 以 manifest 为准)全绿。

23. **§8.2 利用率字段与 codex wham 主动查询落地**(2026-11-04,决策
记录 19 收口):① `ProviderState` 新增 `util_5h`/`util_7d`(百分比
0–100,None = 未知);归一在边界写死——Anthropic unified 头
(`anthropic-ratelimit-unified-5h/7d-utilization`)是 0–1 小数 ×100,
codex `x-codex-*` 头与 wham 体是百分比原样。② 被动采样两条路补齐:
Responses 成功路径把 `x-codex` 窗口头解析升级为浮点(支持 "7.5",
耗尽线 `>= 100.0`)并回写利用率;Anthropic 透传路径新增 unified 头
采样(含 `-5h-reset` 重置时刻,复用只前移语义)。③ 主动查询(第二
数据源):credentials 新增 `WhamFetcher`(`GET
https://chatgpt.com/backend-api/wham/usage`,Bearer = 盘上 access
token,正缓存 180s/负缓存 60s/单飞;结构从宽——无配额槽上游显式回
null 窗口);网关 `GatewayState.wham: Option` 默认不挂(未登录/测试
环境零外呼),组装层 `with_wham_endpoint` 显式接线,仪表盘路由触发
(缓存 3min,频繁刷新不放大外呼)。**v1 口径不变**:主动查询只补
利用率与重置时刻,rest 决策仍由被动采样/失败路径驱动。利用率回写
合并式——None 不覆盖已知值(头缺失 = 未知,不抹上一窗口采样)。
④ 仪表盘 providers JSON 与桌面壳快照表加 `util_5h`/`util_7d` 列
(null 显示"—")。manifest+credentials 29 测试 + gateway 22 集成
测试(codex 浮点采样断言、Anthropic unified 头采样、wham 仪表盘
触发一次 + Bearer 校验 + 缓存命中)全绿。

22. **§8.4 会话亲和落地**(2026-11-04,§8 全子系统收口):① 亲和 key
协议感知三层推导——原生会话头(8 个,x-opencode-session/
x-session-affinity/x-session-id/session_id/session-id/
x-claude-code-session-id/x-conversation-id/
x-deepseek-harness-session-id,优先级即列表序,截断 128 字符)>
Anthropic `metadata.user_id`(proto 新增字段,serde 从宽)> 首条 user
消息内容 FNV-1a 64 指纹兜底(`msg-<hex16>`;内联实现不引散列依赖——
亲和 key 只需跨进程稳定不需密码学强度,一次错粘只损失一个冷窗);
Claude Code 子 agent(`x-claude-code-agent-id`)追加 `|agent:<sub>`
独立后缀,子 agent 粘性不覆盖主会话。② 粘性看数据不猜:回复 usage
的 cache_read + cache_creation ≥ 1024 token 才值一粘(usage 采集挂在
AnthropicLoopGuard——客户端侧 SSE 流唯一收口,透传/翻译两路径一个
覆盖,流末经 AffinitySink 回写);冷窗 Anthropic 1h(prompt cache
TTL)/常规 5min,过冷即失效。③ 重排:亲和源(main 优先于 last,
backToMain——替补答话不改写 main)提到候选最前;亲和源 rest/
sink/熔断 open 时**不抢救**,立即放行迁移(表在源恢复后由回复
自然回流);仅订阅源参与(免费档 cache 命中不比延迟值)。④ 纯函数
层 `router::affinity`(57 router 单测)+ gateway 亲和表(上限 512
满时淘汰最旧)+ 3 集成测试(粘性建立且 scope 到会话、cache 不足
不粘、亲和源 rest 时让位)。M1 内存态(与 rest 同口径,
write-behind 持久化 M2 随 ledger 落 SQLite)。工作区 157 测试
全绿,clippy 0 警告。

21. **§8.5 防卡死护栏与订阅并发槽位落地**(2026-11-04):① tool call
循环护栏——会话尾部同一 tool call(name + input 序列化)连续 8 轮
(轮间只隔 tool_result)→ gateway 本地作答完整 assistant SSE
(end_turn,usage 零),不转发上游(止损线,magpie 308 次读同文件
烧 537 credits 事故);纯函数 `router::guard::tool_loop_detected`,
作答文本不指名 aubridge/厂商(注入纪律)。② 订阅源并发槽位——
每订阅账号 `Semaphore(2)` 定死(sub2api 槽位语义,防单账号被打爆
触发厂商风控),permit 整请求持有、流式期由流包装器扛到流结束
(透传 PermitStream / 翻译 TranslateStream),免费源不限;③ 文本
重复循环切断——输出侧包装器提取 text/thinking delta 喂
`router::guard::TextLoopDetector`(尾部最小周期 ≥8 字符、重复
≥ max(5·p, 48) 字符即命中;短周期排除 markdown 分隔线/空白缩进/
两词短循环的误伤面),命中即停转发并补完整 error 终止事件
(F-5 口径,不裸断流),透传与翻译两路径统一覆盖(对客户端都是
Anthropic SSE,一个包装覆盖两处)。router 42 + gateway 17 集成测试
(循环护栏本地作答零上游调用、槽位第 3 并发请求等待、重复流被
切断且后续帧不到达)。工作区 139 测试全绿,clippy 0 警告。

20. **5xx/网络失败熔断器落地**(2026-11-04,§8.1 熔断行):参数与
社区惯例同值(model-router/CLIProxyAPI)——closed 期 5xx/408/网络
失败连续 5 败 → open 30s(open 期排除出候选,含 probeOne 与 sink
兜底);open_until 过期 = half-open 放行试探,2 连成恢复 closed,
半开失败立即重开。计数口径:408/500..528/530..=599(与 classify
状态码臂同口径;529 不计——自冷却+不下架);网络失败计数(plain
路径 + Responses 通道细分 Network/Unavailable,凭据不可用不算源
健康问题不计);成功响应恢复计数。状态落 `ProviderState.breaker`
(serde default,旧快照兼容),纯函数层 `router::breaker`,
gateway 接线 + 仪表盘 `breaker_open` 字段,router 30 + gateway 13
集成测试(5×500 后源被排除、SSE 步不被消费)。工作区 123 测试
全绿,clippy 0 警告。

19. **Codex/ChatGPT 订阅转 API 设计定稿**(2026-11-03,设计入库、
代码未落地):经真实 `~/.codex/auth.json` 冒烟(HTTP 200,gpt-6.1-sol
流式)验证可行后,完成 sub2api + magpie 双向深度调研,设计入库——
① 范围澄清:§3 的"Responses 协议放 v1.1"指**对外协议面**;Codex
订阅源需要的是**上游方言**(入面仍只有 Anthropic,见 §6.3),不违背
该非目标;② 翻译层按 §6.2 IR 决策顺势重构,新增 Responses 两侧
(§6.3);③ §7 Codex 登录导入细节按 magpie `codex_store.go` 实证补齐
(只导入不代登录、原子写回语义);④ 明确不抄 sub2api 的多账号池/
指纹伪装/按租户 session 隔离(§3 反目标),不注入 Codex CLI 基础
prompt(实测不注入亦 200);⑤ engineering-constraints #11 代理行为
澄清(reqwest 只读环境变量代理,不读 macOS 系统代理)。实施顺序
见 §6.3 末。

18. **kimi-code 真实验收通过 + TLS 缺失修复**(2026-11-03):kimi-code
订阅源经真实 key 全链路验收 PASS(Anthropic 透传 + claude -p 真实工具
调用循环,隔离 CLAUDE_CONFIG_DIR,验收脚本 accept-kimi.sh)。修复过程
暴露真 bug:workspace reqwest `default-features = false` 未启用任何 TLS
后端,所有 https 上游连接即失败,而网络失败分支静默吞错(`let _ = e;`)
只剩"no candidate provider succeeded"(集成测试用 http 回放服务器,
从未暴露)——补 `rustls-tls` feature + 网络失败 eprintln 日志;验收脚本
临时 settings 增加 Read 权限允许(非交互会话权限系统默认拦截文件读取)。

17. **环境变量去前缀**(2026-11-03):上游 key 环境变量从 `AUBRIDGE_KEY_<ID>`
改为 `KEY_<ID>`(如 `KEY_ZHIPU`;去掉用户手填变量的厂商前缀),插件目录
变量 `AUBRIDGE_PLUGINS_DIR` → `PLUGINS_DIR`,模板/脚本/网关回退路径同步;
codex 受管块的 env_key 由 `AUBRIDGE_KEY` 改为 `AUBRIDGE_TOKEN`(语义是
网关本地 token,与 `KEY_<ID>` 上游 key 家族区分,保留前缀以免混淆)。
.env 模板中未被任何代码读取的 `AUBRIDGE_TOKEN`/`AUBRIDGE_PORT` 注释项
删除(死配置,sub2api 教训)。

17. **chatgpt/Codex 订阅源验收 + 被动采样落地**(2026-11-04):① §6.3
全链路真实验收 PASS(accept-codex.sh:Responses 翻译 + OAuth 凭据/401
刷新 + claude -p 真实工具调用循环,gpt-6.1-sol);② 限流头被动采样
(§8.2/§8.3)接线:UpstreamSignal 新增 window_exhausted(x-codex
used-percent ≥100 与 quota 词表同桶休到窗口重置),Retry-After/
reset-after-seconds 进失败信号,成功响应采样记录 resets_at(rest 只
前移);③ 范围澄清:v1 被动采样只落 rest/reset 决策,利用率百分比
字段随 codex wham/usage 主动查询(§8.2 第二数据源)一并进 schema,
不为无消费者加字段。118 测试全绿,clippy 0 警告;

16. **M2/M3 UI 收尾 + 协议头透传**(2026-11-03):① Anthropic 透传路径补协议
头——透传客户端 anthropic-beta,缺省注入 anthropic-version: 2023-06-01
(model-router injectAnthropicHeaders 同款;kimi 等端点按 Anthropic 协议校验
该头,缺失必拒);② /admin/dashboard 增加源状态快照(类别/模型/剩余/重置/
rest/sink/健康/启用);③ 插件页数据面与开关面:/admin/plugins 扫描+校验
失败清单+/admin/plugins/{id}/enable 运行时开关(默认全关,开启才进候选,
enabled.json 原子持久化);④ 桌面壳 UI 双页(仪表盘:分享卡片一键复制+源
状态表;插件:开关+校验失败详情),零外部请求;⑤ 修复真 bug:from_manifests
把 kind 硬编码为 FreeTier,订阅源(kimi-code)此前被误当免费源参与 free/
组合语义。82 测试全绿,smoke PASS;

15. **kimi-code 订阅源接入**(M2 扩展,2026-11-03):Anthropic 原生
协议透传(协议/端点/鉴权按 model-router profiles.ts kimi-code 核实:
 + messages + x-api-key),manifest 新增
 与  字段;同协议字节透传零翻译(§6 性能与
保真双优,集成测试验证事件原样到达); 模板 + aubridged 自动
加载(本地 key 不入 git);验收脚本 。

基于对 magpie / sub2api / CLIProxyAPI / model-router / new-api / cc-switch 六个
本地代码库的实地调研(见附录),做以下修订:

1. **进程模型获得先例背书**:cc-switch 3.20.4 已在生产形态实现"Tauri 进程内
tokio+axum 代理"(`src-tauri/src/services/proxy.rs`),同进程架构从"待验证
假设"升级为"已验证路径";
2. **插件架构重定义**(解决 v1 的自相矛盾):v1 插件 = 纯声明式 manifest,
无 WASM 代码执行;wasmtime 推迟到 v2 且仅限"自定义配额探测"一种用途
(§9);
3. **协议翻译保真进入 M1 验收标准**:把 magpie LESSONS.md 七大翻译事故
固化为回归测试用例(§6.1);
4. **路由错误分类矩阵细化**:吸收 sub2api 的 529 语义、401 单飞刷新、
订阅源并发槽位(§8.1);
5. **订阅额度可查询**:Claude OAuth 走 `/api/usage`(sub2api 验证过的路径),
第二梯队排序有了数据源(§8.2);
6. **OAuth 写回降级为 opt-in**:默认只读,写回需显式开启 + 备份 + 一键还原
(§7);
7. **会话亲和范围收窄**:仅对订阅源启用(免费源无 prompt cache,亲和只
有害处)(§8);
8. **"省 $X"口径明确**:对标模型价格 × 用量,未定价记 null 不伪造成本
(model-router 的纪律)(§8.3);
9. **零遥测写进产品立场**(§2);
10. **端口占用回退策略**:port 0 随机分配后持久化(cc-switch 模式)(§4);
11. M1 范围收窄、M3 因插件简化而缩短(§10);
12. **吸收 CLIProxyAPI 深度调研**(v1.1 第二批修订):双层冷却(源级 +
模型级)、冷却持久化、reset 防雪崩 fuzz、翻译器 Registry 组织、
错误兜底日志、别名模型池(§6.2/§7/§8.1);修正事实:Gemini CLI
OAuth 的现役后继是 Antigravity 端点,CLIProxyAPI 无内置免费源预设
(§7/§12);
13. **吸收 sub2api 深度调研**:凭据写回的字段级合并纪律(整体覆盖会
回滚别处刚轮换的 refresh_token,§7);真实 Claude Code 流量透传
不改写 = 天然官方指纹,aubridge 无需 mimicry(§7);usage 查询具体
端点与缓存/抖动纪律(§8.2);上游 400 实战过滤规则(§6.2);
"无 reset 头的 429 不长封"语义(§8.1);
14. **吸收 magpie 深度调研**(v1.1 第三批修订):翻译层改用 IR 中间
表示(§6.2);holdWriter"首字节前才可 failover"铁律(§8);
rest 四桶分类与爬梯持久化(§8.1);凭据"刷新必等完"与按指纹识别
账号(§7);端口 handover 平滑重启替代"空闲才更新"(§4);
亲和按 cache 读写量自动判定(§8.4);防卡死护栏(§8.5);语料纪律
"用上报者原始字节"(§6.1)。

## 1. 意图与使命

**让不买 token 的人也能顺畅地写代码。**

本地运行的桌面客户端,把两类资源汇成一个池,统一代理给本机任意
coding agent 使用:

1. **厂商官方免费档**——各平台免费模型(OpenRouter `:free`、GLM-Flash、
ModelScope 等),单家不够,集成叠加够日常编码;
2. **用户已付费的订阅**——Claude Pro、ChatGPT Plus、各家 Coding Plan
(Kimi Code、GLM Coding Plan、Qwen Coding Plan 等),订阅费已花,
跨 agent 复用不产生额外计费,等于免费。

核心体验承诺:**额度耗尽或被限速时,顺滑迁移到下一个源,agent 感知不到
错误**。用户的用量自动按"免费优先、订阅兜底"消耗。

## 2. 产品立场(README 第一段,发布时原文使用)

> aubridge 只做两件事:把**厂商官方免费档**和**你自己付费的订阅**,统一
> 代理给你本机的任意 agent 使用。所有凭据保存在你自己的设备上,不经过
> 任何第三方服务器;不支持多用户、不支持把额度转发给他人。**零遥测——
> 不上报任何用量、错误或统计,所有数字仅在你的本地生成。**社区免费模型
> 源以插件形式提供,默认全部关闭,由用户自行评估开启。

这段同时是合规自辩的底稿:核心功能 = 官方免费 API + 用户自有订阅的
本机复用;不碰共享、不碰破解、不碰厂商私有协议。

## 3. 目标 / 非目标 / 反目标

**目标**

- 开源,下载应用或源码编译即可自用;
- 白名单核心:大厂免费源 + 个人订阅;社区免费源走插件,默认全关;
- magpie 式路由编排与故障处理(rest / sink / 会话亲和);
- 第一天就有"本月省了 $X"的可分享统计(口径见 §8.3)。

**非目标(v1 明确不做)**

- 多用户、计费、拼车、额度对外转发(sub2api 方向);
- 多账号池(同一源注册 N 个账号叠加额度——CLIProxyAPI 的招牌用法,
属额度池化,灰色,不做);
- TLS 指纹伪装 / 请求头指纹模拟(sub2api 的 `fingerprint.go` 能力,
结构性 ToS 风险,不抄);
- Gemini 原生协议(放 v1.1);OpenAI Responses 协议——**指对外
  协议面**(提供 `/v1/responses` 入口,放 v1.1);Codex 订阅源所需的
  Responses **上游方言**(入面仍是 Anthropic,只翻译出向请求/入向
  SSE)不在此列,见 §6.3;
- 70+ agent 全量接线(v1 只接 4 个)。

**反目标(做了就算失败)**

- 凭据离开用户设备;
- 网关默认暴露给本机之外;
- 主项目内置任何社区中继/灰色免费源;
- 协议翻译丢流、丢 tool call(magpie 的 LESSONS.md 全是这类事故,
已固化为验收用例,见 §6.1)。

## 4. 总体架构

```
┌─────────────────────────────────────────────────────┐
│ Tauri 2 桌面壳(托盘 + 主窗口)                      │
│ 前端: React + TS                                   │
├─────────────────────────────────────────────────────┤
│ Rust 核心(单进程,tokio 多任务)                    │
│                                                     │
│ ┌───────────┐ ┌──────────────┐ ┌────────────┐      │
│ │ gateway   │ │ quota ledger │ │ agent      │      │
│ │ axum 网关 │←─│ 余量/成本账本 │ │ wiring    │      │
│ │ :3427     │ │ (SQLite)     │ │ 配置改写   │      │
│ └─────┬─────┘ └──────╲───────┘ └────────────┘      │
│       │                │                            │
│ ┌─────▼────────────────▼─────────┐ ┌───────────┐   │
│ │ router:候选循环(免费优先)    │ │ credential│   │
│ │ 错误分类/熔断/亲和             │ │ store:    │   │
│ └─────┬──────────────────────────┘ │ OS keyring│   │
│       │                            └───────────┘   │
│ ┌─────▼──────────────┐ ┌────────────────────────┐  │
│ │ provider registry  │ │ plugin: 纯声明式      │  │
│ │ 白名单 presets +   │ │ manifest(无代码执行, │  │
│ │ 订阅导入           │ │ 默认关)              │  │
│ └────────────────────┘ └────────────────────────┘  │
└─────────────────────────────────────────────────────┘
                │ 本机回环,对 agent 暴露
   Claude Code / Codex / OpenCode / Gemini CLI …
```

**进程模型(作者决策)**:网关与 UI 同进程;Tauri 主线程跑事件循环,
tokio runtime 跑网关与全部 IO。

**桌面退出**。托盘可用时，关闭主窗口仅隐藏窗口，网关继续服务。macOS
应用菜单 Quit 或 Cmd+Q 显式退出时，先停止接收新连接，给在途响应最多
5 秒完成，再刷新已结算状态、释放数据目录实例锁并退出；重复退出不提前
打断等待。重启、系统外部终止与强杀不在此有界停机保障内。托盘仍接入
同一退出路径，其实际点击验证边界见 [0062](iterations/0062.md)。

**先例(v1.1 新增)**:cc-switch 3.20.4 已在 `src-tauri/src/services/proxy.rs`
(约 4 万行)中实现进程内 tokio + axum/hyper 代理(含 failover 队列、按
`apiFormat` 的协议转换、全 rustls),并保持托盘/窗口/更新器共存。aubridge
的同进程架构无需 spike 验证,直接照抄其工程结构;仍需在第一周写一个
50 行的 axum-in-tauri 冒烟测试确认事件循环共存。

**端口**。首次尝试 `127.0.0.1:3427`，重启优先复用上次端口。
端口被占用时由系统分配，实际端口持久化并供桌面和 Agent 接线读取。避开
magpie 的 3425 与 cc-switch 的 15721,允许三者共存。

**自动更新与长流**:v1 用"空闲才更新"(无活跃 SSE 连接时才提示安装,
用户可强制跳过)。v1.1 目标是 magpie 的 **handover 模式**(`listen.go`/
`handover.go`):新进程先接管端口,旧进程把手头流跑完再退——重启零断流,
更新不再需要挑时机。

## 5. 技术栈

| 层 | 选择 | 理由 |
|---|---|---|
| 语言/异步 | Rust + tokio | 已定 |
| 桌面壳 | Tauri 2 | cc-switch 已验证同进程代理形态 |
| HTTP 服务 | axum | 生态标准,SSE 支持好,cc-switch 同款 |
| 上游客户端 | reqwest(rustls) | HTTP/2、代理支持 |
| HTTP 日期 | `httpdate` | 解析上游的绝对 `Retry-After` 日期，复用锁文件中的版本 |
| 序列化 | serde + serde_json | 协议翻译层地基 |
| 凭据 | `keyring` crate | 三平台钥匙串统一 API(cc-switch 同款) |
| 管理令牌 | `getrandom` | 由操作系统生成每次运行的随机令牌 |
| 本地存储 | rusqlite(SQLite,WAL) | 账本、插件状态、路由历史(model-router BYOK 同款,零外部依赖) |
| 散列 | sha2(RustCrypto) | stash 完整性校验 + §7 凭据指纹(变更记录 25 引入) |
| 前端 | React + TS + Vite | 与 Tauri 生态最顺 |
| 更新 | tauri-plugin-updater | minisign 签名 + GitHub Releases(cc-switch 同款) |

注:v1.1 移除 wasmtime(见 §9)。

## 6. gateway(协议面)

**已实现的入面包括 Claude Code 的 Anthropic Messages 和 `count_tokens`、OpenCode 的 Chat Completions，以及 Codex CLI 的流式 HTTP Responses。**
Gemini 原生入口放 v1.1。
**上游方言**(入面协议 → 上游另一种协议的翻译)按需增加:Codex
订阅源的 Responses 方言 v1 即做(§6.3),因为它承载订阅兜底这一
核心承诺。Responses 入站可复用此上游通道。

- 路由:`/v1/messages`、`/v1/messages/count_tokens`、
`/v1/chat/completions`、`/v1/responses`、`/v1/models`;
- **同协议字节透传**:入面协议 = 上游协议时不做 JSON 反序列化重建,
直接转发字节流(model-router `bridge.ts` 验证过的性能路径,也天然
消除"透传路径上的翻译 bug");
- Chat Completions 默认返回非流式 JSON。同协议保留原生选项，兼容跨协议
  工具调用与结果。确认上下文超限且切源未能恢复时，返回
  `context_length_exceeded` 供客户端处理。
- Codex CLI 通过 HTTP/SSE 发送完整历史，使用 `stream: true` 和 `store: false`。
  文本、函数、命名空间及纯文本自定义工具可转换到 Chat 或 Messages。
  原生 Responses 保留事件。跨协议时移除原上游加密推理载荷。
  兼容源的 `response.done` 按明确的响应状态转换为标准终态。
  Responses 转译时，JSON 对象缺少顶层 `type` 且 SSE `event` 为已支持标签，
  才补入该类型；显式 `type`（包括无效值）不覆盖，未知标签与原生透传保持原行为。
- `count_tokens` 双路径(magpie L-23 教训):上游原生支持(Anthropic)
则透传;否则本地估算并标记 `estimated: true`。计数对象 = **掩码/翻译
后实际将发送的 prompt**(magpie `countTokens` 语义)。推理路由复用本地
估算，优先尝试容量足够或窗口未知的候选，较小的模型留给最后尝试。
订阅原生计数共用推理槽位，并持有至完整响应读取结束；满槽立即返回
本地估算，不排队、不写推理历史或用量。空闲原生计数和免费源路径不变。
- SSE 细节(magpie `sse.go` 实战参数):读侧 64KB 起步缓冲、多 data 行
合并、注释行也当活性信号;写侧 `X-Accel-Buffering: no`;
**keepalive 注释后不写空行**(openai-go v2/Crush 把空行当事件崩);
完整 SSE 帧仅在出现 `data` 字段时产生数据事件，且每个帧边界清除事件标签。
显式空 `data` 仍由原有 JSON 解析处理。
- Messages 翻译为 Chat 或 Responses 时移除系统内结构化的
  `x-anthropic-billing-header` 归因行，跨协议计数估算采用同一规则。
  原生 Messages 请求保留系统原文。
- Chat 工具分块以原上游索引关联参数，按实际收到的工具分配状态和下游块号。
  首段已有工具 ID 但缺名称时，等待同一索引的完整名称后再按序交付。只保留一个
  待命名工具，限制为 64 KiB 参数和 256 个分片，计入首个和补齐名称的分片。
  身份冲突、越序正文、达到上限后再收到分片或结束时仍缺名称，均按流错误处理。
  Responses 翻译也以收到的输出项为单位，保留原始索引以匹配后续参数和签名。
  工具参数只随最终输出项到达时，在关闭工具块前补发一次。
  当前消息的文本部分仅随 `output_text.done` 到达时，转译输出一次完整文本。
  若此前没有正文、工具或思考内容，完整 `response.completed` 的 output
  全部为 assistant 纯文本时，先整体验证，再按原顺序补发一次，保留终态用量。
  已交付文本不重复或补后缀；混合工具、思考及错误或不完整终态不做全文恢复。
- Messages 明确要求串行工具时，Chat 和 Responses 的翻译请求携带 `parallel_tool_calls: false`。
- 鉴权:本机 token `aubridge`,每 agent `aubridge-<agent>`;
- 安全边界移植 magpie 的已验证清单(实现于其 `internal/gateway/`
的 `allow.go`/`cors.go`/`lan.go`):
  - Origin 只接受已启动桌面壳的内置来源。未知 Origin 和 `Origin: null` 一律拒绝。
  - Host 只接受回环地址或精确的 localhost。无 Origin 的本机 CLI 仍按路由校验凭据。
  - **IP 判定只看 `RemoteAddr`,永不信任 `X-Forwarded-For`**;
    配置读不了时 fail-closed(只放本机);
  - **key/限流规则覆盖所有到达路径**——`count_tokens` 与决策模型
    路径同样要过 key 检查(magpie #882 事故:曾漏掉);
  - 网关默认只监听回环;
  - 管理 API 走独立 `/admin/*` 前缀及当次随机 token。Tauri 通过本地 IPC 注入。

### 6.1 翻译保真回归集(翻译层 = 产品地基)

把 magpie `docs/LESSONS.md` 的事故清单固化为 CI 回归用例,每个用例 =
"含陷阱特征的请求录制 → 断言翻译后事件序列逐事件等价":

| 用例 | 源事故 | 断言 |
|---|---|---|
| F-1 tool_use 增量保序 | L-04(input_json_delta 乱序致 agent 死循环) | 每个 tool_call 的 input_json_delta 严格按序 |
| F-2 usage 全程透传 | L-07(流末 usage 丢失致记账为 0) | message_delta 的 usage 在翻译后完整出现 |
| F-3 thinking 块完整性 | L-11(签名丢失 → 上游 400) | thinking 块整块保留或整块剥离,内容永不修改 |
| F-4 finish_reason 显式映射 | L-14(SAFETY→stop,agent 收空内容当成功) | 非常规 finish 原因显式映射并报错,不默认 stop |
| F-5 mid-stream 失败终止 | L-19(裸断流致 agent 状态错乱) | 上游中途失败必须发出完整 error 终止事件 |
| F-6 count_tokens 降级 | L-23 | 上游不支持时本地估算,不 500 |
| F-7 thinking 签名整流重试 | sub2api / model-router rectifier | 上游报签名无效时剥离 thinking 重试一次,而非直接失败 |
| F-8 孤儿 tool_result 清理 | model-router | 翻译过程中清理无法配对的 tool_result |
| F-9 keepalive 事件形状 | magpie #2169/#2711/#2737(翻译流保活是一整类 flake) | 翻译流中注入的注释/心跳不产生伪事件,流边界事件完整 |
| F-10 空响应/截断 body | magpie #823 | 空 body、截断 body 必须进测试,不 panic 不死循环 |
| F-11 tools/tool_choice 往返 | engineering-constraints A-3 | tools schema 原样映射、tool_choice 四态映射、include_usage 注入、max_tokens 透传 |

测试语料:录制真实 Claude Code 会话(含工具调用循环、长流、中断),
脱敏后入库。**语料纪律(magpie #823):用上报者的原始字节做 fixture,
不自造样本**——真实客户端的长尾格式是自造样本覆盖不了的。
**回归集是 M0/M1 的一级交付物**(见 §10)。

### 6.2 翻译层组织与调试设施

- **IR 中间表示(magpie 模式,v1.1 修订后采用)**:每种协议写
  "解析到 IR / 从 IR 生成"两侧(`Part{text|image|tool_call|
  tool_result|thinking}` 等公共形状),协议组合数增长时是 N 个
  解析器而非 N² 个成对转换器(CLIProxyAPI 的 from→to Registry 制在
  4 协议时是 12 对,v1.1 加 Gemini/Responses 到 6 协议时成对制要
  30 对——IR 制只需 12 侧)。同协议对走透传短路;
- **Chat 缓存用量**：Chat 转 Anthropic 时，分别取已报告的输入总量、输出量和
  `prompt_tokens_details` 中缓存读写计数的累计最大值。普通输入量从输入总量
  扣除缓存读写，最低为零。只有实际报告的缓存字段才会随回复发送；
- **Chat 拒答增量**：Chat 转 Anthropic 时，字符串 `delta.refusal` 按收到的
  顺序原样交付。同一分块按正文、拒答、工具调用的顺序处理。仅含空白的片段
  不打断正在接收的工具参数，也不算有效拒答正文。已交付非空白拒答正文且
  没有工具调用时，正常 `stop` 映射为 `refusal`。其他结束原因和断流行为
  沿用现有映射；
- **改写降级重试循环**(magpie `forwardTranslated`):上游拒绝时按
  逐项回退再试——丢 cache key → 丢 image tool → 去 effort 档位 →
  换思考级别,而不是直接失败;
- **响应模型名回写**:响应体里的真实上游模型名改写回客户端请求的
  别名,保证 agent 看到一致的模型标识(CLIProxyAPI
  `response_model_rewriter.go`);
- **请求日志(调试模式,默认关)**:可 dump 完整请求/响应含 SSE 流
  (脱敏、压缩);关闭时仍保留"仅错误"日志——出错的那次请求自动
  补捕获请求体(CLIProxyAPI `request_logging.go` 模式)。翻译 bug
  的定位效率取决于此;
- **错误脱敏**:转发给 agent 的上游错误先剥离 token/路径等敏感字段
  (CLIProxyAPI `SanitizeUpstreamErrorSummary`);
- **上游 400 实战过滤规则**(sub2api 踩过的坑,转发前清洗请求体):
  空文本块、无签名的 thinking 块、`cache_control` 块超过 4 个、
  web_search 历史遗留的 `server_tool_use` 块——这些真实 CLI 不会发,
  但第三方客户端/翻译层会产生,上游直接 400;
- **注入文本纪律**(magpie LESSONS):任何注入 prompt 的文本不得
  点名 aubridge 或厂商限制——prompt 改动与 header 改动同级对待,
  都可能成为风控信号。

### 6.3 Codex 订阅上游方言(Responses,v1)

magpie/sub2api 双向调研 + 本机真实验证(HTTP 200)的落地设计。
范围:**入面永远是 Anthropic Messages**(§6 v1 决策),新增的只是
"从 IR 生成 Responses 请求"+"解析 Responses SSE 到 IR"两侧——
这正是 §6.2 IR 决策的用例,IR 重构由此触发(原 translate.rs 成对
ChatToAnthropic 拆为 Chat 解析側 + Anthropic 生成側)。

**请求侧强制变换**(上游硬约束,照抄 sub2api,非可选项):

- `stream: true`、`store: false`("Store must be set to false" 否则
  上游报错);
- system 消息搬进 `instructions`(chatgpt.com 端点拒 `role:"system"`);
  **不注入** Codex CLI 基础 prompt——实测 instructions 留空亦 200,
  注入官方客户端 prompt 属冒充客户端文本,边界不干净,不做;
- thinking(thinking 块)存在时加 `include: ["reasoning.encrypted_content"]`,
  加密 reasoning 内容原样透传(F-3 thinking 整块纪律照旧适用);
- Messages 请求显式设为 `thinking.type: enabled` 或 `adaptive`，且
  `output_config.effort` 为 `low`、`medium` 或 `high` 时，转译将该值传给
  `reasoning.effort`。其余组合省略 `reasoning`，包括仅指定思考预算的请求。
- tool 映射:anthropic tools → Responses `tools`(function 类型)、
  tool_choice 映射。

**事件侧翻译**(SSE 事件 → Anthropic 事件状态机,sub2api
`responses_to_anthropic.go` 的映射可移植):

- `response.created` → `message_start`;`response.output_item.added`
  → `content_block_start`;`response.output_text.delta` →
  `content_block_delta`;`function_call_arguments.delta` → 工具入参
  增量(F-1 保序照旧);reasoning summary/text delta → thinking 增量;
- `response.refusal.delta` 的正文按收到的顺序作为文本增量发送。
  正常完成且有非空拒答正文时，停止原因为 `refusal`。已发出工具调用时沿用
  `tool_use`。拒答只使用流式增量，不从终态全文补齐；
- 其他终态沿用现有映射(F-4/F-5)：普通 `response.completed` →
  `message_delta` (`end_turn`)，`incomplete` 使用对应原因，
  `failed` 与流截断使用完整 error 终止事件；
- usage 换算(sub2api 口径):`input = responses.input − cached −
  cache_creation`,cached / cache_creation 分别进
  `cache_read_input_tokens` / `cache_creation_input_tokens`(F-2
  usage 透传照旧);
- **F-12(新用例)**:Responses 事件序列保真(含 reasoning 与工具
  调用混合流、`incomplete`/`failed` 终态、usage 换算);fixture 用
  真实 codex 响应字节录制(语料纪律 §6.1)。

**网关接线**:

- 端点 `https://chatgpt.com/backend-api/codex/responses`;头:
  `Authorization: Bearer <access_token>`、`chatgpt-account-id`、
  `originator: codex_cli_rs`、对齐 Codex CLI 的 User-Agent
  (§7 一致性铁律,与 kimi 透传的"UA 透传或对齐"同款)、
  `OpenAI-Beta: responses=experimental`;
- 401 → 刷新一次重试(仅一次,§7 401 语义细分);
- 429 及 `x-codex-primary/secondary-used-percent` 等头 → 被动采样
  回写 rest 状态(5h/7d 双窗口语义进 §8.1 分类,§8.2 已规划);
- 流式走 TranslateStream 同款管道,上游解析器换 Responses;
- **模型清单按用户套餐实测枚举**,不抄 sub2api 的全家桶别名映射表
  (那是多模型家族长尾;aubridge 单用户直连,写死最小集——
  sub2api 教训:正确行为写死,不做配置面)。

**明确不抄**(sub2api 多租户池的生存技巧,与 §3 反目标/单用户
定位冲突):多账号池调度、`x-codex-installation-id`/`window-id`
分阶段指纹、按租户 session_id 隔离、instructions 基础 prompt 注入。

**实施顺序**(每步可验证):① IR 重构 translate,F-1~F-11 全绿
(纯重构行为不变)→ ② proto Responses 线类型 + Responses→Anthropic
翻译 + F-12 → ③ credentials ChatGPT 通道(§7)→ ④ registry
`chatgpt` preset + gateway Responses 分支 → ⑤ 真实验收
`accept-codex.sh`(仿 accept-kimi.sh)。

**网络部署事实**(本机):chatgpt.com 直连被 DNS 污染,须走本地
代理;reqwest 只读环境变量代理(engineering-constraints #11 澄清),
用户在 `.env` 设 `HTTPS_PROXY` 即可,零代码。

## 7. provider registry(源注册表)

两类条目共用一个数据模型:

```rust
struct Provider {
    id: String,              // "openrouter", "zhipu", "kimi-plan"
    kind: ProviderKind,      // FreeTier | Subscription
    endpoints: Endpoints,    // chat/messages URL, auth 方式
    free_models: Vec<ModelId>,
    quota: QuotaSource,      // Queryable(uri) | Inferred(429/headers) | UsageApi
    resting: RestPolicy,     // 重置时间怎么算
}
```

**首批免费源白名单(作者决策,6 家)**(名单与 magpie 内置 preset 清单
同源,其实现可参考):

| 源 | 免费内容 | 额度获取方式 |
|---|---|---|
| OpenRouter | 内置回退清单与可刷新目录中的零价 `:free` 文本模型 | 免费请求状态从 429 推断，`GET /api/v1/key` 单独读取美元预算 |
| 智谱 / Z.ai | GLM-4.5/4.6v/4.7-Flash | 推断:429/响应头 |
| ModelScope | Qwen3-235B、Qwen3-Coder-30B、GLM-4.5/4.6 等 7 个 | 每日免费额度,推断为主 |
| SiliconFlow | Qwen3-8B/3.5-4B 等小模型 | 推断 |
| NVIDIA NIM | 52 个模型(DeepSeek-V4.1-Flash、gemma 系) | 账号积分,待验证;M1 前无法验证则降级为插件示例,不进 v1 |
| HuggingFace Router | GLM-4.7-Flash | 推断 |

已保存的自定义 Chat 来源可单独读取其 `/models` 目录，使用当前连接与凭据，
最多读取 2 MiB 和 1000 个去重模型 ID，总时限 10 秒。选择的 ID 先加入编辑草稿，
显式保存后才接入模型清单；目录读取不推断费用或能力，也不删除原有模型。
读取期间连接变化则结果作废，错误仅返回固定类别与文案。

OpenRouter 的目录通过 Provider 页按需刷新。网关从公开目录读取模型 ID、
价格、模态、工具能力和上下文长度，保存最近一次有效快照。目录至少要有一条带
非空 ID 的记录，其输入和输出价格均为可解析的有限数字字符串，且均无负号。
刷新失败时保留快照。只有付费模型的有效目录仍可发布空免费清单。免费模型的各项
费用仍按精确的零价格字符串判断。`discount` 还兼容表示无折扣的 JSON 整数
`0`，不用于折算费用。已下架的模型在成功刷新后退出候选，已保存分组继续可编辑，
订阅候选仍可兜底。

内置 OpenRouter 出站请求固定使用网关选中的 `:free` 模型，并设置零价上限。
模型回退、preset 和服务等级由网关约束。普通 Chat 请求和客户端函数工具仍可用。
需要托管工具、启用插件或文件处理的请求跳过该免费源，代理页显示原因。

海外 B 类放 v1.1。**v1.1 特别备注**:CLIProxyAPI 验证了 **Gemini
OAuth 免费档**(个人 Google 账号 OAuth,免费高限额,无需 API key)是
白名单外最大的免费源。注意其现役形态是 **Antigravity 端点**
(`cloudcode-pa.googleapis.com`,Gemini CLI OAuth 的后继,CLIProxyAPI
`internal/auth/antigravity/`);国内可达性是引入 v1.1 的唯一阻碍,
按用户网络环境评估,连同设备码/OAuth 登录流程(桌面端 = Tauri 窗口
开 OAuth 页 + 回调)。另注:CLIProxyAPI 并无内置免费源预设,其免费
用法靠 `openai-compatibility` 任意 base-url + **别名模型池**(多个
真实模型共用一个别名,池内轮换、失败自动换)表达——后者与 aubridge
`free/<model>` 组合模型语义同构,v1 直接采用。

**订阅导入(C 类)**:

- 读 Claude Code 登录:`~/.claude/.credentials.json` + keychain
(macOS 先 keychain 后文件;移植 magpie `claude_dirs.go` 读取语义:
解析 `claudeAiOauth` 的 accessToken/refreshToken/expiresAt);
- 读 Codex 登录:`~/.codex/auth.json` + keychain(尊重
  `cli_auth_credentials_store` 配置;移植 magpie `codex_store.go`)。
  **只导入不代登录**(magpie 同策略):登录由用户跑 `codex login`,
  aubridge 读凭据 + 代刷新,登录态失效时提示“请先 codex login”。
  `auth.json` 形状(magpie 实证):`tokens{id_token, access_token,
  refresh_token, account_id}` + `auth_mode`;account_id 缺省时从
  access token JWT 的 `https://api.openai.com/auth` claim 读
  `chatgpt_account_id`(仅解 payload 不验签);
  文件型登录在取凭据和刷新返回时重新核对。退出或切换登录会更新候选资格，
  文件暂时无法解析时先避让，恢复同一登录后可继续使用只读模式已轮换的内存凭据。
- **OpenCode 登录读取（2026-11-13 起实施，已交付）**：首轮仅读取 XDG data/opencode/auth.json
  的 `openai` / `type:oauth`，复用内置 ChatGPT 固定 Responses 目标；同时将 `openrouter` API key
  作为内置官方 OpenRouter 的只读末级凭据来源（已保存key > 环境key > OpenCode）。ChatGPT 登录来源
  显式选择 Codex（缺省）或 OpenCode；每次请求跟随所选文件更新，失败/注销不跨来源回退。
  OpenCode 的 expires 是毫秒，支持 opaque access；刷新只保存在本应用内，现有共享登录
  开关仅适用于 Codex，不得把 OpenCode 刷新写入 Codex。其他API映射与OAuth后置。
  自动路径沿OpenCode/Magpie的XDG data规则，不需要用户填写路径；Magpie只做API导入，
  本次OAuth与每请求文件热更是新增能力。
  schema、来源/刷新/只读边界见 [OpenCode 登录自动读取](opencode-auth-design-2026-11-13.md)。
  386 项回归、UI、同进程热更/注销和真实 OpenCode 1.18.5 客户端调用已验收；
  真实上游使用已有 Codex access 的私有 OC 格式兼容副本，原生 OC 登录文件未发现，
  不据此宣称原生登录/真实续期已验。安装包同步恢复 reqwest 官方 system-proxy 支持，
  可使用本机已配置的静态系统代理，不改用户系统设置。完整命令、首败与最终包身份见
  [OpenCode 鉴权接入验收](opencode-auth-acceptance-2026-11-13.md)。
- OAuth 常量与流程可直接复刻(sub2api `internal/pkg/oauth/oauth.go`
  已验证):Claude PKCE S256 + 官方 CLI client_id + token 端点
  `platform.claude.com/v1/oauth/token`;Codex 官方 client_id
  `app_EMoamEEZ73f0CkXaXp7hrann` + token 端点
  `auth.openai.com/oauth/token` + `localhost:1455/auth/callback` 回调。
  桌面端交互 = Tauri 窗口开 OAuth 页 + 本机回调;
- Codex token 刷新：依据 **access token** 的 `exp`，余量 < 5 分钟或 401 触发；
  ID token 的身份生命周期不用于判断 access token 是否需刷新。无法解析 access exp 时
  不主动刷新，保留原 401 兜底；最小修正与验收见
  [Codex 本地登录刷新依据修正](codex-auth-refresh-2026-11-12.md)。
  轮换出的新 token 写回 auth.json 使用私有临时文件和原子替换，跨进程文件锁互斥。
  还原在同一写入互斥边界内替换文件并失效登录缓存，避免与刷新提交交错。
  Unix 上的写回、备份和还原文件均使用 `0600`；
- **不做** sub2api 的完整 mimicry 体系(TLS 指纹、每账号指纹缓存、
metadata.user_id 重写,§3 反目标)。aubridge 有结构性优势:**agent
本身就是官方客户端**——Claude Code 发来的请求头(UA / anthropic-beta /
X-Stainless / metadata.user_id)天然自洽,**透传即合法指纹,且不改写
body 保住 prompt cache 前缀**(sub2api 对真实 CC 流量同样不改写,
其 `gateway_forward.go` 的判定语义可参考)。仅当非 Claude Code 客户端
走 Claude 订阅时做最小 header 对齐:beta 集合带上
`oauth-2025-04-20`——缺它会被降级到第三方额度而非订阅额度;
- 一致性铁律(sub2api 教训,单机同样成立):UA / beta 头 / body 里的
版本字段 / session id 头四者必须同源自洽,一次请求取一次值;
- Coding Plan(Kimi/GLM/Qwen 订阅 key)= 普通 preset + key 输入;
- 401 语义细分:expired(刷新一次重试;同 refresh token 指纹的并发
刷新单飞合并)→ 失败则 revoked(标记失效,UI 显示,不再发送);
刷新服务带失败冷却 + 临时下架 10 分钟(sub2api `token_refresh.go`
模式);
- **刷新必等完**(magpie 铁律,OpenAI 轮换 refresh token):刷新请求
一旦发出,即使发起方取消也要等响应完成(30s 不可取消窗口)——
丢弃应答会让手里的旧 refresh token 已被作废(refresh_token_reused);
- **账号身份永远按凭据指纹**(sha256(access+refresh)),不按显示名
(magpie 事故:按显示名删账号,把同邮箱两个 Team 座位的凭据一起删了);
- 备选形态(magpie 模式,若 opt-in 写回事故率高则切换):**谁的登录
谁刷新**——让用户自己的 CLI 在隔离配置目录里跑并自刷,aubridge 只
回读,永不代刷。v1 先做"代刷 + opt-in 写回",此形态记为逃生舱;
- **写回原存储(降级为 opt-in,v1.1 修订)**:
  - 默认**只读**:刷新得到的 access token 只存 aubridge 自己的
    keyring,不动用户 CLI 的凭据文件;
  - "与 CLI 共享登录"为显式开关;开启时先完整备份(stash 语义,
    mtime + hash),temp-and-rename 原子写保留其它字段,UI 提供
    一键还原;
    同一次开启期间的重复请求保留首次备份。重启会核验备份，校验失败则以只读启动。
  - **字段级合并,绝不整体覆盖**:sub2api 记录的事故——用快照回写
    credentials 会把用户 CLI 刚轮换出的新 refresh_token 回滚成旧值,
    账号直接死亡。写回必须"重新读盘 → 只改目标字段 → 原子落盘",
    任何时刻不基于内存快照整文件覆盖;
  - 风险披露:同 token 双客户端并发使用可能触发厂商异常检测,
    开关旁明示。

## 8. router(免费优先候选循环)——产品的心脏

每请求决策管线:

```
请求 → 模型解析(直接 id / free/<model> 组)
     → 生成候选序列:
         第 1 梯队:免费池中供应此模型、当日剩余量 > 0 的源
                    按 (剩余量 desc, 重置时间近的先) 排序
         第 2 梯队:用户订阅内供应此模型的源
                    按 (订阅余量 desc) 排序(§8.2 数据源)
     → 逐个尝试(错误分类见 §8.1):
         失败/429 → rest(记到该源的重置时刻)→ 下一个
         429 且仍有配额 → sink(沉底,兜底再试)
     → 成功:流式转发,响应头 X-Aubridge-Provider 标明谁应答
     → 会话亲和(仅订阅源,§8.4)
```

组合模型以「源 + 模型」生成候选。模型级限流、额度耗尽或不可用只冷却
对应模型。账号级额度耗尽会跳过该源的其他候选，包括本次请求已经排进
队列的模型。冷却状态随现有路由状态写入 SQLite，期限届满后恢复候选资格。
静态 key 收到 HTTP 401 后，同一连接的模型在进程内避让 60 秒。保存或清除 key 后立即重新评估该源。
路由快照批量写入失败时保留待保存项，由下一次刷新重试。
新建自定义源前清理同 ID 的旧路由快照。此前的请求记录和用量仍可查询。
删除自定义源先清理本机钥匙串，再提交配置。后一步失败时，界面会显示清理结果和当前源状态。
编辑或删除自定义源前，若当前有效 Claude Code/Codex 受管选择的声明供应
将从有变无，则在清理 Key 或写配置前拒绝；同名备源或组内仍有合法供应时
允许，free_only 按编辑后的源类别判断。不自动改写客户端模型，自动别名、
原本无供应的引用及临时暂停/冷却不参与此结构性保护。
免费优先组有订阅候选时，在四次尝试的上限内为订阅保留一次兜底。
单次尝试等待首段最多 30 秒，总等待最多 60 秒。免费优先组有订阅候选时，
最后 15 秒留给订阅。非流式请求在响应交付前也受总预算限制。
账本记录实际尝试及跳过原因，诊断字段仅含源、模型、状态和时间。
分组的只读路由预览默认按普通文本筛选。工具调用模式复用正式路由的工具能力
筛选，并显示候选与跳过原因。两种预览都不发起推理或推进轮流序号。
预览同时显示读取时的网关运行状态，停机时仍可查看恢复后的配置候选。
单源手动暂停写入本机配置。暂停期间跳过该源的推理请求，恢复后按现有凭据、健康状态和冷却重新参与路由。Provider 页仍可手动检测连接。
Provider 页按模型展示当前路由状态与已知的可重试时刻。状态按暂停、缺少凭据、
已有路由阻塞的优先级显示，重试时间只属于当前显示的阻塞。这是读取时的快照，
不包含请求工具能力、分组筛选、全局开关或实时探测。

**首段缓冲**参考 magpie 的 `holdWriter`。网关在找到有效正文或工具调用前
暂存流。遇到空流、超时或断流可尝试下一候选。交付首段后，当前流按入站
协议结束并在账本记录结果。
首段后收到可信结构化 402/429 时，沿用既有分类更新后续请求的账号或模型
冷却，并核对实际发送时的连接版本，避免旧错误污染新配置。当前选中流仍以
HTTP 200 的流错误结束、保留已报告部分用量，不切源或重放，也不重复叠加
通用熔断；未知及认证错误维持原处理。
已交付的 SSE 流空闲 5 秒时发送一行保活注释，上游读超时照常结算。
命令行网关收到 Ctrl+C 或 Unix SIGTERM 时关闭监听，让在途响应最多收尾五秒，随后刷新已完成请求的本机结算并移除当次管理会话文件。
Chat 的 `finish_reason: "error"` 是上游失败信号。流式分块和非流式 JSON
中的该信号都遵循相同的交付边界。
原生 Chat 的每个预期 choice 都明确报告结束原因、索引无歧义且上游干净 EOF 时，
网关补发一次 `[DONE]`。结束原因之后到达的用量仍会读取，帧截断继续按流错误处理。
Kimi Coding Plan 形状的结构化 403 明确报告额度窗口耗尽时，按账号额度冷却，
历史保留 HTTP 403。并发拒绝和普通访问拒绝单独处理。

首段前确认上下文超限时，在剩余预算内只尝试能容纳估算输入的已声明窗口。
若失败模型也声明了窗口，下一候选的窗口须更大。跳过原因和估算值记在本地账本。

**patience(组内重试)**:全部候选以"可过"方式失败(408/5xx/短 429,
排除 quota/credit 类)时,倍增暂停轮询重试,预算默认 60s;等待期间用
SSE 注释保活,agent 侧无感(magpie `patience` 语义;保活间隔 3–5s,
10s 会断的中间代理是实测坑)。

从 magpie 移植的三个机制:

1. **rest 按重置时刻而非固定时长**(错误响应里的 reset 信息优先:
   响应体绝对时刻 > Retry-After 相对秒 > 按源预设的重置策略);
2. **sink 区分"真没量"与"被限速"**(429 且有量的沉底,失败不沉;
   sink 与 rest 独立字段,恢复逻辑不同——magpie `rest.go`/`sink.go`);
3. **亲和但源休息时让路**。

默认路由使用免费优先和订阅兜底。命名分组可选择仅免费档、顺序兜底或轮流。仅免费档分组只接纳标记为免费档的源。
用户可在分组编辑器中查看、调整并保存成员顺序。顺序兜底沿该顺序尝试，免费规则仍按免费资格和梯队筛选，轮流在有序候选上应用现有轮次。

### 8.1 错误分类矩阵(吸收 sub2api `ratelimit` 服务 1200 行的语义)

| 上游信号 | 动作 |
|---|---|
| 429 + quota_exceeded / remaining=0 | rest 至重置时刻;真没量 → sink |
| 429 无细节、无 reset 头 | **视为非真实限流**:秒级回避,不长封
(sub2api 语义——这类多为"需开 extra usage"等软限制) |
| 429 且 remaining > 0 | 短 rest(30s),不沉底——被限速不是没量 |
| 429 带 `anthropic-ratelimit-unified-*` 头 | 解析实际触发的窗口
(5h/7d),rest 到该窗口 reset 时刻(sub2api `handle429` 语义) |
| 内置 ChatGPT 通道 HTTP 429，`error.type = usage_limit_reached` 且 `error.resets_at` 为未来 32 天内的整数秒 | 整账号进入额度冷却。已耗尽窗口的重置头优先，否则使用正文时刻 |
| 529 overloaded | 源冷却(30s~10min 可配,sub2api 默认 10min),
**不消耗重试预算、不下架**(换下一个源重试不算失败) |
| 401 expired | 单飞刷新一次重试;刷新 invalid_grant → revoked;
刷新期间该源临时下架 10 分钟,把恢复留给刷新服务 |
| 403 banned/suspended | 源下架 + UI 通知;细分 Cloudflare 拦截 /
风控验证(带验证 URL)/ 封号,分别提示 |
| 5xx | 熔断器计数(5 连败 open,30s 半开,2 成恢复——社区惯例参数,
model-router 与 CLIProxyAPI 同值) |

**双层冷却**(CLIProxyAPI 模式):源级冷却(该源整体 rest)与
模型级冷却(仅该模型 429,源内其它模型仍可用)分开记录;冷却中的
模型仍在 `/v1/models` 列表里,请求返回 429 + retry 时间而非"模型不存在"。
上游封禁解除后，可从 Provider 页手动重新允许路由。该操作只清除上游禁用标记，
继续应用已有的手动暂停、限流冷却和熔断条件。
403 需要明确的账号停用证据才停用整源。工具或请求拒绝及 HTML 拦截只参与当次切源。
当所有具备凭据和请求能力的候选都处于可确认的限流或额度冷却时，推理入口返回 429，
`Retry-After` 按最早可用候选计算。同一候选同时受到账号和模型限制时，以较晚的期限为准。
reset 时刻加 1–30 秒随机 fuzz 防同刻雪崩;冷却状态持久化到 SQLite,
重启不丢。

**rest 四桶分类**(magpie `routing.go` 的 `failure()` 语义,决定
rest 多久):

| 桶 | 触发 | rest 时长 |
|---|---|---|
| credit | 402/余额类字样(含智谱 1113 等) | 爬梯 30m→2h→8h→24h→3d→7d,持久化;带 Retry-After 的只休 30min 不上梯 |
| quota | 配额/窗口耗尽 | 休到 vendor 说的 reset 时刻(兜底 15min) |
| rate | 限速 | 休到 Retry-After,上限 30min |
| other | 其余可 rest 错误 | 1min 起,倍增至 10min |

分类靠状态码 + 多语词表正则(含中文厂商报错;200 里的 error body
也要当真实状态读);**先判断拒绝属于账号、模型还是请求内容,再 rest
账号**(magpie 事故:把"渠道未批准"当账号故障,全部账号休了 5 天)。
rest 中的源排候选末位;每请求只探"rest 结束最早的那一个"
(`probeOne`,避免逐个唤醒);无时间说明的 rest 由后台探活恢复。

重试决策用**结构化返回**(new-api `DecideRelayRetry` 的 `PolicyDecision`
模式:返回 stop 原因/是否换源/是否原位重试,而非裸 bool),配合重试时
逐层放宽优先级(new-api `getPriority(group, model, retry)` 模式)。
候选轮转基于源 ID 而非数字索引(冷却会让候选集收缩,索引轮转会饿死
部分源——CLIProxyAPI `selector.go` 教训)。

健康面:探活 60s 间隔、3 连败标记 unhealthy(model-router/cc-switch
同参数); unhealthy 源不进候选,但仍可被兜底命中。

### 8.2 订阅额度查询(第二梯队的数据源)

**Claude 订阅,两条路都要**(sub2api 验证过的实现细节):

1. **主动查询**:`GET api.anthropic.com/api/oauth/usage`(Bearer
   access token + `anthropic-beta: oauth-2025-04-20`),拿
   `five_hour{utilization, resets_at}` 与 `seven_day` 窗口。纪律:
   缓存 3 分钟、错误负缓存 1 分钟、单飞防击穿、0~800ms 随机 jitter
   打散。仅 OAuth 登录可查(API key 与 setup token 不支持);
2. **被动采样**:每个成功响应的 `anthropic-ratelimit-unified-5h/7d-*`
   头即时写入源状态(利用率 + reset 时刻),窗口重置时清零——
   零成本、覆盖 100% 请求,是 rest 决策的第一数据源。

**Codex/ChatGPT 订阅**:`GET chatgpt.com/backend-api/wham/usage` 拿
primary/secondary 窗口 used_percent + reset_at(sub2api
`openai_quota_service.go` 路径);每个成功响应的
`x-codex-primary/secondary-used-percent`/`-reset-after-seconds`/
`-window-minutes` 头被动采样(primary=7d,secondary=5h,sub2api
归一化口径)。查不到的源按推断处理。

### 8.3 quota ledger(账本与"省了 $X")

- SQLite,每请求一行:时间、agent、源、模型、输入/输出 token、
  cache token、错误码、记账状态;
- 请求历史以单调时钟记录首段就绪和总耗时。尝试序列记录各次尝试耗时，旧记录可保留空值。
- Agents 仅统计最近请求窗口内的成功结果。按历史记录中唯一选中且与实际源匹配的尝试类别，分别计入免费档、订阅或来源未知。
- 代理页的「涉及源 ID」同时匹配已选中、尝试及跳过的源，可与选中源或模型等筛选组合。
- 历史查询或逐行读取失败时明确标为不可用，不伪装空页或静默丢行；真实空结果仍正常显示。后续页失败保留已成功页与原游标，重试失败页不跳页。历史可用性与月度金额、源状态及网关控制独立。
- 本机管理页可清空已完成请求的明细。用量继续独立记账，进行中的请求完成后照常写入历史。
- **记账状态机**(model-router 纪律):reported(上游返回了 usage)/
  partial / estimated(count_tokens 与翻译场景的估算)——账本数字
  可信度自描述;
- **价格口径**:models.dev 数据驱动同步;**未定价模型记 null,
  不伪造成本**;
- **"省了 $X"口径(发布口径,写在卡片角落)**:
  `免费与订阅用量 × 对标模型单价`。对标表 = 每个免费模型映射到
  同能力档的付费模型(如 GLM-Flash 免费 → GLM-4.6 付费单价;
  无同类时用 Claude Sonnet 对标并注明)。订阅用量按"若走 API 计费
  会花多少"计算;
- 月度对标金额按已报告用量累计。部分记录仅计入有价格和已知数量的一侧，另一侧继续在账本中保留为空。
- 月度金额、源排名与分享卡取自同一次成功聚合。空账本的真实零正常显示；账本未配置或读取失败时显示统计暂不可用、禁用分享，不以零或旧分享卡替代，其他诊断仍可读取。
- 429 的 `Retry-After` / `x-ratelimit-*` 头即时写入,驱动 rest 决策;
- 分享卡片:"本月省了 $34.7 · 前 3 功臣:ModelScope / GLM-Flash /
  你的 Kimi 订阅(对标口径,明细可查)"。

### 8.4 会话亲和(v1.1 收窄:仅订阅源)

免费源(OpenRouter `:free` 等)普遍不支持 prompt cache,亲和粘在
免费源只带来延迟劣势。因此:

- 亲和 key:协议感知的会话标识(magpie 语义:原生会话头 / Anthropic
  `metadata.user_id` / 首条消息 hash 兜底),**Claude Code 子 agent
  追加独立后缀**——避免子 agent 覆盖主会话的粘性;
- **仅订阅源参与亲和**;粘性的依据是**回复 usage 里的 cache_read +
  cache_write 实际量**(≥ 1024 token 才值得粘)与冷窗(Anthropic
  1h 缓存 / 常规 5min)——不猜,看数据(magpie Auto 模式语义);
  Chat 和 Responses 的嵌套缓存写入量与顶层别名、重复快照取最大值。
- 普通模型、自动路由和免费优先分组可按亲和重排订阅候选。顺序和轮流分组采用各自的排序。
- 亲和表持久化(write-behind 合并写,重启不丢"谁答过");
- 亲和源 rest 时立即放行迁移,亲和表在源恢复后自然回流;
- 替补答过话的会话,主力源恢复后交回(magpie `backToMain`)。

### 8.5 订阅源并发槽位与防卡死护栏

每订阅账号独立并发上限(默认 2),防止单账号被打爆触发厂商风控
(sub2api 账号槽位语义)。免费源不需要(厂商免费档本来就会限)。
推理请求按原顺序尝试现有合格候选，满槽的订阅暂时延后；就绪候选
均失败或被跳过后，按原顺序重访延后项，每项至多一次。重访从当时起
给等待槽位和首段合计最多 30 秒，仍受请求原有 60 秒总截止时间约束；
仅一源或全部满槽时保留等待。未进入发送流程的满槽或失效候选只记跳过，
不消耗上游尝试次数、不触发冷却或熔断。免费优先及订阅的 15 秒与一次
尝试预留不变，取得槽位后仍复核候选资格与连接状态。
订阅连接检测使用同一槽位。满载时立即报告源繁忙。
连接检测从发出短请求到读完协议终态共用 10 秒预算。首段后的断流和超时分别显示。

**防卡死护栏**(magpie 实战事故的直接产物——曾有 agent 半小时
308 次读同一文件烧掉 537 credits):

- 同一 tool call 连续 8 轮 → aubridge 自己作答"该工具调用疑似循环",
  不再发上游(免费额度产品的止损线);
- 文本/reasoning 的重复循环检测(unigram 循环切断),不让跑到
  max tokens 才停。

### 8.6 agent wiring

v1 支持 4 个:**Claude Code、Codex、OpenCode、Gemini CLI**
(magpie `internal/agents/` 对四家均有成熟改写实现可参考:
Claude Code 走 `~/.claude/settings.json` 的 env,Codex 走
`~/.codex/aubridge.config.toml`,OpenCode 走 `opencode.json`,Gemini CLI 走
`~/.gemini/settings.json`)。

沿用 magpie 的三件套:

- `stash`:改写前备份原值(mtime + hash),断开时原样还原;
  Unix 启动取得实例锁后，会核对 Claude Code 与 OpenCode 的固定备份文件，
  并将可被组或其他用户访问的独立普通文件收紧至 `0600`。此检查不读取或重写
  备份内容，失败时记录文件类别及错误，继续启动网关。
- `applied`:记录 aubridge 写过的字段路径,启动时 diff 检测第三方
  改动(drift)→ UI 警告;
- 原子写:temp + rename,保留用户注释与格式,每写一行留
  `# aubridge:` 标记,卸载不留尸体。

Claude Code 以完整的回环地址和接线 token 识别受管配置。网关启动时若
换了端口，只修复仍指向原受管地址的字段。首次备份保留到断开，断开
预览只列出字段及恢复操作，部分漂移导致地址和凭据无法成对恢复时暂停
写入。关窗收起主窗口。托盘可查看网关状态、暂停新推理请求并恢复，已进入的
请求继续完成。托盘与代理页共用同一运行状态，通过状态通知同步；退出排空开始后
禁用托盘服务操作。显式退出由系统托盘菜单提供。

OpenCode 读取全局 JSONC 或 JSON 配置。初次接线只编辑 `provider.aubridge`，
端口和模型列表同步沿用首次备份。只有在应用内显式选择默认模型时，才备份原顶层
`model` 并写入 `aubridge/<模型ID>`。重复选择保留第一次的原值。断开预览同时
列出已管理模型与 Provider 的恢复操作，确认后一次写入恢复；外部修改任何受管字段
都会阻止整个恢复。配置文件内的其他设置和注释保留。

Codex 使用单独的 `aubridge.config.toml`。客户端显式选择 `--profile aubridge`，
使用回环凭据和 `wire_api = "responses"`。该 profile 可选择代理模型，换端口时
自动更新。默认 `config.toml` 和登录数据由客户端管理。遇到旧版接线，只有受管
文件与首次备份能精确核对时才自动恢复旧配置并建立新 profile。

Agents 高级配置只编辑客户端自己的配置文件。Claude Code 的模型、角色映射、
自定义请求头和 CLI 环境变量写入 `settings.json`；密钥类值读取时打码，
保存时未修改的占位符原样回填，要求填写实际值时才报错。Codex 高级字段只写
受管 profile 顶层，provider 表仍由接线器独占。两端保存都校验读取时返回的
revision，外部修改后旧页面不能覆盖新内容。高级配置不引入网关级自由配置入口。

测试按钮执行后端常量命令，不接受前端参数：Claude Code 使用 `-p`、
`--strict-mcp-config`、`--no-session-persistence` 和文本输出；Codex 使用
`--profile aubridge exec`、`--skip-git-repo-check`、`--ephemeral` 和只读沙箱。
探针在临时目录运行，超时终止，输出截断并脱敏，同一时间只允许一个。测试
使用已保存的客户端配置，未接线、二进制缺失或命令超时会明确报告；测试可能
消耗订阅额度。

删除模型分组前，检查当前有效接线的 Claude Code、受管 Codex profile 与已显式
管理的 OpenCode 默认模型；仍引用该分组时拒绝删除并指出客户端。先切换到其他现有模型或断开
接线后可删除，不自动改写客户端模型。未接线或已被替换的配置不视作受管
引用。OpenCode 目录中单纯列出的模型和外部选择的会话模型不纳入此保护。
当前有效 Claude Code/Codex 接线及已显式管理的 OpenCode 默认选中的既有分组，若编辑成员或规则会使
合法声明供应从有变无，则拒绝保存；已接线客户端显式选择无合法声明供应
的分组也会被拒绝。未选分组和未接线准备配置、旧坏组修复继续允许，
不以临时暂停或冷却判定声明供应，也不扩展为启用接线时的全面检查。

## 9. plugin(默认全关)——v1.1 重定义

**v1 插件 = 纯声明式 manifest(JSON),无任何代码执行。**解决 v1 草案
"声明式 manifest 却要求 WASI socket 过滤"的自相矛盾:

- 插件文件只声明源(base URL、auth 方式、免费模型、quota 语义、
  重置策略)。宿主把声明交给 provider registry,与内置 preset 走
  **完全相同**的处理路径;
- 宿主在配置时检查地址结构，出站读取凭据前用 HTTP 客户端解析主机，
  并复核已声明的 `net` 域名单。base URL 接受 HTTP(S) 和路径前缀，
  拒绝用户信息、查询串及片段。
- **删掉 wasmtime 依赖**:v1 无代码执行 = 最强沙箱;
- v2 再评估:若出现"宿主内置探测器覆盖不了的配额接口",才引入
  WASM 模块且**仅承担配额探测**一种职责,net 权限只用于该探测;
  在那之前 wasmtime 不进依赖树。

```json
{
  "id": "kenari-free",
  "version": "0.3.1",
  "provider": { "base": "https://api.kenari.dev/v1", "auth": "api-key" },
  "free_models": ["deepseek-v4-flash:free", "kimi-k2-6:free"],
  "quota": { "source": "inferred", "rest_hint": "daily-utc" },
  "permissions": { "net": ["api.kenari.dev"], "store_key": true }
}
```

- 默认**全部关闭**,插件页逐个开关,开关即 manifest 校验结果
  (schema 校验 + 域名合法性);
- "自动开启新插件"是独立设置,**默认关**;
- 插件源(仓库 URL)由用户自行添加;官方不运营社区源清单,README
  只写格式不列名单。

**架构约束**。插件只提供声明。安装或开启时校验 schema，出站时由宿主复核目标主机。

## 10. 里程碑

| | 内容 | 交付标准 |
|---|---|---|
| M0(第 1 周)**✅ 已完成(2026-11-03)** | Tauri+axum 同进程冒烟测试;翻译回归集(§6.1 用例 F-1~F-11);录制第一批真实会话语料(语料为 synthetic,真实录制器排 M1) | F-1~F-11 + golden + SSE 往返共 13 测试全绿(clippy 0 警告);Tauri+axum 冒烟 PASS(退出码 0,SSE 翻译回环)。代码:`crates/proto`、`crates/translate`、`crates/smoke` |
| M1(~4 周)**🟡 订阅路径验收已过(2026-11-04)** | 网关双协议 + Claude Code 接线 + 3 家免费源(OpenRouter/智谱/ModelScope)+ rest/sink + 错误分类矩阵 | 已落地(2026-11-03):8 crate、50 测试全绿、smoke 经真实 gateway 流式全链路 PASS(429 自动切源由集成测试验证);**上游转发已流式化**(逐帧翻译即时下发,EOF/读失败按 F-5 补齐终止);凭据注入通道就位(`KEY_<ID>` 环境变量 / 构造注入,M2 接 keyring)。Claude Code 真实会话验收:免费档路径待 KEY_ZHIPU(用户操作);订阅路径(kimi-code,Anthropic 透传 + 工具调用循环)已于 2026-11-03 真实 key 验收 PASS;chatgpt/Codex 订阅路径(§6.3 Responses 翻译 + OAuth + 工具调用循环)已于 2026-11-04 真实验收 PASS(accept-codex.sh,gpt-6.1-sol);限流头被动采样(§8.2/§8.3 x-codex/Retry-After)已接线;熔断器已接线(§8.1:5xx/408/网络失败 5 连败 open 30s、半开 2 成恢复,2026-11-04,仪表盘 breaker_open 字段);§8.5 防卡死护栏与订阅并发槽位已接线(2026-11-04:tool call 循环 8 轮本地作答、订阅源每账号并发 2、文本重复循环切断);**§8 全子系统已收口**——§8.4 会话亲和已接线(2026-11-04:会话头/metadata.user_id/首条 user 指纹三层 key + 子 agent 独立后缀、cache ≥1024 才粘、冷窗 1h/5min、backToMain、rest 放行迁移、表上限 512 淘汰最旧,M1 内存态) |
| M2(+3 周)**🟡 数据面+UI+持久化 已落地(2026-11-04)** | 订阅导入(只读默认 + opt-in 写回)+ `/api/usage` 额度查询 + Coding Plan presets + 账本仪表盘 + 分享卡片 + 其余 3 agent 接线 | 已落地(2026-11-03):ledger ✅(SQLite/WAL/记账状态机/null-cost/月度省 $X 聚合,3 测试)、credentials ✅(KeyStore trait + OS keyring 实现,M2 第一版 key 通道)。OAuth 登录读取 ✅(claudeAiOauth/tokens 解析,keyring 模式降级,只读纪律,3 测试);token 刷新 ✅(刷新必等完/invalid_grant→revoked/只读纪律)、usage 查询解析 ✅(five_hour/seven_day 窗口 + 被动采样头,4+3 测试);opt-in 写回 ✅(字段级合并/并发轮换冲突放弃/空 refresh 保留原值,3 测试);usage HTTP 拉取 ✅(缓存 3min/负缓存 1min/单飞共享,2 测试);仪表盘数据面 ✅(/admin/dashboard:月度省 $X + 功臣榜 + 分享卡片生成,admin key 独立空间隔离,3+1 测试);桌面壳仪表盘页 ✅(分享卡片复制 + 源状态快照表);router 运行态 write-behind 持久化 ✅(2026-11-04,变更记录 24:ledger router_state 表 + 5s 周期合并落盘 + 重启载入——§8.1 冷却/爬梯/熔断与 §8.4 亲和表重启不丢,aubridged 已挂 ~/.aubridge/ledger.sqlite);利用率字段 + wham 主动查询 ✅(2026-11-04,变更记录 23);订阅写回 UI ✅(2026-11-04,变更记录 25:share_login opt-in 开关 + stash 备份/一键还原 + 设置页 + 风险披露;刷新写回收敛到开关后,默认只读);剩余:Claude OAuth 真实登录验收(用户操作;kimi-code 订阅源已验,见 M1 行) |
| M3(+2 周)**🟡 核心+UI 已落地(2026-11-03)** | 插件系统(manifest 校验 + 插件页 + 自动开启设置)+ 端口回退 + 更新空闲策略 | 核心逻辑 ✅:manifest 校验与域名单强制(manifest crate,M1)+ 插件目录扫描/逐条可读错误/默认全关/开关持久化(registry::plugins,2 测试)。插件页 UI ✅(开关+校验失败详情,2 集成测试);端口回退 ✅(3427 占用 → port 0 随机 + on_bound 回调持久化到 ~/.aubridge/config.json,1 集成测试);自动开启设置 ✅({auto,on} 双格式持久化 + 降级兼容,1 单测 + 1 集成测试)。**M3 代码项全部落地**;更新空闲策略随 M4 updater(暂缓) |
| M4(+2 周)**🟡 壳+前端已工程化(2026-11-04)** | 三平台打包 + 自动更新 + 签名 + 文档站 | 桌面壳骨架 ✅:smoke 进化为窗口壳(静态仪表盘页 + /admin/dashboard 数据面 + bundle 配置 dmg/app);aubridge smoke: PASS 全链路 PASS。React 前端工程化 ✅(2026-11-04,变更记录 26:crates/smoke/web Vite 工程 + 三页组件化,产物直落 ui/,新 clone 需先 npm build)。agent 接线产品化 ✅(2026-11-04,变更记录 30/31 批 1:AgentWiring trait 注册表 + /admin/agents 面 + Agents 第四页签 + aubridged 路径注入;HTTP client 复用 + §11 转发 p99 预算基线测试)。批 2 能力项 ✅(2026-11-04,变更记录 31 批 2:group/a+b 请求内联跨模型兜底 + 请求决策日志 decisions 表 + Dashboard"最近决策"区)。剩余:三平台打包矩阵(签名密钥,用户门控)、tauri-plugin-updater(暂缓,§4 端口 handover 替代)、文档站、公开 v0.1.0 |

相对 v1:M1 收窄(先 1 个 agent,翻译保真优先;3 个 agent 挪 M2),
M0 新增;M3 因去掉 wasmtime 从 3 周缩至 2 周;总周期不变。

## 11. 已知风险(写进设计,不藏)

1. **协议翻译保真是最大技术风险**:LESSONS 事故清单已固化为回归集,
   但真实 agent 会话的长尾(中断恢复、多工具并发、超大上下文)只有
   语料积累才能覆盖——回归集语料是持续资产,不是一次性交付;
2. **厂商免费政策是流动的**:quota source 全部数据驱动(models.dev
   同步 + 每源探活测试),政策变了改配置不改代码;
3. **ToS 风险无法清零**:立场段 + 插件隔离是减震不是免疫;复用官方
   CLI client_id 属社区通行但有被针对的可能;若收到厂商函件,标准
   动作是下架对应 preset/插件并公告,不动核心。不做指纹伪装意味着
   承受"被识别"的可能,这是产品立场的代价;
4. **写回用户 CLI 凭据的破坏风险**:已通过默认只读 + opt-in + 备份
   还原降级,残余风险明示给用户;
5. **订阅额度查询接口非公开契约**:`/api/usage` 是官方 CLI 使用的
   内部接口,可能变动;查询失败降级为推断,不影响可用性。

## 12. 待拍板(作者未决,移交产品负责人)

1. **仓库位置**(项目名已定 aubridge,仓库尚未创建);
2. **对标价格表**:免费模型 → 付费对标模型的映射(§8.3)逐家确认,
   这是"省 $X"数字的公信力来源;
3. **插件"自动开启"语义**:按"独立开关、默认关、开了之后新装插件
   自动启用"理解,待确认是否需要"官方/社区分级信任";
4. **Gemini OAuth 免费档(Antigravity 端点)进 v1.1 的条件**
(国内可达性验证方案);
5. NVIDIA NIM 的账号积分模式验证结果,决定去留。

## 13. 决策记录(作者已定,可推翻)

- 2026-11-12 代理页增加显式手动模型测试：自动、命名分组与具体模型均使用独立 `ui-test` 身份，经现有 `/v1/models` 与 `/v1/messages` 生产鉴权、路由和账本；不直连源、不增加后端测试入口。仅补 CORS 对既有 `x-aubridge-provider` 的暴露，保留 Origin、Host、鉴权限制。固定短文本、有限等待、真实额度提示及结果证据边界见 [模型真实调用测试](proxy-model-test-2026-11-12.md)；不改变路由/接线或 0097 状态。
- 2026-11-12 用户 UI 改版：六页与既有业务能力不变，显示为 Agents / 模型源 / 模型 / 代理 / 插件 / 设置。默认800×480、最小760×480、不最大化；采用紧凑顶栏、可滚动主区、源配置与请求诊断按需展开。视觉与呈现定稿见 [ui-redesign-2026-11-12.md](ui-redesign-2026-11-12.md)，术语同步 [ui-design.md](ui-design.md)。仅设计/展示层决策，不改变源、路由、凭据与接线合同；实现和原生验收分别记录，0097的BLOCKED及累计4/10不变。

- Rust + Tauri 2(决定因素:Tauri 桌面壳、常驻低内存、cc-switch 已
  验证同进程代理形态;代价是开发速度与贡献者门槛);
- 网关与 UI 同进程;
- 端口 3427,占用时 port 0 随机 + 持久化;
- v1 双协议(Chat + Messages),同协议字节透传;
- v1 只做"免费优先"一种路由策略;
- 插件 v1 = 纯声明式 manifest,无代码执行;wasmtime 不进 v1 依赖树;
- 白名单核心 + 插件外围(默认关)的源治理结构;
- 不做:多账号池、TLS 指纹伪装、多用户/计费;
- OAuth 写回默认只读,共享登录 opt-in;
- 网关直接使用已有的 `httpdate` 解析绝对 `Retry-After`，统一进入现有冷却边界。迭代 0019 记录对照与验收。
- 会话亲和仅订阅源;
- 翻译回归集(LESSONS 固化用例)是 M0/M1 一级交付物。

## 附:来源与参考

- magpie(`~/play/magpie`,本地源码,深度调研):IR 双向翻译架构、
候选循环与 holdWriter("首字节前才可 failover")、rest 四桶分类
(credit 爬梯持久化/quota/rate/other)与 `probeOne`、patience 组重试、
亲和 Auto 模式(cache 读写量为据)、`claude_dirs.go`/`codex_store.go`
凭据语义与"刷新必等完"铁律、安全边界中间件(`allow/cors/lan.go`)、
端口 handover、防卡死护栏、`docs/LESSONS.md` 898 行事故库(已固化为
§6.1 用例 F-1~F-10 与散见各节的设计规则);
- sub2api(`~/play/sub2api`,Go + PG + Redis,深度调研):多用户/计费/
多账号池/完整 mimicry 的反面教材(明确不做);正面借鉴——Claude
OAuth 常量与流程、`/api/oauth/usage` 订阅额度查询与被动采样、
ratelimit 头解析(`anthropic-ratelimit-unified-*`)、错误分类矩阵、
token 刷新调度与"字段级写回"事故教训、400 请求体过滤规则、
一致性铁律(UA/beta/版本/session 同源);
- CLIProxyAPI(`~/play/CLIProxyAPI`,Go 单二进制,深度调研):凭证
抽象 + (策略/权重/优先级/双层冷却)调度器、翻译器 Registry
(from→to 注册制 + 四通道)、请求日志与错误兜底捕获、别名模型池、
错误脱敏、Gemini OAuth(Antigravity)免费档先例;
- model-router(`~/play/model-router`,TS BYOK 代理):探活/熔断参数、
同协议透传、AES-GCM 密钥纪律、价格 null-cost 纪律、记账状态机、
thinking 整流器、sticky sessionHash;
- new-api(`~/play/new-api`,Go 分发网关):能力表路由索引、加权平滑、
结构化重试决策(`PolicyDecision`)、渠道自动禁用/恢复;
- cc-switch(`~/play/cc-switch`,Tauri 2 桌面应用):同进程 tokio+axum
代理、preset 注册表数据模型、keyring、原子写配置、托盘生命周期、
tauri-plugin-updater、port 0 回退;
- `~/play/sub2api-vs-model-router-token-proxy-对比报告.md`、
`~/play/model-router-最新迭代深度分析-2026-10-17.md`、
`~/play/model-router-移植sub2api能力方案.md`:本地已有的一手调研;
- models.dev:免费模型清单与价格数据源。

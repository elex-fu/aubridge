# Codex 本地鉴权自动重读 · 2026-11-13 真实验收方案

状态：A 安装版真实调用、B 同进程热更及清除登录后的本地拒绝均 PASS。首次验证器错误独立保留；仅验证已有行为，不改生产代码、新增功能或推进 0097 BLOCKED / 4 of 10。

2026-11-17 状态校正：本文的 0097 BLOCKED / 4 of 10 是当时记录；
0097 后续在 2026-11-14 完成托盘验收，当前为 PASS / 5 of 10，见该轮页首。
本专项通过范围仍限定为下述 A 与 B2，不扩展为原生 OpenCode 登录或真实续期。

## 1. 要证明的两件事

A. 当前已安装桌面应用的生产网关，使用本机现有登录，对 `gpt-6.1-sol` 完成一次真实普通文本调用。

B. 另一个使用同一份生产 Runtime/ChatGptChannel 代码的 CLI 单一进程，在其私有 auth 文件从 synthetic 登录替换为真实 access 副本、再替换为无登录后，分别使用新内容成功请求、随后停止使用旧凭据。不重启进程，不靠刷新页面/管理页触发重读。

两项证据独立：B 证明生产凭据路径的进程内热更，不能声称修改了真实桌面应用正在读取的 auth 文件，也不能声称做过 GUI 文件更换验证。只从本机时间或 token exp 不能判断远端可用；A/B 均以真实响应及账本为准。

## 2. 已核实路径

- `crates/gateway/src/runtime.rs::Runtime::start` 为桌面与 CLI 共用入口；Codex 路径优先取进程 `CODEX_HOME`，否则 HOME 下 `.codex`。数据目录取 `PLUGINS_DIR` 的父目录，持有目录锁；私有 config 端口可独立。
- `ChatGptChannel::current -> reconcile` 每次读取指定 `auth.json/config.toml`；变化时替换 source/effective，`tokens:null` 清空缓存。`ready` 再以 access token exp 判断是否刷新。
- `has_login -> current` 只读取，不调用 `ready/refresh`；路由资格检查 `credential_with_key` 会走该入口。
- `GET /admin/providers` 虽可触发该检查，但还调用 OpenRouter 预算查询；不优先用于“完全不发上游”的预热。
- 更窄的预热用既有 `GET /admin/routing/preview?model=<私有命名分组>&tools=false`：只执行 route_plan/资格检查，不推进轮转，不发推理、不刷新登录。该 API 要求已有命名分组，故只在私有配置中建立含 `gpt-6.1-sol` 的单成员分组。预览结果需唯一指向内置 chatgpt，否则停止并收窄隔离配置。
- `GET /admin/dashboard` 会触发 WHAM/usage 与 `ready`，实验全程禁止用其作为就绪探针。

## 3. 私有环境与凭据处理

执行器在权限 0700 的任务临时目录内创建独立 HOME、CODEX_HOME、PLUGINS_DIR/data、Claude/OpenCode/XDG 路径，文件 0600。环境值通过子进程 env 显式传入，不改当前会话的 HOME/CODEX_HOME。清除继承的 Provider key 环境变量，使用新的账本与私有端口，不继承用户接线记录或真实配置。CLI 会读取 cwd 中的 `.env`：子进程 cwd 必须为本次私有 stage，不能使用仓库目录；stage 内不得放 `.env`，child env采用最小白名单，避免继承源key或代理相关非预期配置。启动唯一生产 `aubridged` 子进程，保存 PID/起始时间/二进制摘要；整个 B 阶段不能重启。

所有凭据读取、JSON 解析、原子替换都在验证器内完成。真实 `~/.codex/auth.json/config.toml` 只读，不写入、不重命名、不导入新登录、不要求用户登录。比较原始字节只能记录 unchanged=true/false；不输出内容、token、ID/account 标识或鉴权文件 hash。真实文件若被外部 CLI 并发改动，记录“原文件并发变化，无法证明字节未变”，不要写回以恢复它。

**私有可运行副本移除 refresh_token。** `share_login=false` 只保证不写回，不能阻止 401 导致服务端轮换 refresh token。B 的目标仅为 access 文件热更，因此 verifier 复制真实 access 与必要登录字段，但将私有工作文件的 refresh_token 置空；记录 access_matches_source=true、refresh_removed=true，不记录值。先确认真实 access exp 可读且余量大于 5 分钟加本次实验预算；不足就停止，不主动刷新或尝试重新登录。原始完整文件只在内存短暂读取，不落一份含可用 refresh 的实验文件。

保持私有 `share_login=false`，用现有设置读取验证；不得自动打开共享登录。私有 config.toml 使用文件存储语义，不搬运真实配置中无关 Agent/服务器/密钥设置；如果真实配置存在阻止文件读取的 guard，报告并停止，不通过私有环境冒充对真实路径的等价验收。OAuth 仅走内置 chatgpt 固定 Responses 目标。

## 4. 可执行顺序与断言

### A. 当前安装应用实调

1. 确认实际监听进程属于当前安装包，记录可公开二进制摘要与 endpoint；管理令牌只留内存，不回显。保留现有路由和 share-login 设置，不为实验改动用户设置或凭据。
2. 用真实 `crates/smoke/web/src/api.ts::testProxyModel('gpt-6.1-sol', signal)` 调用该 endpoint。运行入口仅注入 `gateway_access` 所需的 endpoint，实际 fetch 不做 mock，不复制改写 classifier；65 秒总等待且不自动重试。记录调用前后的账本定位条件，以单次客户端 ui-test/请求模型/时间/实际来源一致性匹配，不盲取最新一行。
3. 成功要求 helper 为 success、有效非空文本与明确完成终态，且本次来源为 chatgpt；真实失败如实留存。此步是安装包生产网关实调，不是鼠标点击 GUI 的证据。其正常 OAuth 处理沿既有行为；不能因为 share-login=false 就声称服务端绝无 token 轮换。

### B. 同一生产 CLI 进程热更

1. 启动前在私有 auth.json 写入 `auth_mode:chatgpt`、非空 synthetic sentinel access、空 refresh/ID/account；sentinel 不含真实值。配置所需私有单成员命名分组及 share_login=false。启动后只读私有 session 文件取得 endpoint/admin token；不打印 token。
2. 只调用一次该分组的 routing preview，确认 chatgpt 是唯一候选，凭据状态通过。此时 sentinel 已被 `current/reconcile` 观察；不允许任何推理、dashboard、Provider check 或 refresh 访问。保存同一 PID 与已就绪证据。
3. 在同目录写入 0600 临时文件并原子 rename，将私有 auth 替换为上述真实 access 工作副本。**从替换完成直到下一条推理之间，不调用任何 admin、目录、预览、健康检查或 UI。** 单纯 PID 存活检测不读取凭据，允许。
4. 立即用同一实际 api.ts helper 发一条 `gpt-6.1-sol` 请求到该私有 endpoint。helper只调用/v1/messages，不先拉目录。65秒超时，不自动重试。断言 success、provider=chatgpt、有效文本；再读私有账本，核对 agent=ui-test、requested model、selected provider/served model、outcome与时间；记录无刷新能力及同一 PID。
5. 完成记录后，在私有 auth 原子写入 `auth_mode:chatgpt,tokens:null`。**同样不夹入任何 admin/目录/预览读取**，直接用同一 helper 发送同一模型请求。
6. 该请求必须本地拒绝：helper非success、chatgpt被以missing-credential跳过、没有实际上游尝试。仅看到HTTP失败不够；查看该次私有 ledger 的 attempts为空且skipped包含chatgpt/missing-credential，不能将unknown/缺失轨迹等同于零尝试。该证据属于生产路由决策，不宣称做了网络抓包。同一 PID仍在则完成热更→撤销闭环。
7. 结束本任务创建的CLI进程，等待退出/账本结算，删除私有auth工作文件与临时副本，清理不含其他用户数据的任务目录；仅保留经过白名单处理的非敏感证据。最终比对真实auth/config未被验证器写动，发生并发改变则如实标注。

## 5. 停止条件与交付格式

任何一步出现路径指向用户真实目录、权限不私有、非预期共享登录、需刷新才能继续、来源不是chatgpt、多个候选导致归属不清、进程重启/退出、helper失败/未确认、无法匹配账本或撤销后仍有实际尝试，都停止该项。保留首个失败，不改用户登录、不加重试层、不切其他源。清理始终执行。

A失败不自动否定B的本地重读机制；B远端调用失败也不能把“未成功”冒充热更成功。分别记录：安装包实调结果、独立同源CLI热更结果、凭据撤销结果、源码/二进制/进程身份、真实文件未写入断言、清理状态。正常预期真实上游推理最多两条（A一条+B有效access一条）；B撤销检查预期零上游尝试。

本方案不需要GUI、新登录、新功能、全套测试或修改真实配置。

## 6. 执行记录

本次由 Astra 定稿方案，Luna 执行真实验证，root 归档。证据目录 `/tmp/aubridge-auth-autoload-F1hqIu/`，不复用此前成功回执。

### A. 安装版真实调用：PASS

- 实际应用 `/Users/lex/Applications/aubridge.app/Contents/MacOS/smoke`，PID `67295`，监听 `127.0.0.1:62760`；执行前后同一进程、监听与二进制身份一致，没有重启。
- 安装 executable SHA-256 `0fe54c9f6f446a909d56c1d3981f06e6873edcf1951f334412119b005ed85b45`；真实 helper source SHA-256 `b9d15b313970088b9ca1e18e1e56f450d676aae576e6321a6843f9e7de052ffe`。
- 目录 HTTP 200、21 个模型且目标可选；随后仅一条 `testProxyModel("gpt-6.1-sol")` 实际调用：HTTP `200`，provider `chatgpt`，响应模型 `gpt-6.1-sol`，文本 `OK`，helper 耗时 `3653ms`。
- 本次正常账本按时间、模型和 `ui-test` 匹配一条 `outcome=ok`；单次 chatgpt attempt、状态 200、报告 input/output `10/5`，账本耗时 `3647ms`。没有伪造或插入历史。
- [独立回执](/tmp/aubridge-auth-autoload-F1hqIu/desktop-backend-result.json) 与 [实际命令](/tmp/aubridge-auth-autoload-F1hqIu/desktop-backend-command.txt)。这只证明实际安装版网关调用，不证明 GUI 按钮或桌面原文件热更。

### B. 首次验证器错误：不计入通过

首次执行因验证器装配错误中断，保留为 `INVALID_FIXTURE`，不计作产品失败或热更通过：

- 私有 CLI PID `73303`、二进制 SHA 与预期 `ec1faffa3d6e30cc8a59cd45884b7b941e698606adbb92e59ad4b33375d0bdc0` 一致；sentinel 预览唯一候选为 `chatgpt/gpt-6.1-sol`，真实 access 余量检查及私有文件原子替换均完成。
- 验证器漏注入 Node 环境所需的 `gateway_access` IPC stub，helper 在 HTTP 发出前失败，HTTP 状态 null、没有匹配账本。该次 `effective_request.count=1` 表示调用 helper 一次，不能解释为实际 HTTP 或上游请求；实际推理请求为零，撤销阶段未执行。
- 私有进程已退出 0，私有 auth/session/runtime log 已清理，原始 auth/config 比较均 unchanged=true，正常桌面进程未动。
- [首次原始结果](/tmp/aubridge-auth-autoload-F1hqIu/hot-reload-result.json)、[首次命令](/tmp/aubridge-auth-autoload-F1hqIu/hot-reload-command.txt) 保留。Sol 仅修正验证器、另存 v2，由 Luna 新执行一条完整 B 链；不会覆盖原记录。每条完整链内部仍必须保持同一 PID。

### B2. 同进程热更与清除登录：PASS

Sol 交付 `/tmp/aubridge-auth-autoload-F1hqIu/hot-reload-verifier-v2.mjs`，补齐并自检固定 endpoint 的 IPC stub、使用 helper 绝对路径，并以 rowid 下界区分撤销后的新账本记录，避免同秒误取上一条成功。只修改临时验证器，保留原始脚本和失败记录；`rtk proxy node --check /tmp/aubridge-auth-autoload-F1hqIu/hot-reload-verifier-v2.mjs` 为 RC 0。

Luna 在私有 stage 运行：

```sh
rtk proxy node /tmp/aubridge-auth-autoload-F1hqIu/hot-reload-verifier-v2.mjs
```

进程执行时间 `2026-11-13T05:52:15.889Z` 至 `05:52:26.336Z`，CLI 为 `/private/tmp/aubridge-cycle93.0qTct6/target/debug/aubridged`、SHA 与上文 `ec1faffa…d0bdc0` 一致。两次推理入口请求始终使用同一 PID `74593`，没有重启或实际请求重试。

| 阶段 | 实际结果 |
|---|---|
| 缓存预热 | 私有 synthetic 登录经 routing preview 观察，唯一候选 `chatgpt/gpt-6.1-sol`；没有上游推理 |
| 替换为真实 access 副本 | 原子替换成功，`access_matches_source=true`、`refresh_removed=true`；access 余量检查通过，副本没有真实刷新能力 |
| 不夹入管理/目录读取，立即实际调用 | 请求模型/响应模型均 `gpt-6.1-sol`，HTTP 200、provider `chatgpt`、文本 `OK`，helper 耗时 `4325ms`；私有账本 rowid 1、`ui-test/ok`、单次 chatgpt attempt、报告 input/output `10/5`，账本耗时 `4321ms` |
| 私有文件原子写入 `tokens:null` 后立即调用 | 本地 HTTP 502、helper 耗时 `3ms`；新账本 rowid 2、`no-candidate`、`attempts=[]`，明确 `skipped=chatgpt/missing-credential`；usage `null/null`，没有复用旧登录发出上游尝试 |
| 退出与清理 | 私有 CLI 退出 0；私有 auth、session、runtime log 清理完成；原始 `auth.json`、`config.toml` 均 unchanged=true；正常桌面应用未操作 |

[B2 独立回执](/tmp/aubridge-auth-autoload-F1hqIu/hot-reload-v2-result.json)、[B2 实际命令](/tmp/aubridge-auth-autoload-F1hqIu/hot-reload-v2-command.txt)。本次 A+B2 共 3 条真实推理入口 HTTP 请求，其中 2 条到真实上游成功，另 1 条是清除登录后预期的本地拒绝；B1 没有发出推理 HTTP。没有以无账本记录充当零上游尝试。

本次证明：实际安装版能够使用当前本地登录；同源生产网关在同一进程内可在普通请求时自动采用变化后的登录内容，并在登录清除后停止复用缓存。热更实验使用独立私有文件，未修改桌面实际鉴权文件；不扩大为 GUI 热更、真实 refresh-token 自动轮换、其他模型或长期可用性验收。仅文档与临时验证器发生变化，因此没有重复编译、fmt、Clippy 或工作区测试；0097 BLOCKED / 4 of 10 保持。

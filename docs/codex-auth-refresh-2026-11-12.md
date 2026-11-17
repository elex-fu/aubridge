# Codex 本地登录刷新依据修正 · 2026-11-12

状态：实现、358 项回归，以及隔离实例和正常桌面进程的真实代理重试通过；修复版已启动，GUI 核对因窗口丢失及再次锁屏未完成。用户本次要求“使用本地 codex 鉴权文件更新下信息，重新试下”。本方案只修正主动刷新依据，随后通过真实代理重试指定模型；不推进 0097 BLOCKED 或改变累计 4 / 10。

## 1. 当前失败与证据边界

本次桌面代理页请求已有一条真实失败记录：内置 ChatGPT 候选以缺凭据跳过。排查已确认本机 `auth.json` 可解析为 ChatGPT 登录且相关 token 字段存在；只检查字段存在性与到期元数据，不打印 token、账号标识或完整文件。

本次安全元数据检查显示：ID token 已过期，access token 尚未到期。当前 `crates/credentials/src/chatgpt.rs` 的 `access_expires_at` / `needs_refresh` 实际读取传入 ID token 的 exp；`crates/gateway/src/chatgpt.rs::ready` 传入 `t.id_token`，因而即使 access token 仍在期限内，也先触发刷新。刷新无法返回可用凭据时，候选被跳过。

这是本地判定缺陷，不能由此声称服务端一定接受当前 access token。JWT payload 解码不验签，exp 只用于本机刷新时机；服务端授权结果必须由后续真实生产请求确认。既有失败记录保留，修正后的结果另记，不覆盖原失败证据。

官方 Codex 本地源码（提交 `99744cfe04806ebaa1e5d08e3e790070f852472b`）`codex-rs/login/src/auth/manager.rs::should_refresh_proactively` 优先解析 `tokens.access_token` 的到期时间。本次仅采用这个字段依据，不复制其 last_refresh 定期刷新策略或阈值。

## 2. 最小实现合同

1. `access_expires_at(access_token)` 读取 access token JWT 的 `exp`，函数命名保持；修正参数名、文档和对应测试名称，避免继续把 ID token 语义写入合同。
2. `needs_refresh(access_token, now)` 沿用当前 `< 5 分钟` 阈值与饱和减法；已到期需要刷新。无 exp、opaque token、无法解析的 payload 或非整数 exp 仍返回 false，等待服务端 401 走原有兜底；不以 ID token 或 last_refresh 猜测 access 生命周期，不增加失败重试层。
3. `ChatGptChannel::ready` 将 `t.access_token` 传给判定。凭据获取、刷新方法、刷新单飞、401 后刷新一次重试、撤销与换源分类均保持现有行为；不将“刷新失败仍强行用过期 token”作为补救。
4. ID token 继续作为登录材料保存和必要时轮换，不因其过期删除登录或取消仍可用的 access token；本次不新增身份校验、账号选择或 OAuth 登录 UI。

生产改动限于两个现有 ChatGPT 凭据模块及必要的已有测试。无新 crate、依赖、API、凭据导入功能、配置项或存储。

## 3. 本地文件与权限边界

- 复用已有 `read_codex_login` 与 `reconcile`：发送前重读本地登录，外部 CLI 更新后下一次请求应使用当前文件；不能另外复制用户 token 到配置、模型源 key 或网页状态。
- 现有文件缺失、暂时无法解析、登出、切换登录、刷新期间文件变更与还原失效语义保持。不得为了本次重试绕过 guard、忽略撤销或复活旧登录缓存。
- 继续尊重 `cli_auth_credentials_store` 和当前 share-login 设置。默认只读原 CLI 文件；只读模式刷新结果留在现有内存缓存。用户已有 opt-in 写回时继续原有字段合并、并发核对、文件锁、私有权限和原子替换，不自动打开 share-login，不新增写入路径。
- OAuth 仍仅由内置 `chatgpt` 连接器使用，目标是既有固定 Responses 地址，维持拒绝重定向；自定义源/插件不能借此使用 Codex 登录。
- 不记录或渲染 access/refresh/ID token，不输出完整 auth JSON 或 Authorization 头。设计者不读取原始鉴权文件；此文仅引用已完成的安全元数据结论。

## 4. 最小验证与真实重试

先添加能证明入口字段正确的针对性失败用例，再修改实现：

| 用例 | 期望 |
|---|---|
| access 有效期明显超过 5 分钟、ID 已过期 | `ready` 返回当前 access；刷新端点请求数为 0 |
| access 已临近到期、ID 仍有较长有效期 | `ready` 调用既有刷新端点一次并使用新凭据 |
| access 无可读 exp、ID 已过期 | 不主动刷新；保持原 401 兜底 |
| access 剩余恰好 300 秒 / 299 秒 / 已过期 | 分别不主动刷新 / 刷新 / 刷新，维持原边界 |

前两例必须经过 `ready` 或等效服务边界，不能只把同一个 JWT 传入纯函数便声称修复了调用字段。使用合成 JWT 与本地回放端点，不使用真实凭据做单测。

保留并运行直接相关的现有 401 刷新/撤销、磁盘变更/登出/切换、刷新回执与文件竞争、只读与 opt-in 写回测试；不扩展其他 Provider 或协议矩阵。完成相应 fmt/编译与定向检查后再重包，记录构建身份。

随后使用新包中的代理页测试功能，显式选择 `gpt-6.1-sol`，发送一条既定短提示，经真实生产代理入口验证。复用当前本地登录和当前路由，不改模型源或接线，不转为直连调用，不顺带测试 Kimi。回执分别记录：请求模型、响应模型、真实来源、终态/错误、耗时及账本归属；凭据值不进入证据。

如果服务端拒绝当前登录或刷新失败，保留该真实结果并报告首个实际阻碍；不把本地 exp 有效、编译通过或回放成功写成真实上游成功，也不连续扩大重试范围。

## 5. 产品与 UI 关联

产品设计 §7 的 Codex 刷新条款同步明确 access token exp 为依据，并指向本文。代理页沿用 [模型真实调用测试](proxy-model-test-2026-11-12.md) 的现有按钮、短提示、有限等待、未知值和实际结果语义，无新增 UI 或凭据管理流程。

## 6. 已完成代码验证

- 两条新增 `ChatGptChannel::ready` 回归在旧生产实现上各运行 1 项、均 RC 101：有效 access 被错误刷新（实际 1 次、期望 0 次），临期 access 配新 ID 时漏刷新（实际 0 次、期望 1 次）。这两次为真实断言失败，不是编译失败或零匹配。
- GREEN 修正后，三条 ready 回归（含 opaque access＋过期 ID）全部通过；凭据模块 9 项、既有网关登录/401/变更边界 4 项通过。
- 最终 fmt、工作区全部目标 Clippy 与工作区测试通过，20 suites、358 passed、0 failed、0 ignored；测试使用合成凭据和本机刷新端点，没有读取真实鉴权或调用上游。
- 证据位于本次 stage 的 `codex-auth/`：`auth-red-1.log`、`auth-red-2.log`、`04-green-cargo-ready.log`、`05-green-cargo-credentials-chatgpt.log`、`06-green-cargo-gateway-chatgpt.log`、`07-cargo-fmt-final.log`、`08-cargo-clippy-final.log`、`09-cargo-workspace-test-final.log`。真实重试结果单独追加，不以这些回归替代服务端授权成功。

## 7. 真实重试结果

CUA 启动修复包时发现 Mac 锁屏，未绕过锁屏。新包已签名安装；后台改用同源新生产 CLI 和真实 `testProxyModel` helper，仅提供 endpoint 的 IPC stub，生产 reader 读取原始本地 Codex 文件，经正常 `/v1/messages` 到固定内置 ChatGPT 目标。

该隔离实例仅发出一条真实推理：`gpt-6.1-sol`，HTTP 200，provider `chatgpt`，响应模型 `gpt-6.1-sol`，短答 `OK`，耗时 2292ms。账本 `ui-test/outcome=ok`，一次 chatgpt attempt，报告输入/输出 10/5；本地鉴权与 Codex 配置内容、mtime 均未变化。私有验证网关已退出。

这是后台真实调用成功，不是 GUI 成功或长期可用性承诺。用户解锁后修复版已由 CUA 启动并附着；模型选择时窗口丢失，随后系统再次锁屏，未点击测试。只读核验 PID 67295 与其回环监听 127.0.0.1:62760，安装包身份一致；GUI 重试已停止。新 App executable SHA `0fe54c9f6f446a909d56c1d3981f06e6873edcf1951f334412119b005ed85b45`，新 CLI SHA `ec1faffa3d6e30cc8a59cd45884b7b941e698606adbb92e59ad4b33375d0bdc0`。完整证据与原先失败对照见 [模型测试验收](proxy-model-test-acceptance-2026-11-12.md)。

另对这个正常桌面进程执行一次真实 helper 请求，未新启网关或重启应用：`gpt-6.1-sol`，HTTP 200，来源 `chatgpt`，响应模型同名，回复 `OK`，2002ms；正常账本 `ui-test/ok`、一次 chatgpt attempt、报告用量 10/5。目录 HTTP 200，21 个模型且目标可选。本次验证未读取 auth/admin token、未改配置；实际安装版代理已能使用当前本地登录。该结果属于桌面后端 HTTP 实测，图形按钮仍未完成复核。

用户随后要求真实验证自动加载，新增 [2026-11-13 专项验收](codex-auth-autoload-acceptance-2026-11-13.md)：当前安装版新调用 `3653ms` 成功；另一个生产 CLI 同一进程在私有登录内容变化后，不借管理页重读即可真实调用成功 `4325ms`，清除私有登录后本地拒绝且账本零上游尝试。原始 auth/config 未变，首次验证器错误及修正后的独立回执均保留。此项证明文件重读，不宣称验收了远端 refresh-token 轮换。

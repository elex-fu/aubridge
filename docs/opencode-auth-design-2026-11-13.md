# OpenCode 登录自动读取 · 2026-11-13 最小设计

状态：三项功能已实现，386 项回归、UI 回放、真实客户端兼容验证及安装通过；原生 OpenCode 登录文件缺失，该来源原生登录实测单列未完成。由 GPT-6.1 Sol 实现、Luna 独立验证；不改 0097 BLOCKED / 4 of 10，不启动新迭代。完整结果见 [专项验收](opencode-auth-acceptance-2026-11-13.md)。

## 1. 首轮范围：三项

1. 自动定位 OpenCode `auth.json`，读取其 OpenRouter API key，作为内置官方 OpenRouter 的只读末级凭据来源。
2. 读取其 `openai` OAuth 登录到内置 `chatgpt` 固定 Responses 通道；设置中显式选择 Codex / OpenCode，未选择过时首启自动判定缺省（2026-11-14 变更，见 §4）。选择后每次请求自动重读对应文件；文件更新无需重启或导入。失败、注销、文件损坏时都不自动切换到另一登录来源。
3. 最小来源/读取状态/只读说明 UI，以及针对读取、刷新、热更和来源边界的验证。

本轮API白名单只含 `openrouter`，其他 `type:api`、`wellknown`、非 openai OAuth、自定义 Provider、环境 JSON、任意路径扫描、多账号池与登录交互后置。`openai` API key绝不能映射ChatGPT订阅OAuth；Kimi/Zhipu等ID映射存在版本差异，不顺手扩大名单。

## 2. 源码依据

参考读取的是工作区源码，不是用户鉴权文件。记录 HEAD 只是定位，不表示工作区无未提交改动。

| 仓库与 HEAD | 依据 |
|---|---|
| OpenCode `a85d8d23aa297b3051e642c28e3fc79b457fc4bc` | `packages/opencode/src/auth/index.ts:10-35`：数据目录 auth.json；顶层 providerID 映射；oauth/access/refresh/expires/accountId 格式；未知类型分离 |
| 同上 | `packages/core/src/global.ts:3-14`：数据目录为 XDG data/opencode；不是 OPENCODE_CONFIG 所指配置文件 |
| 同上 | `packages/opencode/src/plugin/openai/codex.ts:10-12,124-139,359-388`：OpenAI client ID、官方 token/Responses 地址、逐请求 getAuth、expires 毫秒、刷新 form 请求 |
| Magpie `8abdf53a881facd421b0477647a7cc81beb78bff` | `internal/provider/import_opencode.go:23-29,40-53,81-82`：自动XDG数据路径、API白名单（含openrouter）、跳过OAuth/wellknown；`internal/provider/importapps.go:76-89,109-133`：按用户打开导入器发现/读取，非每请求追踪；appdir `env.go:104-117` 忽略相对XDG路径 |
| aubridge `49d5d17dfef35b9ac51ba3acbe170a619ed18bd1` | `crates/credentials/src/oauth.rs`、`crates/gateway/src/chatgpt.rs`、`runtime.rs`、`state.rs`：现有 Codex 文件重读、source/effective generation、刷新单飞、Codex写回/备份和固定目标约束 |

Magpie不要求用户填写标准auth路径，但它的OpenCode入口是用户打开导入器后读取并保存所选Provider；它没有实现OAuth读取或凭据文件每请求热更。我们复用自动定位和最窄API映射，热更/OAuth是本次新增，不能冒称Magpie已有。Magpie的配置文件检测另走OPENCODE_CONFIG_DIR/XDG_CONFIG_HOME，不是auth发现；没有额外macOS Library/Application Support候选，不添加猜测路径列表。

OpenCode 当前 `CLIENT_ID` **实际与 Codex 相同**：`app_EMoamEEZ73f0CkXaXp7hrann`。这是本次源码核实结果，不是“所有 OAuth 可共用 Codex client_id”的推断。`enterpriseUrl`、文件中额外 issuer/base/client_id 或插件地址均不改变本功能固定目标。

## 3. 路径与读取接口（credentials owner）

Runtime 启动时确定 OpenCode 文件路径：有效绝对 `XDG_DATA_HOME` 下 `opencode/auth.json`；未设置、空或相对路径则用 `HOME/.local/share/opencode/auth.json`（HOME 继续沿现有 USERPROFILE 后备）。路径一经选定不扫描其他位置，不跟随 OPENCODE_CONFIG、CODEX_HOME、OPENCODE_TEST_HOME 或任意插件配置。改变进程路径环境仍需重启，文件内容更新不需要。

新模块建议 `credentials::opencode`，冻结公开读取合同：

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeOpenAiLogin {
    pub access: String,
    pub refresh: String,
    pub expires_ms: Option<u64>,
    pub account_id: String,
}
pub fn read_opencode_openai_login(path: &Path)
    -> io::Result<Option<OpenCodeOpenAiLogin>>;
```

- 只选择顶层精确 `openai` 键且 `type == "oauth"` 的对象。其他 provider 项不逐条反序列化；未知/损坏的无关项不能阻断有效 openai 项。
- 文件不存在、没有 openai、该项非对象、类型缺失/非 oauth、access 为空或全空白：`Ok(None)`。不接受尾斜杠/模糊ID别名，不把其他项拼成登录。
- 根 JSON 不是对象、JSON/IO 错误，或已选中 oauth 的 access 缺失/非字符串：`Err`。refresh/accountId 缺失为空字符串，存在但类型错误为 `Err`。
- `expires` 映射到 `expires_ms`，缺失允许 None；存在时只接受 u64 非负整数，null/字符串/负数/小数均为 `InvalidData`。`0` 是明确过期，不当作未知。原 schema 要求 expires；此处允许缺失是为保持未知到期时间的401兜底，不补造期限。
- access/refresh/accountId 保留原字符串字节，不在解析时 trim 后改写凭据；只用 trim 判断 access 是否全空。其余未知字段忽略，enterpriseUrl 无执行含义。
- 不消费 `OPENCODE_AUTH_CONTENT`。本次任务是读取文件，该环境功能需要另定可观察性和撤销合同。所有凭据结构只在内存传递，不加入 admin JSON、日志或调试输出。

## 4. 来源选择与状态（gateway/settings owner）

配置字段固定为 `chatgpt_login_source: "codex" | "opencode"`（Settings内存字段使用带默认值String，gateway显式validate；Runtime来源判定使用load_checked，不复用吞错默认）。**2026-11-14 变更：默认值从 `"codex"` 改为空串，空串 = 从未选择**；字段缺省或文件缺失时启动期自动判定一次缺省：Codex 登录可读则 codex；仅当 Codex 干净缺失（`Ok(None)`，含 keyring guard——本通道读不到文件登录）且 OpenCode `openai` 登录可读时才缺省 opencode；Codex 文件损坏（`Err`）或缺两者时保持 codex。判定仅在启动 `with_settings` 时执行、运行期不重估；OpenCode 路径未接线时无从判定，直接 codex。已持久化显式选择的安装不受影响（旧版 save/POST 均写入完整值）；仅"从未保存过选择且 Codex 本无可读登录"的安装会从 codex-missing 变为缺省 opencode——此前该来源本就无可用登录，不构成换账号。POST 只接受 `codex`/`opencode`，空串不经 API 持久化。源配置存在但值无效、或已有配置读失败时，ChatGPT 登录选择必须 fail-closed（状态 unavailable），不能因为 Settings 通用默认回退而悄悄启用 Codex。其他既有设置的恢复行为不在本轮重构。

扩展现有 `/admin/settings`，保留管理鉴权：

- GET 新增 `chatgpt_login_source: "codex" | "opencode" | null`、`chatgpt_login_status: "ready" | "missing" | "unavailable"`、`chatgpt_login_path: string | null`。null来源表示无有效选择；ready仅代表当前文件含可读取凭据，不代表上游授权或额度。path只回本机已确定的文件路径，无任何凭据或账号值。
- POST 接受 `{"chatgpt_login_source":"opencode"}` 或原有 `{"share_login":bool}`；一次只能有一个已知操作，两个同时给、两个都无或类型/枚举错误返回400，避免部分持久化。其他未知字段沿既有宽容原则忽略。
- 选择更新先在 settings_io 锁内 load/字段合并/保存成功，再激活来源并失效当前登录 generation。持久化失败则原选择不变。不隐式改 share_login、不触发上游请求或自动测试。
- 当前选择稳定有效时，每次 has_login/current/ready 只读取所选文件。Codex 缺失、keyring guard、损坏或401失败都不转 OpenCode；OpenCode同理不转Codex。用户必须显式更改选择。
- 同一所选文件的 access/refresh/accountId/expires 更新构成新 generation，下次请求采用外部 CLI 的最新登录。UI明确说明“跟随所选 CLI 的当前登录”；不会从其他文件自动猜账号。不是账号选择器，不显示账号ID/邮件/token。

## 5. 缓存、到期、刷新与写回

### 同一来源内热更

缓存身份必须包含来源、该来源完整已选字段快照与 generation；不能把 OpenCode 字段简单塞进 CodexTokens 然后丢失来源/expiry。有效凭据可映射成现有请求所需 access/account fields，但来源与到期元数据仍保留于网关内。

文件未变化时保留已经轮换的 effective 内存凭据；文件变化则替换 source/effective。删除文件、移除 openai、tokens不可用、type变为api均立即取消资格并清空有效缓存；临时不可读/坏JSON当次不授权，保留观察代次只用于文件恢复后核对，不继续用旧access发送。一个无关 provider 项更新不应打断 openai 登录缓存。

刷新返回时重读所选文件并核对来源/generation，旧回执不能覆盖新来源/新登录或复活已注销凭据。请求必须绑定最初使用的来源/登录代次：来源切换后旧请求可完成已发出的响应，但401恢复不得改用另一来源。refresh调用上下文需携带该来源/代次或等价不可混淆快照，不能只比较可能相同的access字符串。OpenCode刷新期间文件换账号或注销时，丢弃旧回执并结束该次恢复，下次新请求再使用当前登录；现有Codex同来源的恢复/备份回归仍须保留。

OAuth 401 不应误写 API Key 的 `key_retry_at`：现有生产路径仅在 `sent.uses_key` 时写该冷却，回放必须保留这一区别。ChatGPT 的来源隔离由上述请求快照/generation 保证，不增加没有实际失败依据的路由版本框架；OpenRouter API key 的版本与冷却合同另见 §8。同一已选来源重复提交必须幂等，保留只读模式已刷新的有效内存凭据。

### 到期与刷新

- Codex：保持已修正的 access JWT exp（秒）与 `<300秒` 判定，缺exp用401兜底，ID token不参与access到期判定。
- OpenCode：文件显式 expires_ms 优先；用毫秒计算 `<300000ms` 余量，不能误当秒或凭位数猜单位。expires缺失才读access JWT exp并安全换算，仍未知则不主动刷新、等401；opaque access配有效expires完全可用。
- 主动刷新或401恢复仍每次最多一次并保持单飞；生产端点固定 `https://auth.openai.com/oauth/token`，禁止重定向。OpenCode分支使用已核实的client ID与 `application/x-www-form-urlencoded`，字段只有grant_type/refresh_token/client_id。不把其他OAuth项送入这个刷新器，不接受文件指定的client/issuer。
- 刷新成功需非空access；refresh缺失沿原流程保留当前refresh。OpenCode effective expiry优先使用响应中合法的 `expires_in` 秒数安全换算为“当前时间+毫秒”，否则新access JWT exp，仍未知则置未知等待401。**不能保留旧的已过期expires导致每次请求重刷，也不凭空补3600秒。** 可给现有刷新响应/结果增加可选expires_in，不改变Codex现有判定。
- 刷新缺refresh/撤销/失败返回当前源不可用，按现有候选/失败路径处理，禁止换登录来源。出错文案随来源指向“请在 OpenCode 中重新登录”或现有Codex指引，不能总叫用户codex login。

### 只读与原有共享登录

OpenCode首轮**始终只读**：刷新结果只进来源绑定的内存缓存，不写OpenCode文件、Codex文件、keyring或stash。现有share_login即使true，也只能作用于Codex来源；代码写回分支必须明确检查source==Codex。来源切换不改变该偏好，也不创建OpenCode备份。

默认只读不代表刷新不会使服务端旧refresh失效：UI在OpenCode说明中明确“自动更新仅保存在本应用内；同时在OpenCode使用同一登录时可能需要重新登录”。本轮不扩展跨进程刷新协调或OC写回；重启丢失内存轮换是已有只读模式的明确边界，必要时由OpenCode重新登录更新原文件。

## 6. 最小 UI 合同（frontend owner）

设置页首部新增紧凑“ChatGPT 登录来源”panel：两项select Codex/OpenCode＋“应用”按钮；读取即展示当前生效来源（首启未选择过时即自动判定值，2026-11-14），不因用户浏览下拉立即保存。保存后说明“后续请求跟随此 CLI 的当前登录，不会自动切换到其他来源”。当前路径与读取状态使用现有小字/可折行详情；missing显示未找到登录，unavailable显示暂不可读并保留刷新入口。

改选择前后不自动发推理；使用已有代理页测试手动确认。保持800×480/min760×480，无新弹窗流程或登录表单。

原“与CLI共享登录”区域明确改称“与Codex共享登录”。选OpenCode时说明“OpenCode始终只读”；原share_login偏好保留，控件暂禁用，不能把true展示成当前OC会写回。Codex备份区标明归属，OC选择时禁用还原并提示切回Codex后操作；不删除备份。选回Codex恢复既有备份/共享交互。

模型源页 chatgpt 行追加“登录来源：Codex/OpenCode”的只读信息，固定由 `/admin/providers` 该行新增 `login_source:"codex"|"opencode"|null` 提供；其他行省略。不要为此再读取settings，也不新增key输入。来源读取失败显示未知而不是默认Codex。网页/API只获得来源、状态、路径，不获得access/refresh/account。

## 7. 首轮验证和证据边界

1. **读取与生命周期回放**：真实OpenCode schema合成夹具；未知项不干扰；expires毫秒/opaque/JWT后备；选中来源热更→注销→损坏→恢复；双文件均有登录但绝不自动切换；保存失败不激活新来源。直接走生产入口或channel边界，不仅测纯函数。
2. **刷新与来源隔离回放**：核对OC form/client_id/固定目标、单飞、新expires_in防重复刷新、401仅一次、刷新期间换来源/注销丢弃回执；share_login=true下OC也不能改任一真实格式文件；自定义源/非openaiOAuth不能取此凭据。保留现有Codex access-exp、只读、写回、备份/恢复相关回归。
3. **小窗与一次真实调用**：构建后核对来源可见、保存状态、小窗无横滚、测试仍明确手动。原生OpenCode凭据仅在实际文件存在且用户授权路径确定后，以当前源真实生产入口验证；不得创建假用户文件或为验收自动切换用户现有来源。当前root仅确认默认OC文件不存在，用户实际路径待补，独立实现与回放继续推进。

允许用既有真实Codex access映射为**私有OC格式、refresh移除**验证新读取路径到上游，但必须标为“真实access的OC格式兼容验证”，不能称“原生OpenCode登录实测”。真实原文件只读，私有cwd/环境/数据目录隔离，无token/账号日志。原生登录、回放、格式兼容和UI证据分开记录；没有原生OC登录文件时该项明确未验证，不杜撰通过。

不因本轮增加完整协议矩阵、benchmark或多源批量实测。

## 8. OpenRouter API fallback 冻结合同（按用户Magpie参考补充）

credentials owner在同一opencode模块新增：

```rust
pub fn read_opencode_openrouter_api_key(path: &Path)
    -> io::Result<Option<String>>;
```

只取精确顶层openrouter、type=api、key为非空字符串。缺文件/缺项/非object/非api/全空白key返回None；选中api但key缺失或坏类型、根JSON/IO错误返回Err。保留key原始字节，不遍历未知项。不开放任意providerID入参；openai OAuth读取与该函数共享同一确定路径。

**凭据顺序固定：aubridge已保存key > 现有KEY_OPENROUTER环境值 > OpenCode API key。** 保留原saved/env的判定语义，不把已配置但被远端拒绝的高优先key静默替换为另一账号；OC仅在高优先来源不存在时使用。该fallback与ChatGPT登录来源选择相互独立。

目标必须是registry内置OpenRouter preset，且ID、provider协议/base/auth、允许网络目标与内置官方定义匹配；custom/plugin不能因为同ID、同hostname或“兼容”名称取得OC key。文件中的地址/metadata不参与目标配置。新增来源标签 `key_source:"opencode"`，`key_set/ready`仅表示可读凭据，不表示上游可用。API不回传key。只读挂接，不复制进钥匙串、不写auth.json，不新增保存/导入按钮。

每次规划前观察当前有效凭据（仍按上述优先级）。有效key从旧值变为不同新值或消失时，推进现有connection_version，仅清理旧key相关的key_retry_at；不能清除用户暂停、Provider模型限流或其他非凭据状态。相同key、无关项/mtime变化、标签变化不重置冷却。首次观察只建立baseline，不能在没有旧值证据时清除持久化冷却。删除/损坏当前文件时不继续用旧OC key。

实现必须在route_plan读取routing_sources快照之前完成必要reconcile，dispatch发送前再次重读并核对凭据/version。当前dispatch持有versions读锁再resolve：不能在resolve内升级为versions写锁造成死锁；由state owner统一梳理这一最小锁顺序。新请求使用新key，新key观察后的旧请求401/403不得冻结新代次；日志仍按旧attempt记录事实。远端请求已经发出不会因删除文件自动撤销，下一次发送必须重新核对。

UI显示“来自OpenCode（只读）”；删除按钮只能清除aubridge自己保存的key，不能删除OC文件。清除saved key后回退到env/OC时，如实显示剩余来源并解释，不宣称凭据已完全移除。读取失败不伪装成保存key或真实零用量。

追加定向验收：默认/自定义XDG自动发现；API与OAuth同文件共存/无关项损坏；saved/env优先及被拒绝时不换key；OC key轮换/注销/损坏；旧401迟到不污染新key；只匹配内置官方目标；文件不写回。可用本机回放验证，不因新增API要求真实OpenRouter额外消耗。

settings.rs、两个OpenCode读取函数及OC刷新helper由credentials owner统一修改。ChatGPT/source/settings/Runtime/lib由OAuth gateway owner修改；新增API auth模块、state keys resolver/version相关块与route_plan刷新钩子由API gateway owner修改，state内避免整段覆盖并保持双方修改。Runtime只计算一次auth路径，分别注入OAuth通道和 `with_opencode_api_auth(PathBuf)`。刷新helper使用单独OC结果（access/refresh/id_token/expires_in）或经协商的等价可选字段扩展，必须保留原Codex JSON刷新行为与现有测试。

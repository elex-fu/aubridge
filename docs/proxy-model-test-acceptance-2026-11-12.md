# 代理页模型测试验收 · 2026-11-12

状态：功能、本机回放、代码门禁，以及隔离实例和正常桌面进程的真实调用通过；修复版已启动，原生按钮复核因窗口丢失及再次锁屏未完成。

设计先行，见 [模型测试方案](proxy-model-test-2026-11-12.md)；用户补充要求读取本地 Codex 鉴权后重试，修复见 [刷新依据修正](codex-auth-refresh-2026-11-12.md)。Astra 设计、6.1 Sol 实现、Luna 编译与验证。默认 800×480、最小 760×480、六页与既有历史筛选/分页保持。

## 已交付行为

- 代理页「测试模型」默认收起。显式选择自动、分组或具体模型，经 `/v1/models` 发现、`/v1/messages` 真实调用，客户端身份 `ui-test`。
- 固定短文本、512 输出上限、65 秒等待；单请求锁、取消等待、换页清理与迟到结果隔离。暂停时禁发，不代用户恢复或改配置。
- 完整非空 assistant 文本及明确终态才成功；HTTP 200 错误、空/仅思考/工具/缺终态不会误报。来源、响应模型缺失保留未知，不把 body usage 的默认零当成账本报告量。
- 显示请求模型、响应模型、来源、耗时及短回复；结果只在本页内存，不新增提示词/回复日志或持久化。完成后不自动重置历史筛选或已加载页。
- 后端仅对既有允许 Origin 暴露既有 `x-aubridge-provider`，未新增测试后端、放宽认证/Host/Origin 或将管理令牌用于推理。

## 分层验证结果

| 层次 | 实际结果与范围 |
|---|---|
| 前端分类合同 | 真实 `api.ts` 经 TypeScript 转译后由 Node 标准断言验证，HTTP 200 错误、空文本、仅思考、终态、未知来源与截断分类通过；仅算 helper 合同 |
| 本机生产路由回放 | 真实 UI helper／`/v1/messages`／本机上游 SSE，指定模型原样到入口，free 429→backup、分组、`ui-test` 账本与报告用量通过；不是厂商实测 |
| 生命周期与小窗 | 800×480、760×480，重复点击锁、暂停/恢复、取消等待、压缩时钟的 65 秒 watchdog、迟到保护、历史筛选/分页及横向边界通过；压缩时钟图不代表真实响应耗时 |
| CORS 与鉴权 | 原真实 HTTP 集成测试扩展后通过，推理身份独立、provider/expose、原 preflight、拒绝 Origin/rebinding、admin/inference 隔离保留 |
| 初次真实桌面请求 | `gpt-6.1-sol`，HTTP 502，UI 约 1.1 秒；对应历史总耗时 971ms，ChatGPT 被以缺凭据跳过，无上游 HTTP attempt，用量未知。发现误用 ID token 到期触发刷新，不判定模型本身不可用 |
| Codex 修复回归 | 两条真实 caller RED 后 GREEN；增加 opaque access 回归，最终工作区 358 项通过，0 失败，fmt/Clippy 通过 |
| 修复后隔离实例真实调用 | **HTTP 200，provider `chatgpt`，请求/响应模型 `gpt-6.1-sol`，回复 `OK`，耗时 2292ms**。该实例仅一条真实推理，账本 `ui-test/outcome=ok`、单次 chatgpt attempt，报告用量 10/5 |
| 最终原生窗口 | 修复包安装、签名与源/安装身份已核对。用户解锁后 CUA 会话重建并成功附着应用，模型选择时返回 `cgWindowNotFound`，之后 CUA 与系统只读状态均显示再次锁屏。应用 PID 67295、监听 127.0.0.1:62760，尚未点击测试或发出新的 GUI 推理；未继续 GUI 重试 |
| 正常桌面进程真实调用 | 实际安装应用 PID 67295、SHA 与最终包一致；其目录 HTTP 200、21 个模型、目标模型可选。一次真实 helper 请求 **`gpt-6.1-sol` → `chatgpt`，HTTP 200，响应模型同名，回复 `OK`，2002ms**；正常账本 `ui-test/ok`，一次 chatgpt attempt、报告用量 10/5。不是 GUI 按钮实测 |

后台真实调用使用新生产 CLI 与真实 `src/api.ts::testProxyModel`、真实 `fetch`；仅提供该实例 endpoint 的 IPC stub，未替代 fetch/Response、未直连上游代替代理。私有 HOME、配置、数据与端口隔离；生产 reader 引用用户原始 Codex 文件，未复制 OAuth 到源 Key、第三方源或日志。`share_login=false`；真实 `auth.json`、`config.toml` 内容与 mtime 前后一致。验证进程已停止，没有遗留验证网关。

解锁后的补充验证直接使用已运行的正常桌面进程 `127.0.0.1:62760`，没有新启动网关或重启应用。真实 `api.ts` 经转译加载，仅 stub `gateway_access` 返回 endpoint；目录和推理均使用真实 fetch。2026-11-13T05:30:38Z 发起一条请求，账本只读核对时间、模型、`ui-test` 及实际尝试；正常历史由这次生产请求自然产生，没有插入隔离结果。请求前后 PID、监听、二进制 SHA 一致；未读取 auth/admin token 或修改源、接线与配置。

## 命令、构建身份与证据

根目录 `/Users/lex/play/aubridge`；warm target `/private/tmp/aubridge-cycle93.0qTct6/target`，开发/测试 debug symbols 关闭。所有命令显式正确工作目录。

- `rtk npm --prefix crates/smoke/web run build`：RC 0，`index-C64faXYF.js`、`index-BixCZq8v.css`；Auth 后端修复未改前端，Tauri hook 同样构建通过。
- `rtk cargo test --offline --locked -p gateway --test integration cors_only_admits_the_desktop_origin_with_the_right_key -- --exact`：修复夹具后 1 项通过。最终 CORS integration Clippy 补验 RC 0。
- Auth 两条 `ready_*` 精确用例：旧生产各运行 1 项，均断言失败 RC 101；GREEN caller 3 项、凭据 9 项、既有网关认证边界 4 项通过。
- `rtk cargo fmt --all -- --check`、`rtk cargo clippy --offline --locked --workspace --all-targets -- -D warnings`、`rtk cargo test --offline --locked --workspace --all-targets`：最终均 RC 0，20 suites、358 passed、0 failed、0 ignored。
- `crates/smoke` 下 `rtk proxy cargo tauri build --debug --bundles app --ci -- --offline --locked`：RC 0；完整本机 ad-hoc 签名、`codesign --verify --deep --strict` 及源/安装完整 SHA manifest 比对均 RC 0。
- Auth 后 `rtk proxy cargo build --offline --locked -p gateway --bin aubridged`：RC 0。后台真实 runner 的精确命令与分层输出见下列证据，不使用旧 CLI。
- `rtk node /var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n/codex-auth/desktop-backend.mjs`：RC 0；真实桌面进程目录及一次指定模型调用通过，完整命令和回执独立留证。本次仅补实机运行环境验证，没有重跑已通过的编译与测试。

最终已安装 `/Users/lex/Applications/aubridge.app`，桌面入口不变；executable SHA `0fe54c9f6f446a909d56c1d3981f06e6873edcf1951f334412119b005ed85b45`。Auth 后 CLI SHA `ec1faffa3d6e30cc8a59cd45884b7b941e698606adbb92e59ad4b33375d0bdc0`。旧版本完整备份保留。

证据根目录：`/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n`：

- `probe-ui-checkpoint.md`、`proxy_model_ui_check.py`、`proxy_model_contract_check.mjs`：具体命令与本机回放回执。
- `probe-native-ui.log`：首次真实桌面失败及精确历史只读归因。
- `codex-auth/auth-red-1.log`、`auth-red-2.log`、`04`–`09` 系列：真实 RED/GREEN、最终门禁。
- `codex-auth/native-retry.log`：精准退出及首次锁屏阻碍；其中安装 SHA 的 `e0fe…` 为抄写错误，以安装清单与上文 `0fe54…` 为准。
- `codex-auth/native-unlock-followup.log`：解锁后附着、窗口丢失及再次锁屏；保留备用日志，并注记完整 stage 路径实际仍存在，不能据路径访问误判为系统删除。
- [真实后台结果](/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n/codex-auth/real-codex-background-result.json)、[精确命令](/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n/codex-auth/real-codex-background-command.txt)：真实上游与账本证据，凭据值未记录。
- [正常桌面后端结果](/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n/codex-auth/desktop-backend-result.json)、[精确命令](/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-proxy-model-test-j_k02p8n/codex-auth/desktop-backend-command.txt)：实际安装包进程、目录、唯一新增请求与正常账本；不证明 GUI 点击。

## 首次失败与恢复边界

初次 CORS 回放错误返回 JSON，生产非流式 Messages 仍先读上游 SSE，故得到 502；改为既有完整 SSE 并保留真实断言，生产只改 expose header。格式检查首错也保留。早期 UI 夹具误用 repo/target 旧 CLI、跨 Origin 桥、压缩时钟与回放延迟、响应模型期望和滚动位置断言逐项修正；旧二进制的运行不计入当前验收。最终正确回放 CLI 为 auth 修复前的 `41067e8a…ceaea5`，仅证明该阶段前端/CORS/一般路由，不冒充 Auth 修复验证。

后台真实 runner 初次参数错误未启动网关、未发推理；修正后只发出 1 条真实调用。Kimi runner 仅语法检查，收到用户改用 Codex 后停止，没有发出任何 Kimi 推理。

本次真实成功限定当前账号、模型、时刻，以及已分别留证的隔离和桌面运行环境，不承诺持续健康或其他模型/工具能力。修复版图形按钮仍未完成复核；正常应用继续运行。0097 旧托盘交互仍 BLOCKED、累计 4 / 10 不变。

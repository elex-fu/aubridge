# OpenCode 鉴权文件接入验收 · 2026-11-13

状态：本专项的功能实现、386 项回归、UI 回放、真实 access 的 OpenCode 格式兼容与真实客户端调用、最终安装均 PASS。原生 OpenCode 登录凭据实测单列待验，2026-11-17 再次核对默认文件仍不存在。设计见 [OpenCode 登录自动读取](opencode-auth-design-2026-11-13.md)。本专项不重算迭代轮数；0097 当前状态以该轮页首为准。

2026-11-17 状态校正：下文引用的 0097 BLOCKED / 4 of 10 均为当时记录。
0097 已在 2026-11-14 完成独立托盘验收，当前为 PASS / 5 of 10，见其页首汇总。
本专项的原生 OpenCode 登录材料及真实续期验收仍独立待完成。

## 交付范围

- 沿用 Magpie 的自动定位规则：绝对 `XDG_DATA_HOME/opencode/auth.json`，否则 `HOME/.local/share/opencode/auth.json`。不依赖可执行文件存在，不扫描猜测路径。
- 内置 OpenRouter 的 API key 来源为 aubridge 已保存 Key → 进程环境 → OpenCode 文件；文件按请求重读，轮换只清旧 key 的凭据冷却，删除或损坏后停止使用旧 key。OpenCode key 不复制到钥匙串，仅允许固定官方目标。
- 内置 ChatGPT 通道可显式选择 Codex / OpenCode，默认保留 Codex；所选文件变化自动生效，失败或注销不切到另一登录。OpenCode `openai` OAuth 保留毫秒到期值，使用官方 form 刷新，只更新来源绑定的内存凭据。
- 设置页显示来源、读取状态与发现路径；OpenCode 来源不启用 Codex 写回/还原。OpenRouter 文件 Key 显示只读来源。其他 Provider 的 API/OAuth、本轮没有纳入的环境 JSON 均不宣称支持。

## 实施与验证责任

Astra 定稿；6.1 Sol 分别负责 credentials、ChatGPT gateway、OpenRouter key gateway、前端；Luna 独占构建槽并分别执行代码门禁、UI/真实入口验证。保留共享工作区已有未提交改动，不自动提交或推送。

## 证据位置

- 构建与回归：`/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-opencode-auth-evidence.T72EJYL8U8`。
- 生产入口/真实上游准备：`/tmp/aubridge-opencode-auth-9kd03w`。
- 当前原生 OpenCode 文件条件：自动计算的默认 `~/.local/share/opencode/auth.json` 不存在，当前未设置 `OPENCODE_AUTH_CONTENT`；已按用户补充核对 Magpie 源码，其规则也没有额外 macOS 路径。不能据目录发现规则推断存在可用登录。

原生 OpenCode 登录实测与“已有真实 access 的私有 OpenCode 格式兼容验证”分开记录；不创建用户认证文件、不自动改变用户现有来源，真实文件不输出 token、账号或完整 JSON。

## 实际命令与结果

### 定向读取与来源检查：PASS

以下均使用 warm target `/private/tmp/aubridge-cycle93.0qTct6/target`、离线锁定依赖与 debug=0 profile；数字来自实际非零 test result，不把 filter 空匹配计为通过。

| 命令（均含 `--offline --locked`） | 结果 | 日志 |
|---|---|---|
| `rtk cargo test -p credentials opencode::tests` | 12 passed | `02-credentials-opencode.log` |
| `rtk cargo test -p credentials settings::tests` | 8 passed | `03-credentials-settings.log` |
| `rtk cargo test -p gateway --lib chatgpt::tests` | 首阶段 7 passed | `04-gateway-chatgpt-core.log` |
| `rtk cargo test -p gateway --test opencode_auth` | 首阶段 3 passed | `05-gateway-opencode-auth.log` |
| `rtk proxy env -u KEY_OPENROUTER cargo test -p gateway --lib opencode_keys` | 6 passed | `06-gateway-opencode-keys-lib.log` |
| `rtk cargo test -p gateway --test opencode_keys` | 1 passed | `07-gateway-opencode-keys-integration.log` |
| `rtk cargo test -p gateway --test integration codex_` | 7 passed | `08-gateway-integration-codex.log` |

首阶段 lib test 输出过一处 unused import warning，Sol 已最小删除；不会用有 warning 的输出冒充最终 `-D warnings` 门禁。

### UI 回放：PASS

最终前端 `index-JVhb63m3.js` SHA-256 为 `3791e3860ed5560a9c295ce696b2589cfadd2c2353714132acfba81d0502d34e`，构建 RC 0。隔离 HTTP stub/CDP 回放 RC 0，检查来源下拉仅修改草稿、应用只 POST 来源、不发推理、保存失败保留旧来源、未知/缺失/不可读状态不误报、OpenCode 只读且禁用 Codex 共享/恢复、OpenRouter 保存/清除后正确回退且不回显 key。800×480 与 760×480 无横向溢出；root 另查看了 800×480 设置页失败/未知状态截图。

[回执](/private/tmp/aubridge-opencode-auth-9kd03w/opencode-auth-ui-receipt.json)、[脚本](/private/tmp/aubridge-opencode-auth-9kd03w/opencode_auth_ui_check.py)、[检查点](/private/tmp/aubridge-opencode-auth-9kd03w/ui-probe-checkpoint.md)。这是 UI 回放，不是原生登录或原生窗口验收。

### 补充回归与错误假设更正

root 曾怀疑 OAuth 401 会将新登录冻结在 API Key 冷却，要求增加生产入口断言。`17-red-opencode-file-rotation.log` 确实运行 1 项并 RC 101，但失败发生在测试自己的错误前提 `key_retry_at.is_some()`，尚未到热更新业务断言。生产代码只有 `sent.uses_key` 为 true 时才写 API Key 冷却，OAuth 为 false。`18-red-inflight-401.log` 同时运行 1 项并 PASS，不能称“两条产品回归 RED”。

17 独立保留为 `INVALID_TEST_ASSUMPTION`。测试已纠正为 OAuth 401 不写 API Key 冷却、原子换入新 access 后下一普通请求成功；未引入所猜测的路由版本框架。另补同一来源重复提交的幂等保护，保留只读模式已刷新的内存凭据，并在原刷新测试中验证。最终定向/工作区门禁、CLI/安装包身份和真实入口结果继续在下文追加；中间 `7639cd…bb5c3` CLI 与中间 source app 不作最终发布验收。

### 鉴权功能候选代码门禁与 source 安装包：PASS

最终证据目录 `/private/tmp/aubridge-opencode-auth-final-20261113-51123/`，对应同源选择幂等修复和纠正后的回归。

| 命令/检查 | 实际结果 |
|---|---|
| `rtk cargo test --offline --locked -p gateway --test opencode_auth` | 4 passed，RC 0；包含 401 后文件轮换和在途切源 |
| `rtk cargo fmt --all -- --check` | RC 0 |
| `rtk cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | RC 0 |
| `rtk proxy env -u KEY_OPENROUTER cargo test --offline --locked --workspace --all-targets` | RC 0，22 个 test targets，386 passed / 0 failed / 0 ignored；含 1 个无测试的 binary target，不把它算作额外测试 |
| `rtk cargo build --offline --locked -p gateway --bin aubridged` | RC 0，新 CLI SHA 见下 |
| `crates/smoke` 中 `rtk proxy cargo tauri build --debug --bundles app --ci -- --offline --locked` | RC 0，33.00 MiB source app；标准前端 hook 构建通过，JS 与 UI 回放一致 |
| 本机 ad-hoc 签名与 `codesign --verify --deep --strict` | 均 RC 0；source manifest 已保存 |

root 直接读取最终文件再次核对 SHA：

- CLI `/private/tmp/aubridge-cycle93.0qTct6/target/debug/aubridged`：`35829dcff545ce3dbbe113a094ddca7e7c60a6748175a409107093608a8d8ae3`。
- Source bundle `/private/tmp/aubridge-cycle93.0qTct6/target/debug/bundle/macos/aubridge.app`，executable `Contents/MacOS/smoke`：`44a88a6c1f6496c9552456cee86889112e59bbe467a9e796bc95142c1f18c49e`。
- Bundle ID `dev.aubridge.smoke`，图标 `icon.icns`，最终 JS 保持 `3791e386…2d34e`。
- 完整 source 清单为最终证据目录内 `09-source-files.manifest`、`09-source-hashes.manifest`；本段只证明 source 包，安装后身份与运行身份另记。

### 真实入口与安装

#### 安装与启动：PASS

- 旧 PID `67295` 的两次短间隔检查均无 ESTABLISHED TCP，精确 SIGTERM 后退出；旧包备份在最终证据目录 `09-installed-before.app`。
- 新包安装至 `/Users/lex/Applications/aubridge.app`；完整清单 4/4 文件 SHA 与 source 一致，strict codesign RC 0，桌面 symlink 保留。
- 新运行 PID `54774`，监听 `127.0.0.1:62760`，executable SHA 与最终 source 的 `44a88a6c…8c49e` 一致；只读 `/v1/models` 返回 HTTP 200、21 个模型，无推理。
- 当前真实数据目录为 `/Users/lex/.aubridge`，登录来源字段缺省为 Codex。安装前没有保存此字段快照，不冒称完成了配置字节前后对比；没有手动保存来源或读取 auth/key。
- [安装回执](/private/tmp/aubridge-opencode-auth-final-20261113-51123/10-install-receipt.md)。没有使用不存在的 in-flight API；只将连接检查作为本次更新证据，没有将账本无新记录推导为零在途。

#### 第一条真实兼容调用：网络阶段失败，独立保留

使用最终 CLI SHA `35829d…d8ae3`，私有生产网关 PID `54002`、source=OpenCode，缓存 synthetic 登录的 preview 通过。把真实 Codex access 转换为私有 OpenCode schema（refresh 清空、到期余量已检查）后，真实 `api.ts` helper 发出请求，约 `10016ms` 返回本机 HTTP 502。私有 ledger rowid 1 为 `ui-test/no-candidate`，唯一 chatgpt attempt `status=0/reason=network-error`、约 `10003ms`，没有上游 HTTP 响应；usage 未知，不是缺凭据跳过。

因此真正 OpenCode CLI 和注销阶段尚未执行；没有把预览或解析成功当作真实上游成功。私有进程退出 0，auth/session/runtime/client log 已清理，原始 Codex auth/config 比较未变。此处的凭据仍属于“真实 access 的 OC 格式兼容验证”，不是原生 OpenCode 登录。

[首条真实结果](/tmp/aubridge-opencode-auth-9kd03w/opencode-hot-reload-result.json)、[命令](/tmp/aubridge-opencode-auth-9kd03w/execution-command.txt)。继续仅做无凭据网络诊断；若有明确工作路径后重验，使用独立新回执，不覆盖此失败。

#### 无凭据网络诊断与最小兼容修复

独立诊断未读取 auth、未发推理、未更改系统代理/VPN：本机系统 HTTP/HTTPS/SOCKS 静态代理均为 `127.0.0.1:7890`，由 `fastcatCore` PID `1237` 监听，PAC 未启用。直连官方 Responses 地址的 HEAD 请求完成 DNS 后 TCP/TLS 未建立，15 秒超时、HTTP 0；显式经该现有代理则 TLS 建立，总计约 3.6 秒并收到 HTTP 405。405 只说明目标方法不允许，证明传输可达，不代表授权通过。

源码确认 workspace reqwest 使用 `default-features=false`，同时没有声明其默认包含的 `system-proxy`；当前网关也无代理环境变量。Sol 仅恢复官方 `system-proxy` feature，保留 rustls 和既有版本，不自写代理扫描或修改用户系统配置。本机缓存中的 hyper-util 实现支持读取 macOS 静态系统代理；后续以无 HTTP/HTTPS/ALL_PROXY 环境变量的生产子进程验证，不用测试代理注入掩盖实际行为。该 feature 的必要锁文件变化和最终重建结果另记。

[诊断回执](/tmp/aubridge-opencode-auth-9kd03w/connectivity-diagnostic-v2.json)、[实际命令](/tmp/aubridge-opencode-auth-9kd03w/connectivity-diagnostic-v2-command.txt)。首条真实网络失败仍保留，前述已安装版本 `44a88…` 不包含这一兼容修复；最终安装身份将再次独立记录。

#### 系统代理兼容后的最终门禁：PASS

最终证据目录 `/private/tmp/aubridge-system-proxy-final-20261113-59590/`。Cargo.lock 仅新增 `system-configuration 0.7.0`、`system-configuration-sys 0.6.0`、`windows-registry 0.6.1`，既有依赖没有版本升级。root 对照修改前快照和完整 diff 已核实。

初始 locked metadata 要求必要锁更新，随后全平台 offline metadata 因已有 Windows-only `webview2-com-sys 0.39.1` 缓存缺失失败；原日志 `01`、`07` 保留。使用实际 macOS host 的 `cargo metadata --offline --locked --format-version 1 --filter-platform aarch64-apple-darwin` 后 RC 0，不为本机验收下载无关 Windows 全套依赖。

- `cargo fmt --all -- --check`、工作区 `clippy --all-targets -- -D warnings`、`test --workspace --all-targets` 最终均 RC 0；后两项使用离线/locked 依赖。合成网络夹具以本机 NO_PROXY 隔离，不改变系统配置。
- `11-cargo-workspace-host-final.log`：22 test targets、386 passed、0 failed、0 ignored，root 直接解析汇总复核。
- 新 CLI SHA-256：`c078af3e14309da0e65575edb7d1abfebc09e7a6554adb6354c096b56e93bbef`。
- 最终 Tauri executable SHA-256：`684edc04920aaf294c2e6f41be16e2ae2156fbe505f286f241c5e9c222e1abed`；构建、ad-hoc 签名和 strict verify 均 RC 0。
- JS SHA 保持 `3791e3860ed5560a9c295ce696b2589cfadd2c2353714132acfba81d0502d34e`，此前 UI 回放适用于最终前端；没有重复无变更的 UI 检查。
- 完整 source 清单：`15-source-files.manifest`、`15-source-hashes.manifest`；具体命令与退出码见本目录 `09`–`15` 日志。

#### 无额外代理环境变量的真实兼容与客户端验证：PASS

执行命令独立留证：

```sh
rtk proxy node /tmp/aubridge-opencode-auth-9kd03w/opencode-hot-reload-runner-v3.mjs --run --cli /private/tmp/aubridge-cycle93.0qTct6/target/debug/aubridged --cli-sha c078af3e14309da0e65575edb7d1abfebc09e7a6554adb6354c096b56e93bbef --result /tmp/aubridge-opencode-auth-9kd03w/opencode-hot-reload-result-v3.json
```

同一私有生产网关 PID `66787`、source=OpenCode，始终不重启，Codex 文件为 synthetic 对照防止误回退。没有注入 HTTP_PROXY、HTTPS_PROXY 或 ALL_PROXY，使用本机既有系统代理配置。先以 sentinel 观察缓存，再原子换入真实 access 的私有 OC 格式（refresh 清空、真实到期余量检查通过），普通推理前不插入管理页或目录读取。

| 阶段 | 实际结果 |
|---|---|
| 真实 `api.ts` helper | `gpt-6.1-sol`，HTTP 200，provider/response model 为 `chatgpt` / `gpt-6.1-sol`，文本 `OK`，helper 耗时 10151ms；ledger rowid 1、`ui-test/ok`，报告 input/output `10/5` |
| 真正 OpenCode 1.18.5 客户端 | exit 0、文本 `OK`、总耗时 6400ms；ledger rowid 2 为 `opencode/ok`、chatgpt HTTP 200，报告 input/output `1966/5` |
| 客户端附属请求 | 同一实际客户端还产生 rowid 3，chatgpt HTTP 200 后客户端取消，结局 `cancelled`，用量 `null/null`；不把附属请求隐去、不把未知记为零，也不宣称客户端只发一条请求 |
| 移除私有 OC 登录 | 下一普通请求本地 HTTP 502，约 5ms；ledger rowid 4 `attempts=[]`，明确跳过 `chatgpt/missing-credential`，没有使用 Codex 对照或发上游请求 |
| 清理与文件 | 私有网关退出 0，原始 Codex auth/config 内容 hash 比较未变，私有 auth/session/runtime/client log 已清理；root 再次核对私有 PID 已退出 |

客户端 binary `/private/tmp/aubridge-cycle93.0qTct6/opencode/package/bin/opencode`，版本 `1.18.5`，SHA `45922f63cf068f5c72d44cf18d1cde9816f359668258608b0272e54c304106c1`。本次共 4 条推理入口 HTTP：helper 成功一条、真实客户端成功一条及取消附属一条、注销后本地拒绝一条；不将首条失败回执并入这一成功链。

[v3 回执](/tmp/aubridge-opencode-auth-9kd03w/opencode-hot-reload-result-v3.json)、[v3 实际命令与备注](/tmp/aubridge-opencode-auth-9kd03w/opencode-hot-reload-v3-command.txt)。脚本运行完成后仅为未来回环隔离补了显式 NO_PROXY，命令文件已说明这一后续修改，没有重复请求，也不宣称本次已设置该变量。

#### 最终安装与运行：PASS

最终包安装至 `/Users/lex/Applications/aubridge.app`。前一 `44a88…` 包保留为最终证据目录中的 `16-installed-before.app`；两次空 ESTABLISHED 检查后精确终止旧 PID `54774`，一次 `open -g` 启动新版。

- 新 PID `67713`、`127.0.0.1:62760`；实际运行与安装 executable SHA 均为 `684edc04920aaf294c2e6f41be16e2ae2156fbe505f286f241c5e9c222e1abed`，root 实际读文件与 ps 再核对。
- 完整 source/installed manifest 4/4 文件一致，strict ad-hoc codesign RC 0，Desktop symlink 正确。
- 只读 `/v1/models` HTTP 200、21 模型；安装流程没有发推理或取得 admin token。
- 安装前后只读来源标记均为 `codex`；用户实际数据目录仍为 `/Users/lex/.aubridge`，没有替用户切换登录来源。旧 QA controller PID `31280` 仍运行，未动其数据与证据。
- [最终安装回执](/private/tmp/aubridge-system-proxy-final-20261113-59590/17-install-receipt.md)。这是安装身份、运行身份与目录健康，不冒充原生窗口按钮验收。

### 原生 OpenCode 登录凭据：BLOCKED（独立验收项）

root 最终再次仅检查自动计算的 auth 路径，文件仍不存在。本次真实成功使用已授权的本机 Codex access 转为私有 OpenCode 格式，完整验证了新读取链路与真正 OpenCode 客户端，但不等于真实 OpenCode 登录材料、真实 refresh-token 轮换或所有 Provider 均已验收。

恢复条件：在 OpenCode 完成原生登录，使其标准 XDG auth 文件存在；设置中选择 OpenCode 后，以正常客户端请求补一次原生材料实测。程序会自动定位并读取，不需要填写路径、导入 token 或重启来读取更新后的文件。此项外部条件不改旧迭代 0097 BLOCKED / 4 of 10。

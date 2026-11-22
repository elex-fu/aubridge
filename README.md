# aubridge

> aubridge 只做两件事:把**厂商官方免费档**和**你自己付费的订阅**,统一
> 代理给你本机的任意 agent 使用。所有凭据保存在你自己的设备上,不经过
> 任何第三方服务器;不支持多用户、不支持把额度转发给他人。**零遥测——
> 不上报任何用量、错误或统计,所有数字仅在你的本地生成。**社区免费模型
> 源以插件形式提供,默认全部关闭,由用户自行评估开启。

本地运行的桌面客户端,把两类资源汇成一个池,统一代理给本机任意
coding agent 使用:

1. **厂商官方免费档**——各平台免费模型(OpenRouter `:free`、GLM-Flash、
   ModelScope 等),单家不够,集成叠加够日常编码;
2. **用户已付费的订阅**——Claude Pro、ChatGPT Plus、各家 Coding Plan
   (Kimi Code、GLM Coding Plan、Qwen Coding Plan 等)。实际计费取决于厂商套餐与用户设置；
   例如 Kimi Code 启用 Extra Usage 后，套餐耗尽可能另扣余额。跨 agent 复用不等于免费。

同一分组里的可用免费源优先于订阅源。网关会在首段有效内容发送前切源，
已发送内容后的断流则由当前流报告错误。
同一源的不同模型可分别尝试。模型限流只冷却该模型，账号额度耗尽时跳过
该源的全部模型。代理页可查看各次尝试及跳过原因。

订阅分组可选择 Smart（较早重置先用）或 Pace（每小时可用比例较多先用），
均保留免费优先。后台持续采样额度，窗口分别显示读数时间和重置时间；
过期或身份已变化的读数不参与额度排序，未知额度仍可作为候补。

推理请求体上限为 32 MiB，支持 identity、gzip 和 zstd；解压后同样受限。
读取与解码最多 60 秒，入口容量占用时返回 503 和重试提示。完整响应或取消后归还占用。

用量保留缓存读取/创建的报告状态，界面展示计价覆盖率、缓存命中率与 API 等值估算。
未知价格保持空金额，订阅费未扣除。代理页可按本机请求 ID 查找并复制脱敏诊断。

## 快速开始(源码运行)

前置为 Rust stable 和 Node.js 20+。在项目根目录运行。

```sh
npm --prefix crates/smoke/web ci
npm --prefix crates/smoke/web run build
cargo run -p smoke
```

桌面壳启动同进程的回环网关。先在 Provider 页保存 key，再到 Agents 页接线 Claude Code、OpenCode、Codex CLI 或 Gemini CLI。
Provider 页可按需发送短请求检测连接。OpenRouter 的 key 预算与免费请求状态分开展示。
可在 Provider 页刷新 OpenRouter 免费模型，再到 Agents 页同步 OpenCode 模型列表。
Provider 的“厂商目录与能力”显示上下文、输出、工具、推理与免费依据；订阅可选择目录内模型。
新鲜目录确认模型可用性，读取失败保留上次结果，未知能力保持未知。
代理页显示实际监听地址。首次使用 `3427`，被占用时由系统分配端口。
重启时优先复用上次端口。换端口时会更新仍指向原受管地址的接线配置。
关窗后网关继续运行，可从系统托盘重新打开窗口或退出。断开前可预览恢复操作。
手动接线时，使用实际地址作为 `ANTHROPIC_BASE_URL`，推理 token 为 `aubridge-claude-code`。
OpenCode 接线后通过 `opencode models aubridge` 查看可用模型和分组。
Codex CLI 接线后用 `codex --profile aubridge` 启动。该 profile 使用本机 HTTP Responses，
可在 Agents 页选择模型或分组。最近真实验收版本为 `0.169.0-alpha.11.1`。

命令行启动可用 `cargo run -p gateway --bin aubridged`。命令行读取当前目录的
`.env`，实际地址和本次管理 token 写入 `~/.aubridge/gateway-session.json`。
开发时可以在 `.env` 中填写 `KEY_OPENROUTER`、`KEY_MODELSCOPE` 或 `KEY_KIMI_CODE`。
Unix 下会话文件权限为 `0600`，正常退出时自动清理。

设置页和 `aubridged status` 从当前网关读取相同的构建身份：版本、commit/tree、
构建模式、profile 与已实现能力。源码运行显示 `source_verified=false`，不代表
已通过干净发行门禁；安装器将新包的运行身份与发行回执逐项核对。
Agents 页支持 OpenCode 固定 CLI 测试；V2 使用 standalone 和临时数据目录，
在受管模型上收到预期文本且无 JSON 错误才通过。完整真实免费工具任务与
受控限流回放的验收范围见 [0104](docs/iterations/0104.md)。

订阅源(Codex/ChatGPT):本机已 `codex login` 后默认只读使用;在设置页
开启"与 CLI 共享登录"才会把刷新得到的 token 写回 CLI 凭据文件(开启时
自动备份,可一键还原)。

## 设计与文档

`docs/` 为仅保留在本机的临时资料，不随远端提交。以下及文中相关链接仅供本地查阅；全新克隆没有该目录属于正常情况。

- `docs/product-design.md` —— 唯一权威方案(含变更记录)
- `docs/crate-layout.md` —— 工程结构与 crate 分层
- `docs/engineering-constraints.md` —— 已拍板的工程约束
- [Magpie 功能对齐与 macOS 小白交付路线图](docs/magpie-parity-roadmap-2026-11-19.md) —— 17 项固定口径、体验目标与工作包（已授权开发，统一候选尚待验收）

## 状态

开发中。客户端支持 Claude Code 的 Messages 和 `count_tokens`、OpenCode 的 Chat Completions、
Codex CLI 的流式 HTTP Responses，以及 Gemini CLI 的 Google 原生 generate/stream 与本地估算 `countTokens`。
0110 开发包已通过完整门禁并安装，运行身份与回执一致；官方 Gemini CLI 0.68.0 在已安装 App 上的
显式选模、真实工具回填和账本对账通过，最终已恢复自动模型并通过 UI 测试，见 [0110](docs/iterations/0110.md)。
前批次验收见 [0104](docs/iterations/0104.md)。正式 Developer ID/公证、三平台公开发行与自动更新仍待交付。
Claude 订阅 CLI 来源默认关闭，真实订阅验收按用户决定延期。

Gemini CLI 保留官方环境文件优先级：项目 `.env` 可能遮蔽 HOME 中的接线，本轮私有 cwd 验收不覆盖此情况。
用户可另选 `advanced.ignoreLocalEnv=true`，但会忽略项目环境文件；本轮未启用该可选设置。

## 源码门禁与 macOS 发行

`scripts/check.zsh` 从锁文件安装前端依赖，再执行前端构建、格式检查、
严格 Clippy 和工作区全目标测试。它可在有或没有 RTK 的环境中运行。

```sh
zsh scripts/check.zsh
```

提交源码后，固定 commit 的干净检出和打包可独立执行。打包先执行相同
完整门禁，再在独立 target 中构建；回执包含 commit/tree、工具链、归档
SHA 和全量 manifest。需要 Python 3.9+、Xcode Command Line Tools 及
Tauri CLI 2.12.4。Rust/Node 版本已固定。

```sh
python3 scripts/check-clean.py
python3 scripts/release-macos.py build --mode development
```

从应用菜单退出 aubridge，等待在途请求完成，再用上一步输出的回执安装。
安装器拒绝替换运行中的应用，核对归档和应用后再交换文件。健康检查绑定
启动 PID、可执行文件和监听 socket；失败会恢复并重启旧包，中断可 recover。
凭据、用户配置和账本沿用原数据目录。

```sh
python3 scripts/release-macos.py install .local/releases/<build>/receipt.json --allow-development
python3 scripts/release-macos.py verify .local/releases/<build>/receipt.json --app ~/Applications/aubridge.app
python3 scripts/release-macos.py recover
```

`--mode distribution` 要求 Developer ID、固定 Team 和实际公证验证，缺环境
明确失败。本机尚无正式证书；先完成流程，证书随后接入。GitHub 已有三平台
源码 CI 和手动 macOS 打包 workflow 定义，执行状态需单独确认。三平台公开
发行、自动更新与零断流交接仍待交付。完整接入与恢复步骤见
[发行说明](docs/releasing.md)。
首次安装依赖需要 npm registry 可达；受限网络下可为本次命令设置
`npm_config_https_proxy` 指向已有代理。构建回执保留失败日志，锁文件不随重试改变。

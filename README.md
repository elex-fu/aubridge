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
   (Kimi Code、GLM Coding Plan、Qwen Coding Plan 等),订阅费已花,
   跨 agent 复用不产生额外计费,等于免费。

同一分组里的可用免费源优先于订阅源。网关会在首段有效内容发送前切源，
已发送内容后的断流则由当前流报告错误。
同一源的不同模型可分别尝试。模型限流只冷却该模型，账号额度耗尽时跳过
该源的全部模型。代理页可查看各次尝试及跳过原因。

## 快速开始(源码运行)

前置为 Rust stable 和 Node.js 20+。在项目根目录运行。

```sh
npm --prefix crates/smoke/web ci
npm --prefix crates/smoke/web run build
cargo run -p smoke
```

桌面壳启动同进程的回环网关。先在 Provider 页保存 key，再到 Agents 页接线 Claude Code、OpenCode 或 Codex CLI。
Provider 页可按需发送短请求检测连接。OpenRouter 的 key 预算与免费请求状态分开展示。
可在 Provider 页刷新 OpenRouter 免费模型，再到 Agents 页同步 OpenCode 模型列表。
代理页显示实际监听地址。首次使用 `3427`，被占用时由系统分配端口。
重启时优先复用上次端口。换端口时会更新仍指向原受管地址的接线配置。
关窗后网关继续运行，可从系统托盘重新打开窗口或退出。断开前可预览恢复操作。
手动接线时，使用实际地址作为 `ANTHROPIC_BASE_URL`，推理 token 为 `aubridge-claude-code`。
OpenCode 接线后通过 `opencode models aubridge` 查看可用模型和分组。
Codex CLI 接线后用 `codex --profile aubridge` 启动。该 profile 使用本机 HTTP Responses，
可在 Agents 页选择模型或分组。验收版本为 `0.167.0-alpha.3.2`。

命令行启动可用 `cargo run -p gateway --bin aubridged`。命令行读取当前目录的
`.env`，实际地址和本次管理 token 写入 `~/.aubridge/gateway-session.json`。
开发时可以在 `.env` 中填写 `KEY_OPENROUTER`、`KEY_MODELSCOPE` 或 `KEY_KIMI_CODE`。
Unix 下会话文件权限为 `0600`，正常退出时自动清理。

订阅源(Codex/ChatGPT):本机已 `codex login` 后默认只读使用;在设置页
开启"与 CLI 共享登录"才会把刷新得到的 token 写回 CLI 凭据文件(开启时
自动备份,可一键还原)。

## 设计与文档

- `docs/product-design.md` —— 唯一权威方案(含变更记录)
- `docs/crate-layout.md` —— 工程结构与 crate 分层
- `docs/engineering-constraints.md` —— 已拍板的工程约束

## 状态

开发中。客户端支持 Claude Code 的 Messages 和 `count_tokens`、OpenCode 的 Chat Completions，
以及 Codex CLI 的流式 HTTP Responses。
三平台打包和自动更新待交付。每轮实施与验收记录见 `docs/iterations`。

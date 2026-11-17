# 工程约束(开发前已拍板)

> 状态:已定(2026-11-03)
> 上游:`docs/product-design.md` §6/§10、`docs/crate-layout.md`
> 分级:A = M0 收尾即执行;B = M1 开工前生效;C = 留约束位,触发时执行

## A. M0 即执行(阻塞 F-1~F-11)

1. **proto envelope 补全**:`message_start` 带 `message{id, model, role}`,
   serde 从宽;M0 按协议文档构造,M1 录真实 Claude Code 流复核字段集;
2. **usage 数据源**:请求翻译默认注入
   `stream: true` + `stream_options: {include_usage: true}`——F-2 断言
   的前提,漏了整条断言空转;Chat 流末 `data: [DONE]` 是流终止标记,
   不算坏 JSON;
3. **tools 翻译纳入 M0 范围**:`tools`(anthropic input_schema → chat
   function parameters)、`tool_choice`(auto→auto、any→required、
   tool→具名对象)、`max_tokens` 映射;新增 **F-11(tools 往返保真)**
   用例——缺了它 Claude Code 发不出任何工具调用请求;
4. **断言格式**:golden 事件序列(内联 `Vec<AnthropicEvent>` 精确相等)
   防漂移 + F 用例结构断言(保序/存在性)可读,两者并用;SSE 层
   加"frame→parse 往返"用例;
5. **语料纪律**:M0 fixture 手工构造,文件头首行
   `: synthetic v0 (待真实语料替换)`;M1 建录制器(dump 上游流)后
   逐个替换;
6. **错误处理**:M0 翻译 API 无失败路径,不引入 thiserror;首个需要
   `Result` 的 API 出现时引入(允许进 proto/translate,需同步修订
   crate-layout 的 proto 依赖白名单);错误信息不得包含密钥与原始 body。

## B. M1 开工前生效(工程架构)

7. **数据目录**:`~/.aubridge/{config.json, stash/, logs/, plugins/,
   ledger.sqlite}`,配置 JSON + 原子写(temp+rename);config.json 是
   共享文档(端口 / share_login 等字段各有写入方)——**字段级合并
   写入,绝不整体覆盖**(2026-11-04,变更记录 25);stash/ 为"与
   CLI 共享登录"开启时的凭据备份(mtime + sha256,一键还原);
   Unix 上的凭据备份、还原和刷新写回用 `0600` 原子文件。启动时收紧已存在的本机备份权限。
   本机配置无效时保留原文件字节，使用本次监听端口并暂缓自动接线修复。配置有效后可正常保存。
   Unix 上主账本及其 SQLite 日志使用 `0600`，打开旧账本时收紧其已有文件权限。
8. **rest 持久化分期**:M1 内存态(重启丢失 = 重新探测,可接受);
   **M2 已落地(2026-11-04,变更记录 24)**——router 运行态(rest/
   sink/爬梯/熔断/利用率 + 亲和表)write-behind 落 ledger SQLite
   (`router_state` 表,5s 周期合并写),设计文档 §8.1 的"冷却持久化"
   与 §8.4 的"亲和表持久化"即此;
9. **回放服务器**:fixture 驱动的 fake Chat 上游(读语料回放 SSE,
   可注入 429/5xx/截断),放 `gateway/tests/support/`;是 M1 验收
   (真实 Claude Code 会话)的唯一可控途径,gateway 测试结构第一天
   为它留位;
10. **日志**:`tracing` 统一,壳初始化 subscriber;默认仅记错误,
    dump 需显式开启且先脱敏(§6.2);
11. **上游客户端**:连接超时 10s、流空闲 60s。首段等待每次最多 30s，
    全部尝试合计最多 60s，详见产品设计 §8。reqwest 读
    代理默认开(海外源用户依赖)。2026-11-13 已启用 reqwest 官方
    `system-proxy` feature，读取环境变量及支持平台的静态系统代理，
    未自写代理扫描或修改系统配置。环境变量 `HTTPS_PROXY` 仍可显式配置；
    本机验收见 `docs/opencode-auth-acceptance-2026-11-13.md`。
    UA 透传或对齐(§7 一致性铁律),
    不随机化;
12. **CI**(仓库建立日生效):GitHub Actions 三道门禁
    `fmt` / `clippy -D warnings` / `test`;OS 矩阵 M1 起 mac+linux,
    Windows M4 前加入(keyring 差异早暴露);
13. **性能预算**(写进设计文档 §11,可验收):透传附加 p99 ≤ 30ms、
    翻译路径附加 p99 ≤ 50ms、常驻内存 ≤ 150MB、冷启动 ≤ 2s;
14. **协作**:trunk-based + conventional commits;单人期 owner 表与
    CODEOWNERS 备而不用。

## C. 留约束位(触发时执行)

- keyring service 命名:`aubridge/<provider-id>`(credentials crate
  落地时);
- `/admin` 面 M1 只绑回环 + 独立 token;
- 时间处理只用 unix 秒(`SystemTime`),不引 chrono;
- 更新签名密钥流程 M4;
- edition 保持 2021,升 2024 单独决策;
- wasmtime/plugin-probe 触发条件见设计文档 §9。

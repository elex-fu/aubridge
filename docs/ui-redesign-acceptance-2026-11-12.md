# 桌面 UI 改版验收

日期：2026-11-12。状态：`PASS`（本次 UI 实现、隔离交互、构建、安装与原生默认窗口范围）。

设计依据见 [改版定稿](ui-redesign-2026-11-12.md)。Astra 设计及视觉复核、6.1 Sol 四块并行实现、Luna 编译/浏览器/原生验收。只调整前端呈现和本地交互，不修改 Rust、API、路由、模型源、凭据存储或默认窗口配置，不增加依赖。

## 实际交付

- 固定 48px 六页导航，暖白/青墨与线桥品牌一致，主次按钮、错误、焦点和弹层统一。
- Agents 改为三列摘要和紧凑连接行；加载、失败、未知和真实零分别显示，编辑令牌默认遮挡。
- 模型源先展示类别、凭据及异常摘要，凭据、检测、模型/路由按需展开；检测仍需明确触发。
- 代理先展示运行状态与请求列表，接入信息和筛选默认收起；当前筛选、订阅类别、尝试/跳过原因、未知用量和分页保留。
- 模型、插件、设置统一层级；清 Key、删除、恢复等动作有明确确认。共享 Dialog 支持 Escape、Tab 焦点圈、返回触发器、busy 关闭锁及弹层内可见错误。

## 命令与结果

仓库根目录为 `/Users/lex/play/aubridge`。Rust 复用 `CARGO_TARGET_DIR=/private/tmp/aubridge-cycle93.0qTct6/target`，`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`。

| 命令与工作目录 | 实际结果 |
|---|---|
| 根目录：`rtk npm --prefix crates/smoke/web run build` | 初版 RC 0；窄修后正确 cwd RC 0。最终 `index-BUhYl0FK.js`、`index-NZv2m-U3.css`，Tauri hook 亦独立构建通过 |
| 根目录：`rtk cargo fmt --all -- --check` | RC 0 |
| 根目录：`rtk cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | RC 0 |
| 根目录：`rtk cargo test --offline --locked --workspace --all-targets` | RC 0；20 suites，355 passed，0 failed |
| 根目录：`rtk proxy cargo build --offline --locked -p smoke` | RC 0 |
| `crates/smoke`：`rtk proxy cargo tauri build --debug --bundles app --ci -- --offline --locked` | 初版及最终文案窄修 RC 0，32.80 MiB |
| `rtk proxy codesign --force --sign - --timestamp=none <本次包>`，随后 `rtk proxy codesign --verify --deep --strict <本次包>` | 源包及安装副本均 RC 0；本机 ad-hoc 签名，不作发行公证结论 |

证据目录：`/var/folders/w0/qd85jr9j09130_t65s7q48700000gn/T/aubridge-ui-redesign-5h69y08a`。原始 Rust 日志为 `rust-fmt-2026-11-12.log`、`rust-clippy-2026-11-12.log`、`rust-test-2026-11-12.log`；最终前端 hook 与原生构建见 `final-tauri-build-2026-11-12.log`，安装见 `final-install-verify-manifest-2026-11-12.log`。

## 隔离浏览器与视觉结果

复用既有本机回放网关与一次性 Chrome profile，实际命令与几何/交互回执见 stage 的 `ui-checkpoint.md`，脚本为 `ui_redesign_chrome_fixture.py`。

- `800×480`、`760×480`：六页导航、main 横向边界、长模型/来源、请求行与操作可达通过。CSS viewport、visualViewport 与最终 CDP PNG 尺寸独立核验。
- 长模型目录弹层：`.modal-body` 高 335、内容高 636，可滚至 301；保存/取消可达。Agent 弹层 5 个可聚焦控件，Tab 圈、Escape、回触发器通过。
- Dashboard loading/failure、模型源读取失败、历史读取失败通过，不将缺失状态显示为健康或真实零。
- Proxy 令牌默认隐藏、路由原因、筛选与分页通过。实际截图发现 `failed` 裸英文后，仅修结果标签为成功/失败/已取消，未知保留原值；后续只跑 Proxy 定向，两种宽度均通过、分页到 27 条，不重跑无关六页矩阵和 Rust 门禁。
- Astra 对精确尺寸的 Agents、模型源和代理截图复核通过，无追加视觉改动。

预览均为隔离验收数据，不表示用户实际额度、账单或来源使用比例：[Agents](ui/2026-11-12/agents.png) · [模型源](ui/2026-11-12/providers.png) · [代理](ui/2026-11-12/requests.png)。

## 原生与安装归属

正常应用安装于 `/Users/lex/Applications/aubridge.app`，桌面入口仍指向该包。替换前通过实际 App 菜单 Quit；仅核验并备份本次先前自有包，不覆盖未知应用。原旧包与首候选分别保存在 stage 的 `old-installed-aubridge-2026-11-12.app`、`first-candidate-installed-a344-2026-11-12.app`。

首候选已实际观察 800×480 逻辑窗口、暖白/青墨六页和安全编辑弹层取消；最终仅包含 Proxy 文案窄修。最终安装 strict verify 通过，source/installed 完整 manifest 一致：可执行文件 SHA `0325210a927299a3057ada17efaaa1ea48911ea281aa1f43a32987ce46a5251c`，ICNS SHA `019f723d83b8380fc2b0acd44e793c6bdd4af76048328a588a4fb90c3abf1b8b`。

最终正式 app 已由 CUA 实际启动，bundle `dev.aubridge.smoke`、PID 14388，直接读回可执行文件哈希与最终安装 manifest 一致。实际监听为该 PID 的 `127.0.0.1:62760`；早期仅检查 3427 的负观察不代表后端失败，已保留原始记录并追加更正。最终窗口截图 Retina 1600×960 对应逻辑 800×480，暖白/青墨 Agents 页、六项导航和页头图标可见，应用保持前台。

CUA 没有 resize API，未宣称原生 760×480 实屏验收；该尺寸由独立浏览器精确 viewport/visualViewport 与交互回执证明。实际六页、弹层取消、精准退出/最终启动及端口归属均见 stage 的 `native-ui-2026-11-12.log`。未探索 Dock/系统栏，不扩张为旧托盘交互验收。

## 首次失败与范围边界

- 最早 UI 夹具 seed 早于 SQLite 创建，修复 seed 时序；后续占位数、测试请求字段、旧滚动节点、程序点击焦点和筛选顺序均为夹具修复，未以改生产迁就旧断言。
- 早期 Chrome 外窗/异步截图尺寸或页名不可靠，保留旧产物；改为唯一文件名、明确 viewport、CDP 目标页稳定后捕获，不以后裁图冒充视口。
- 最终增量首次 npm 从 stage cwd 使用相对 prefix，RC 254（找不到 package.json），原日志 `final-frontend-build-2026-11-12.log` 保留；正确根目录重试及最终 Tauri hook RC 0。
- Debug 链接器签名初次严格检查无资源 seal，保留失败；完整本机签名后验证通过。
- 原生初期沿用 3427 固定端口假设，混入隔离网关的短暂进程观察；最终按正式 app PID 全部监听读回确认动态端口归属，无须修改生产代码或重启应用。
- 浏览器、原生窗口、构建与安装分别记证。本次未做真实上游推理或实际 Agent 配置写入测试；0097 原托盘交互仍 BLOCKED、累计 4 / 10，不开始 0098。

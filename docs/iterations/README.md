# 产品迭代循环

每轮按单机、单用户桌面模型代理的定位选题。重点是稳定使用官方免费源，以用户已有订阅兜底，凭据和诊断数据保留在本机。

1. 先读产品设计和本目录编号最大的记录。遇到 `IN_PROGRESS` 或 `BLOCKED` 就从该轮继续。记录解决阻塞需要的条件。
2. 对照本轮开始时的 aubridge、magpie 和 sub2api 源码。记录参考仓库的提交号和对应代码位置。按用户遇到的问题、交付范围和可验证性排序，选一到三项。没有值得引入的功能时结束本次循环。
3. 新建 `NNNN.md`，先逐项写清当前缺口、实现方法、涉及模块、兼容要求和验收步骤，再按依赖顺序实现。
4. 把实跑命令、用例和结果写回该轮记录。协议、接线或切源变更需要走真实客户端入口，同时用回放验证可控故障。逐项验收后运行前端构建、Rust 格式检查、Clippy 和工作区测试。
5. 所有任务和回归通过后标记 `PASS`。以更新后的源码重新选题。本次循环累计通过三轮即结束，最多尝试十轮。

重新启动循环时沿用编号，按新一轮目标重新累计通过次数。

通用门禁从项目根目录运行。

```sh
rtk proxy npm --prefix crates/smoke/web run build
rtk cargo fmt --all -- --check
rtk cargo clippy --locked --workspace --all-targets -- -D warnings
rtk cargo test --locked --workspace
```

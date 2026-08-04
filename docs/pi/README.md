# Pi 工程化对照读本

> 本目录回答一个具体问题:如何借鉴 Pi,让 Zerone 已有代码资产变得更工程化、
> 更可靠、更实用。它不是 Pi 用户手册,也不是将 TypeScript 实现逐行翻译成 Rust。

## 阅读范围

本读本与 `docs/` 的前七篇一一对应。每篇都采用同一顺序:

1. 用很短的篇幅定位 Zerone 当前实现;
2. 深入解释 Pi 在同一方向上的数据结构、控制流与失败语义;
3. 提炼可以迁移到 Zerone 的设计,并指出不应照搬的部分;
4. 给出按风险排序的落地步骤和验收条件。

| 篇目 | Zerone 基线 | Pi 工程化主题 |
|---|---|---|
| [01 架构](01-architecture.md) | 单进程分层 harness | `ai / agent / harness / session` 的所有权边界 |
| [02 Agent Loop](02-agent-loop.md) | 阻塞循环 + 命令/事件通道 | 事件流、状态机、队列、Hook、并行工具 |
| [03 工具系统](03-tool-system.md) | `Tool + Workspace + Registry` | ExecutionEnv、类型化错误、进度、并发与可恢复输出 |
| [04 Provider](04-provider.md) | 三个手写 SSE 适配器 | 模型元数据、统一流协议、兼容矩阵与两层重试 |
| [05 Context](05-context.md) | ContextProvider + 线性历史 | 运行视图、事实日志、树形 Session 与压缩语义 |
| [06 添加工具](06-how-to-add-tool.md) | 实现 trait 后注册 | 从 schema 到环境注入、details、并发和测试的完整接入 |
| [07 添加 Provider](07-how-to-add-provider.md) | 新增 `Provider` 实现 | API 模块、dispatch、lazy loading、错误保真和专项协议测试 |

## 证据边界

本文依据仓库内的 Pi 教学切片,快照提交为
`a96fb984d8c8b065fc5d193309fc812a882adee0`。切片保留了本文涉及的 agent、
provider、工具环境和 session 实现,但刻意移除了 TUI、扩展系统、完整模型目录、
OAuth 等外围。具体裁剪范围见 [SNAPSHOT.md](../../example/pi/SNAPSHOT.md)。

因此,本文中的“Pi”均指**当前仓库保留的实现**。对被裁掉的上游模块,不会根据名称
猜测其行为。

## 阅读建议

先通读 01 和 02。它们解释 Pi 为什么把“事件事实”“公开状态”“模型上下文”和
“持久日志”拆开。03 到 05 是最值得迁移的工程基建;06、07 则把这些约束收束成
可执行的接入流程。

如果目标是尽快提高 Zerone 日常可用性,建议实施顺序不是篇目顺序,而是:

```text
1. 类型化 ToolOutput + 工具进度
2. Provider 统一终止事件 + 错误保真
3. context transform + token 预算
4. Session 事实日志与模型视图分离
5. steering / follow-up 队列
6. 同路径写串行化与只读工具并行
7. 树形会话、fork 和 compaction
```


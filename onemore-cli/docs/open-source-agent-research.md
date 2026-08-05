# Onemore 开源 Coding Agent 机制研究

> 研究对象：Grok Build `ed6d543643628663873c5de28298e022ed634238`、OpenAI Codex `ed2f985a26eee9a59cde0fdefd20f69b45bc25f5`。
>
> 本文是对上述本地源码快照的观察，不代表两个上游项目当前版本的行为。文中的路径均相对于工作区根目录 `E:\harness from scratch`。

## 结论摘要

Onemore 不需要复制任一参考实现的完整子系统。最值得采用的是它们共同证明有效的边界：

1. **状态先于提示词**：计划、任务和子代理生命周期必须由 reducer 与持久事实约束；提示词只负责引导模型使用这些能力。
2. **持久事实、模型投影、前端事件分离**：事实负责恢复，投影负责让模型获得必要上下文，事件负责 UI；不能把 UI 状态或完整运行时状态伪装成普通聊天文本。
3. **稳定目录、按需正文**：Skills 和 MCP 工具目录需要确定性排序并尽量在会话内冻结；大正文或动态结果只在使用后进入消息尾部。
4. **所有外部能力都服从现有权限上限**：Skill 不能扩大 turn 权限；MCP 远程工具默认审批；子代理最多继承父代理权限，不能自行升级。
5. **并发资源必须先预留再启动**：后台任务和子代理都需要有界数量、取消、超时、终态与恢复语义；共享工作区写并发默认禁止。

建议按以下顺序交付：

1. Todo / 长任务纪律；
2. Skills；
3. MCP stdio 客户端；
4. 持久后台进程；
5. 只读子代理，写隔离后续再开。

其中第 1 阶段可以独立发布。第 4 阶段应先于第 5 阶段，因为后台句柄、状态机、轮询、取消和恢复是子代理控制面的基础，而不是子代理特有能力。

## 研究边界与许可证

两个参考快照都使用 Apache License 2.0：Grok Build 见 `example/grok-build/LICENSE:1-4`，Codex 见 `example/codex/LICENSE:1-5`。本文只提取状态机、不变量、分层边界和测试思路，没有复制实现代码。

若未来直接复用代码，仍需逐项执行依赖和许可证审计，并遵守 Apache-2.0 的许可证副本、修改声明、归属和 NOTICE 传播要求；“同为 Rust”或“仓库本身是 Apache-2.0”不能替代传递依赖审计。首版设计没有要求直接引入参考仓库中的 crate。

## 当前 Onemore 的可扩展边界

Onemore 已具备适合增量演进的三层结构：

```text
Session facts (append-only SQLite)
          |
          +----> model projection / context budget ----> Provider
          |
          +----> AgentEvent ----------------------------> CLI / TUI

ToolSpec + capabilities + permission
          |
          +----> runtime scheduling / approval / cancellation
          |
          +----> atomic ToolUse + ToolResult commit
```

源码依据：

- `onemore-cli/src/session.rs:8-24` 定义追加式 `SessionEntryPayload`；`onemore-cli/src/storage.rs:5-12` 明确 append-only、事务提交与单向投影约束。
- `onemore-cli/src/session.rs:162-207` 从事实生成模型消息，压缩只改变视图，不删除旧事实。
- `onemore-cli/src/event.rs:35-102` 是 Runtime 到所有前端的事件边界。
- `onemore-cli/src/tools/mod.rs:21-72` 把执行模式、只读/破坏性能力和审批声明放在厂商无关的 `ToolSpec` 上。
- `onemore-cli/src/tools/mod.rs:96-118` 将模型正文、UI 摘要和非模型结构化 details 分开；结果正文统一受 `RESULT_MAX_CHARS = 24_000` 限制（`onemore-cli/src/tools/mod.rs:19`）。
- `onemore-cli/src/runtime.rs:540-576` 保证一个工具批次的 `ToolUse` 和全部 `ToolResult` 原子提交。
- `onemore-cli/src/provider/mod.rs:171-203` 将排序后的工具定义和 messages 纳入完整 `prompt_fingerprint`；`prompt_cache_key` 只覆盖稳定的 profile/model/system/tools 前缀（`onemore-cli/src/provider/mod.rs:206-225`）。实际工具排序见 `onemore-cli/src/provider/mod.rs:181-183`。
- `onemore-cli/src/context/mod.rs:1-12` 已明确 Session Fact、模型上下文和 UI 历史不是同一对象，并把 Planning Context 列为扩展位。

因此新特性应该扩展现有边界，不应另建一套聊天记录、权限系统或前端专用状态库。

## 参考项目架构图

### Grok Build

```text
Session actor / Agent loop
  |-- ReminderPolicy + TodoGate
  |-- Resources (serde state)
  |     |-- TodoState
  |     |-- AvailableSkills / discovery tracker
  |     `-- subagent backend/depth/runtime overrides
  |-- Tool bridge
  |     |-- todo_write / skill
  |     |-- MCP-adapted tools
  |     `-- task / get_task_output / kill_task
  |-- MCP dispatcher
  |     `-- per-server client -> initialize -> tools/list -> tools/call
  `-- ACP notifications / compaction reminders
        |-- Plan UI projection
        |-- active TODOs
        |-- running background tasks
        `-- running subagents
```

关键边界不是某个单一 crate，而是“Resources 保存可序列化状态、Session actor 持有生命周期、Tool bridge 暴露模型能力、ACP/Reminder 做投影”。例如 Todo 被注册为 Resources 状态（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs:156-166`），而 turn-end 的计划清理只发瞬时 UI 通知、不修改真实状态（`example/grok-build/crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn_end.rs:7-22`）。

### OpenAI Codex

```text
Thread / Turn
  |-- rollout history + EventMsg
  |-- core tool router
  |     |-- update_plan -> PlanUpdate event
  |     |-- native tools
  |     `-- MCP tool call -> approval -> connection manager
  |-- Skills loader
  |     `-- root snapshots -> metadata catalog -> explicit injection
  |-- AgentControl (one per root thread tree)
  |     |-- AgentRegistry / slot reservation / depth
  |     |-- child thread + independent rollout
  |     `-- detached completion watcher -> parent notification
  `-- app-server event adapter -> frontend notifications
```

Codex 的核心分离是 Thread/Turn 事实流、工具路由和 App Server 通知。`update_plan` 只发 `PlanUpdate`（`example/codex/codex-rs/core/src/tools/handlers/plan.rs:84-95`），App Server 再转为 `TurnPlanUpdated`（`example/codex/codex-rs/app-server/src/bespoke_event_handling.rs:1245-1264`）。多代理则由根线程树共享一个 `AgentControl`（`example/codex/codex-rs/core/src/agent/control.rs:90-100`），而不是让每个 child 自建无界全局注册表。

## Todo 与长任务纪律

### 行为和数据模型对比

| 维度 | Grok Build | Codex | 对 Onemore 的结论 |
|---|---|---|---|
| 状态 | `pending / in_progress / completed / cancelled`，有稳定 ID 和有序 `IndexMap`（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs:113-159`） | `pending / in_progress / completed`，每次传完整步骤数组，无 ID（`example/codex/codex-rs/protocol/src/plan_tool.rs:9-29`） | 首版用三状态、稳定 ID、完整快照；删除项目表达“不再做”，避免把取消误记为完成 |
| 更新 | 支持 replace 和 merge；先拒绝重复 ID（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs:28-95`） | 反序列化完整快照后直接发事件（`example/codex/codex-rs/core/src/tools/handlers/plan.rs:90-95`） | 完整快照更易恢复和测试；更新必须经过严格 reducer |
| 约束 | 数据结构不强制唯一 `in_progress` | schema 文案称最多一个 `in_progress`（`example/codex/codex-rs/core/src/tools/handlers/plan_spec.rs:42-54`），处理器只解析，未校验（`example/codex/codex-rs/core/src/tools/handlers/plan.rs:90-104`） | 不把不变量只写在 schema/提示词里；运行时必须拒绝非法快照 |
| 持久性 | `TodoState` 是可序列化 Resource（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs:156-166,257-260`） | `PlanUpdate` 属于非持久模型投影事件（`example/codex/codex-rs/core/src/session/turn.rs:1770-1791`） | Onemore 应新增持久 `PlanUpdated` Fact，而不是只发 UI 事件 |
| turn 结束 | UI 把残留 `in_progress` 暂时显示成 completed，但真实状态不变（`example/grok-build/crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn_end.rs:39-78`） | 长任务纪律主要来自系统指令（`example/codex/codex-rs/protocol/src/prompts/base_instructions/default.md:54-70,267-275`） | 不伪造 UI 完成；允许一次有界继续提醒，随后真实结束并保留状态 |
| 压缩 | 只重注入活动项，完成/取消折叠为计数（`example/grok-build/crates/common/xai-grok-compaction/src/reminder.rs:137-168`） | plan 事件不进入模型历史，依赖其他上下文 | 压缩后注入活动计划摘要，不重复完整历史 |
| 防死循环 | TodoGate 默认关闭，单 prompt 默认最多触发 2 次（`example/grok-build/crates/codegen/xai-grok-agent/src/system_reminder.rs:57-83`） | 没有严格 gate | Onemore 首版最多追加一次继续提醒；提醒永远不能无限阻止 turn 结束 |

### 值得采用的不变量

1. 一个快照内 ID 唯一、文本 trim 后非空、条目数量和单条文本长度有硬上限。
2. 最多一个 `in_progress`；允许全部 `pending` 或全部 `completed`。
3. revision 单调递增；工具更新基于当前 revision，过期更新返回 conflict，不静默覆盖。
4. `PlanUpdated` Fact、对应 ToolResult 与前端事件源自同一个已提交结果；UI 不得先显示未落库状态。
5. 用户取消当前 turn 时，若自动修复状态，只能把 `in_progress` 归回 `pending` 并追加真实 Fact，不能标记 completed。
6. 正常 turn 结束不改变任务真实状态。尚有活动项只触发一次有界提醒，之后允许结束。
7. 恢复 UI 从 Facts reducer 重建；模型历史不需要混入前端专用事件。
8. 压缩只携带活动项和完成计数；完整事实仍保留在数据库。

### 失败案例

- **只有提示词约束**：Codex 的工具说明声明“最多一个进行中”，但处理器没有验证，非法输入仍能成为前端事件。这说明 schema 文案不是状态机。
- **显示状态与真实状态分叉**：Grok Build 的 cosmetic cleanup 解决 spinner，却可能让用户误以为任务完成。Onemore 应宁可显示真实的 pending/in-progress，也不伪造完成。
- **merge 容忍过度**：Grok Build 在状态丢失时用 ID 回退成内容（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs:65-94`），提高可用性但会掩盖损坏。Onemore 的持久事实已经可靠，首版应拒绝不完整或过期更新。
- **无限 completion gate**：只要模型一直不更新计划，硬 gate 就可能消耗无限推理。提醒必须计数并有上限。

## Skills

### 行为和数据模型对比

| 维度 | Grok Build | Codex | 对 Onemore 的结论 |
|---|---|---|---|
| 发现/作用域 | Local → Repo → User → Server → Bundled，同名按作用域决胜（`example/grok-build/crates/codegen/xai-grok-agent/src/prompt/skills.rs:49-60`；scope 定义见 `example/grok-build/crates/codegen/xai-grok-tools/src/implementations/skills/types.rs:3-33`） | root snapshot 合并后按 scope/name/path 确定性排序（`example/codex/codex-rs/core-skills/src/root_loader.rs:132-200`） | 首版只做 Repo、User 两级，Repo 优先；路径 canonicalize 后去重并稳定排序 |
| 扫描边界 | 有深度/正文 peek 上限与大量冲突测试 | 每 root 最多 20,000 entries、64 并发加载，并记录 warnings（`example/codex/codex-rs/core-skills/src/loader/discovery.rs:17-18,54-114`） | 必须限制目录深度、条目数、技能数和文件大小；单个坏技能只产生 warning |
| 稳定提示 | 目录只公告 name/description/path；记录已公告名称，恢复后避免重复（`example/grok-build/crates/codegen/xai-grok-tools/src/types/skill_discovery_tracker/mod.rs:283-342`） | 可用技能目录是 developer fragment（`example/codex/codex-rs/ext/skills/src/fragments.rs:38-53`） | 会话启动生成一次稳定 metadata catalog，不每 turn 重扫 |
| 懒加载 | 使用 `<skill>` 边界包装正文（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/skills/skill.rs:39-63`） | 仅对明确提及的技能读正文，失败逐项 warning（`example/codex/codex-rs/core-skills/src/injection.rs:75-140`）；作为独立 user fragment（`example/codex/codex-rs/core-skills/src/skill_instructions.rs:22-40`） | `load_skill` 只读启动快照中的正文；正文作为一次工具结果进入尾部，不进稳定 system prefix |
| 动态变化 | canonical path 去重，仅变化时公告；压缩保留发现结果，clear 才全重置（`example/grok-build/crates/codegen/xai-grok-tools/src/types/skill_discovery_tracker/mod.rs:344-455,534-561`） | watcher 节流后清缓存并通知（`example/codex/codex-rs/app-server/src/skills_watcher.rs:140-174`） | 首版不做 watcher。磁盘变化下一会话生效，当前会话目录与路径绑定不变 |
| 权限 | 技能提供说明，不天然提供更高能力 | 测试明确脚本仍受 turn sandbox 控制（`example/codex/codex-rs/core/tests/suite/skill_approval.rs:200-225`） | Skill 内容不改变 `ToolSpec`、workspace 或审批策略；其后每个工具照常过权限层 |

### 值得采用的不变量和失败案例

- catalog 只包含规范化的 `name`、单行 `description`、scope 和受控 locator；正文只能由 locator 对应的已发现文件加载。
- 同一路径只出现一次；同名冲突按明确 scope 规则决胜并发 warning，不能依赖文件系统遍历顺序。
- frontmatter、UTF-8、路径或正文错误只使单项失效，不能让整个启动失败。
- `SKILL.md` 是不可信指令数据，不是权限凭证。技能要求执行脚本时，执行仍走原工具审批和沙箱。
- 目录每 turn 重扫会改变 system prefix、浪费 IO 并破坏 prompt cache；正文全部预载则会膨胀每次请求。稳定 metadata + 懒正文同时解决两个问题。

## MCP

### 共同架构边界

两者都可归纳成三层：

```text
Connection lifecycle
  initialize / cancel startup / timeout / shutdown
                |
Tool catalog snapshot
  tools/list / pagination / schema validation / deterministic naming
                |
Local tool adapter
  approval / tools/call / cancellation / result normalization + truncation
```

Codex 的连接可复用性、启动取消和 shutdown 在 `example/codex/codex-rs/codex-mcp/src/connection_manager.rs:71-123,604-626`；调用前做 filter，并应用每服务器工具超时（同文件 `649-677`）。Grok Build 用单飞初始化避免并发首调用互相失败，且对等待者设上限（`example/grok-build/crates/codegen/xai-grok-mcp/src/servers.rs:3191-3237,3245-3299`）。

### 行为和数据模型对比

| 维度 | Grok Build | Codex | 对 Onemore 的结论 |
|---|---|---|---|
| 初始化 | 单飞 handshake；取消/失败恢复状态并唤醒等待者 | 可复用 live connection；集中取消启动与 shutdown | 每服务器显式 `Starting/Ready/Failed/Stopped`；同一时间最多一个 initialize future |
| 工具目录 | 分页 `tools/list`，保留 schema；空 schema 补 `type/object`（`example/grok-build/crates/codegen/xai-grok-mcp/src/servers.rs:3873-3935`） | 并发聚合服务器后统一规范化（`example/codex/codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs:60-138`） | 先完成各服务器快照，再一次性生成本地 `ToolSpec`；单服务器失败不阻断本地工具 |
| 命名冲突 | server-qualified name，非法工具跳过 | sanitize，碰撞加 identity hash，最后按原始 identity 排序（`example/codex/codex-rs/codex-mcp/src/tools.rs:105-213`） | 首版用 `mcp__{server}__{tool}`；sanitize 后任何碰撞直接禁用冲突项并报错，避免隐式重命名 |
| 分页保护 | 快照中的循环未显示独立页数上限 | 100 页、2,048 项、cursor 大小、重复 cursor 和 30 秒总超时均受限（`example/codex/codex-rs/codex-mcp/src/pagination.rs:9-79`） | 页数、总项数、cursor 长度和总时限全部设硬上限 |
| 目录缓存 | 配置排序防止无意义重启（`example/grok-build/crates/codegen/xai-grok-shell/src/session/managed_mcp.rs:202-212`） | 32 项 LRU、30 分钟 TTL、generation 防旧刷新覆盖新结果（`example/codex/codex-rs/codex-mcp/src/tool_catalog_cache.rs:28-34,108-172`） | 首版只做会话内冻结快照；不做跨会话 LRU 和热刷新 |
| 调用/重试 | 每工具 timeout；超时不重试副作用工具，传输错误最多受控重连一次（`example/grok-build/crates/codegen/xai-grok-mcp/src/servers.rs:1610-1669,1673-1725`） | 调用前走 MCP approval（`example/codex/codex-rs/core/src/mcp_tool_call.rs:225-317`） | 所有 MCP 工具默认 `always_ask`；超时后不自动重放调用 |
| 结果 | MCP 结果转换为本地工具输出 | 进入模型前与普通函数结果同样截断（`example/codex/codex-rs/core/src/tools/context.rs:116-146`） | 复用 Onemore `ToolOutput` 清洗和 24K 字符上限；原始无限结果不能旁路 |
| 热变化 | 合并 `list_changed`/状态通知并有有限重启（`example/grok-build/crates/codegen/xai-grok-shell/src/session/mcp_dispatcher.rs:1-37`） | 支持目录捕获、缓存和 refresh | 首版忽略 `list_changed`，只提示“下一会话刷新”；不自动重启 |

### 值得采用的不变量和失败案例

1. 一台服务器一个连接 owner；所有子进程在会话 shutdown 时被回收。
2. 初始化、list 和 call 都有独立超时与共同取消 token。
3. 目录完整成功后才发布；不能让半个分页结果成为模型工具集。
4. 原始 `(server, tool)` identity 与模型可见名字分开保存；调用永远用原始 identity。
5. schema 必须是受大小限制的 JSON object；无效 schema 只禁用该工具。
6. 规范化后工具名全局唯一且顺序确定；碰撞必须显式可见。
7. 远程调用默认审批，且任何 server annotation 都不能扩大本地权限。
8. timeout 不等于“远端没有产生副作用”，因此不得自动重放 side-effecting call。
9. 结果经过现有统一截断、敏感信息清洗和 ToolUse/ToolResult 原子提交。

典型失败包括无限 pagination/repeated cursor、并发双 initialize、旧 refresh 覆盖新目录、sanitize 后名字碰撞、服务器退出遗留子进程、远端超时后重复创建资源，以及 MCP 大结果绕过本地上下文预算。

## 后台任务与子代理

### 为什么必须分开

后台进程是“一个受控 OS 任务”；子代理是“一个拥有独立模型历史、工具循环和使用量的 child session”。两者可以共享 Task ID、状态机、轮询和取消接口，但不能共享可恢复句柄语义：进程重启后通常无法重新获得原始 child handle，模型会话则可以从持久 session 恢复。

Grok Build 的每会话后台注册表默认最多 10 个任务（`example/grok-build/crates/codegen/xai-grok-shell/src/terminal/background_task.rs:83-112`）；退出时只持久化运行中任务的最小 manifest，恢复后提醒它们可能仍在运行（同文件 `275-357`）。这正是 Onemore 应先实现的控制面雏形。

### 行为和数据模型对比

| 维度 | Grok Build | Codex | 对 Onemore 的结论 |
|---|---|---|---|
| spawn 输入 | prompt、type、后台模式、能力模式、worktree、resume、cwd、model（`example/grok-build/crates/common/xai-tool-types/src/task.rs:11-110`） | child thread spawn，继承环境和 exec policy 前先预留容量（`example/codex/codex-rs/core/src/agent/control/spawn.rs:399-428`） | 首版缩小为 task/description；仅只读、fresh child，不开放 model、resume、persona |
| 所有权 | 请求携带 parent session/prompt 和 child cancellation token（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs:60-101`） | 根线程树共享一个 `AgentControl`（`example/codex/codex-rs/core/src/agent/control.rs:90-100`） | 根 session 持有唯一 control plane；child 不拥有全局 registry |
| 限制 | spawn 前检查深度和 type/model（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs:180-234,301-357`）；结果记录 token 与是否转后台（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs:372-405`） | registry 共享计数、深度判断和原子 slot reservation（`example/codex/codex-rs/core/src/agent/registry.rs:17-27,64-100`） | reserve 成功后才能创建 child；并发、深度、children 和 token budget 都有硬上限 |
| 父上下文 | 可 fresh、fork 或 resume，配置复杂 | fork 前 flush 父 rollout，可 full/last-N，再移除父专属提示（`example/codex/codex-rs/core/src/agent/control/spawn.rs:647-675,711-750`） | 首版 fresh history，只传 task 和只读 workspace context；不复制父 transcript |
| 完成回传 | 返回 final output + tool calls/turns/duration/worktree，而非强制导入 transcript（`example/grok-build/crates/common/xai-tool-types/src/task.rs:208-252`） | detached watcher 只在 final status 时通知父线程（`example/codex/codex-rs/core/src/agent/control.rs:455-546`） | 父会话只追加 final result、status、usage；child transcript 留在 child session |
| 取消/关闭 | 前台等待超时可转后台（`example/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs:462-507`） | child cancellation 从父级联，审批路由回父会话（`example/codex/codex-rs/core/src/codex_delegate.rs:157-188`）；close 级联后代并持久化 edge（`example/codex/codex-rs/core/src/agent/control/legacy.rs:32-101`） | 父 turn cancel 级联尚未完成的 child；close 幂等并释放 slot |
| 工作区 | 支持 shared 或 worktree | 继承 permission/environment；实现包含更完整的 agent graph | 首版只允许 read-only child；写任务必须等 worktree 或独占 workspace lock 完成后再开放 |

### 值得采用的不变量和失败案例

- slot reservation 与 child 创建必须是一个 RAII 生命周期；失败、取消、panic、close 都释放一次且仅一次。
- depth 按 root tree 计算，不信任模型传入值；child 总数、并发运行数和 token budget 分别限制。
- child 的权限是父权限与 child profile 的交集，绝不能取并集。
- 父取消向下级联；child 失败只作为结构化终态返回，默认不取消无关 sibling。
- completed/failed/cancelled/orphaned 都是终态，poll 和 cancel 幂等。
- 不把完整 child transcript 导入父模型历史，否则上下文与缓存成本按 child 数放大，还可能把 child 专属提示泄漏给父代理。
- 共享工作区上两个写代理会产生不可归因的冲突。没有 worktree/独占锁前，写并发必须被能力层拒绝，而不是只在提示词里劝阻。

## 不适合 Onemore 首版的机制

以下机制有真实产品价值，但当前复杂度高于 Onemore 的需要：

- Grok Build 的四态 Todo、merge 容错、动态 TodoNudge、可远程配置 TodoGate 和 cosmetic turn-end cleanup。首版完整快照 + 三态 + 一次提醒足够。
- Skills watcher、动态目录发现、条件路径激活、插件/远程/打包技能、跨 vendor 兼容目录和 slash expansion。首版只做固定 Repo/User 目录。
- MCP OAuth、HTTP/SSE transport、资源/提示协议、server-pushed `list_changed`、自动重连、跨会话 LRU 目录缓存、managed connectors 和插件 provenance。首版只做 stdio 和静态工具目录。
- Grok Build 的 persona/model/role/resume/worktree 组合，以及 Codex 的完整/last-N fork、agent graph、昵称、跨 agent 消息和多版本 residency。首版 child 使用 fresh history、单一只读 profile 和最终摘要回传。
- 直接共享可写 workspace。除非先实现 worktree 生命周期或根级独占写锁，否则它不是“简化版隔离”，而是数据竞争。

## Onemore 分阶段设计

### 阶段 1：Todo / 长任务

#### 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    pub id: String,
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub revision: u64,
    pub items: Vec<PlanItem>,
    pub explanation: Option<String>,
}

pub struct UpdatePlanArgs {
    pub expected_revision: u64,
    pub explanation: Option<String>,
    pub plan: Vec<PlanItemArg>,
}
```

建议硬上限：最多 32 项；ID 最多 64 字符；每项正文最多 512 字符；explanation 最多 2,000 字符。所有文本 trim 后校验，序列化顺序保持输入顺序。

#### Session Facts 与 reducer

在 `SessionEntryPayload` 新增：

```rust
PlanUpdated(PlanSnapshot)
```

`reduce_plan(entries)` 从零开始按顺序读取 `PlanUpdated`：首个 revision 必须为 1，后续必须恰好 `previous + 1`。新写入必须通过 reducer；读旧库遇到非法事实时保留最后一个合法快照并产生 diagnostic，不能 panic 或反向改库。

工具 reducer 生成 `PlanUpdateEffect`，Runtime 将 `assistant ToolUse + PlanUpdated Fact + ToolResult` 放入同一事务。不要借用 `ToolOutput.details` 作为隐式持久化通道；应新增明确的 harness-owned tool effect，因为 details 当前语义是“不进入模型的结构化展示信息”（`onemore-cli/src/tools/mod.rs:113-118`）。

#### Agent events

```rust
AgentEvent::PlanUpdated {
    revision: u64,
    items: Vec<PlanItem>,
    explanation: Option<String>,
}
```

事件只在 Fact 提交成功后发送。`SessionLoaded` 后前端从 entries reducer 重建计划；无需把计划伪装成 `Notice`。

#### 工具契约

```text
name: update_plan
input: expected_revision, explanation?, plan[]
capabilities: read_only=true, destructive=false, execution_mode=Sequential
permission: always_ask=false
output: { revision, pending, in_progress, completed }
errors: invalid_arguments | conflict | internal
```

`update_plan` 是完整快照替换。初始 revision 为 0，调用者必须传当前 revision；空数组合法，表示清除计划。正常 turn 结束若仍有 active item，Runtime 最多注入一次简短继续提醒；第二次模型仍结束时允许 turn 完成并保留事实。用户取消 turn 时，可原子追加一个新 revision，把唯一 `in_progress` 归回 `pending`。

#### 模型投影与压缩

常规历史中，模型已经看到自己发出的参数和工具确认，不应每一轮重复注入完整计划。压缩时在 summary 尾部追加一个有边界的活动计划段，包含当前 revision，只列 pending/in-progress 和完成计数；恢复 UI 始终读取 Facts，不读取 summary。

#### 测试矩阵

| 层 | 必测内容 |
|---|---|
| reducer 单测 | 空快照、稳定顺序、重复 ID、空文本、超限、多个 in-progress、revision gap、过期 expected_revision |
| storage | `ToolUse + PlanUpdated + ToolResult` 全成或全不成；旧 schema 迁移；非法旧 fact diagnostic |
| projection | Plan Fact 不变成聊天气泡；压缩仅保留 active + completed count；恢复前后计划一致 |
| runtime | 成功后才发 event；取消把 in-progress 归 pending；一次提醒上限；提醒后仍可结束 |
| TUI/headless | 新建/更新/清空/恢复；真实状态显示，不做 cosmetic completed |
| provider/cache | 工具定义顺序固定；只添加一次 `update_plan` schema；计划内容变化不改变稳定前缀或 `prompt_cache_key` |

### 阶段 2：Skills

#### 数据与服务

```rust
enum SkillScope { Repo, User }

struct SkillMetadata {
    name: String,
    description: String,
    scope: SkillScope,
    path: PathBuf,
    content_hash: [u8; 32],
}

struct SkillCatalog {
    ordered: Vec<SkillMetadata>,
    by_name: HashMap<String, usize>,
}
```

启动时扫描 `<repo>/.onemore/skills/**/SKILL.md` 与 `~/.onemore/skills/**/SKILL.md`。建议默认上限：深度 4、遍历 4,096 entries、最多 256 skills、单 metadata 文件读取 8 KiB、正文进入模型最多 20,000 字符。Repo 同名覆盖 User；同 scope 同名按 canonical path 最小值决胜并 warning。

目录排序固定为 `(scope_rank, name, canonical_path)`。当前会话绑定 metadata、canonical path 和 content hash；不做 watcher，磁盘变化下一会话生效。加载时重新校验路径仍位于已批准 root 且 hash 一致；不一致返回 stale catalog，避免路径替换攻击。

#### 工具与事件

```text
name: load_skill
input: { name }
capabilities: read_only=true, destructive=false, execution_mode=ParallelSafe
permission: always_ask=false
output: bounded <skill name="..." path="...">...</skill>
```

新增 `AgentEvent::SkillsDiscovered { skills, warnings }` 供 UI 展示启动快照；首版不需要 Session Fact，因为会话恢复时应从当次受信配置重新发现，并将新 catalog 视为新的 prompt identity。若产品要求恢复时严格复现旧目录，再增加只含 metadata/hash 的 `SkillCatalogCaptured`，不要持久化正文。

后续由技能引导的 `read_file`、`run_command` 等仍走现有权限层。`load_skill` 本身只允许读取 catalog 中的确切文件，不能接收任意路径。

#### 测试矩阵

| 层 | 必测内容 |
|---|---|
| discovery | scope 优先级、canonical 去重、同名冲突、稳定排序、深度/数量/大小上限、symlink 逃逸 |
| parsing | malformed frontmatter、非法 UTF-8、空 name/description、单个失败不拖垮目录 |
| loading | 仅 name 不接受 path；hash 变化；文件消失；正文边界截断；XML/标签内容转义 |
| permission | Skill 要求越界写/执行时仍被 turn policy 阻止 |
| cache | 相同目录产生相同 catalog 序列化和 `prompt_cache_key`；未选择技能不加载正文；当前会话文件变化不改变工具集 |

### 阶段 3：MCP stdio

#### 数据与生命周期

```rust
struct McpStdioConfig {
    name: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

enum McpServerStatus { Starting, Ready, Failed, Stopped }

struct McpToolIdentity {
    server: String,
    remote_name: String,
    local_name: String,
}
```

`McpConnectionManager` 独占 child process、stdin/stdout transport、initialize future 和 cancellation token。启动顺序是 spawn → initialize → 有界 `tools/list` → 完整验证 → 发布快照。shutdown 顺序是取消调用 → 协议 shutdown（有超时）→ kill/wait child。

建议默认限制：最多 16 个服务器、总计 512 个工具、32 页、512 catalog items、cursor 64 KiB、单 schema 64 KiB、目录总时限 30 秒；startup 10 秒、call 60 秒，均可在合理范围内配置。

模型可见名采用 `mcp__{sanitized_server}__{sanitized_tool}`，同时保存原始 identity。任何 normalize 后碰撞都禁用冲突项并发送 warning；不要在首版引入 hash 重命名，因为用户很难把模型名映射回配置。

#### 工具适配、Facts 与事件

每个远端工具转换成普通 `ToolSpec`：

```text
capabilities: read_only=false, destructive=true, execution_mode=Sequential
permission: always_ask=true
```

首版不信任远端 annotations 来降低审批或开启并行。调用结果转成现有 `ToolOutput`，经过 24K 字符上限，再与 ToolUse 原子落库。远端调用本身使用既有 Message Facts，不新增专用 Session Fact。

建议事件：

```rust
AgentEvent::McpServerStatusChanged { server, status, detail }
AgentEvent::McpCatalogReady { server, tool_names, warnings }
```

目录在会话内冻结。收到 `tools/list_changed` 只发 Notice，提示下一会话刷新；不修改当前 provider tool schema。服务器失败后对应工具调用返回稳定 unavailable 错误，首版不自动重连和重放。

#### 测试矩阵

| 层 | 必测内容 |
|---|---|
| protocol fake server | initialize 成功/拒绝/挂起；分页；重复 cursor；超页/超项；畸形 JSON；进程提前退出 |
| catalog | schema object 校验、空 schema 补全、大小上限、sanitize、碰撞、确定性排序、单服务器隔离失败 |
| call | 每次审批；拒绝不发远端；timeout 不重试；取消；错误映射；文本/结构化/超大结果截断 |
| lifecycle | 并发首次调用只初始化一次；shutdown 回收 child；启动中取消；多服务器互不阻塞 |
| cache | 配置和 catalog 相同时 canonical tools 与 `prompt_cache_key` 相同；服务器状态变化不改变 schema；明确新会话才接受新目录 |

### 阶段 4：持久后台进程

#### 状态与 Facts

```rust
enum TaskStatus {
    Starting,
    Running,
    Completed { exit_code: i32 },
    Failed { error: String },
    Cancelled,
    Orphaned,
}

struct BackgroundTaskSnapshot {
    id: String,
    command_summary: String,
    cwd: PathBuf,
    output_artifact: Option<ArtifactRef>,
    status: TaskStatus,
    started_at: i64,
    updated_at: i64,
}
```

新增 Facts：

```rust
TaskCreated(BackgroundTaskSnapshot)
TaskStateChanged { id, status, output_artifact, updated_at }
```

可序列化 Facts 是真相；`RuntimeTaskHandle { child, cancel, output_writer }` 只存在内存。恢复时所有最后状态为 Starting/Running 且无法可靠重新附着的任务，追加 `Orphaned` Fact；不能声称仍在控制进程。

#### 工具和事件

```text
spawn_process { command, cwd? } -> { task_id }
poll_task    { task_id, cursor? } -> status + bounded output
cancel_task  { task_id } -> terminal status
```

`spawn_process` 复用 command 审批，Sequential；`poll_task` 为 read-only/ParallelSafe；`cancel_task` Sequential，且只能操作当前 session registry 中的 ID。建议每 session 最多 8 个 active task、输出 artifact 最大 10 MiB、单次 poll 仍受 24K 模型正文限制。

事件为 `TaskStarted/TaskUpdated/TaskFinished`，均从已提交状态变化产生。压缩只投影 active task 的 ID、命令摘要、状态、耗时和 poll/cancel 工具名。

#### 测试矩阵

| 层 | 必测内容 |
|---|---|
| state | 全部合法转移；终态不可回 Running；cancel/poll 幂等；未知 ID |
| process | stdout/stderr、非零退出、超大输出、取消 race、session shutdown、容量释放 |
| restore | running → orphaned；terminal 不变；损坏 artifact/manifest 可诊断 |
| permission | 未批准不 spawn；cwd 逃逸；只取消本 session 任务 |
| cache | 三个固定工具 schema 稳定；动态 task 状态只在消息尾部/压缩 reminder，不进入 system prefix |

### 阶段 5：只读子代理

#### 控制面与类型

```rust
struct AgentControl {
    root_session_id: String,
    registry: AgentRegistry,
    limits: AgentLimits,
}

struct AgentLimits {
    max_active: usize,       // 建议默认 4
    max_children_total: usize, // 建议默认 16
    max_depth: u8,          // 首版固定 1
    max_output_tokens: u64,
}

enum ChildStatus { Starting, Running, Completed, Failed, Cancelled, Orphaned }
```

首版每个 child 创建独立 Onemore session 和模型历史，只共享只读配置快照、provider profile、workspace root、权限上限和根级 control plane。child 获得 fresh system context、用户 task 和只读工具；不 fork 父 transcript，不允许 child 再 spawn。

#### Facts、事件和工具

```rust
AgentSpawned { id, child_session_id, task, parent_turn_id }
AgentStateChanged { id, status, usage }
AgentResultRecorded { id, final_text, usage }
```

```text
spawn_agent { task, description } -> { agent_id }
wait_agent  { agent_id, timeout_ms? } -> status + final result if ready
cancel_agent { agent_id } -> terminal status
```

spawn 先原子 reserve slot，再创建 Fact 与 child；任何失败由 RAII reservation 释放。父 turn 取消向该 turn 创建且未完成的 child 级联。child 完成只把 bounded final text、status 和 usage 写回父会话，完整 transcript 保留在 child session。

事件为 `ChildAgentStarted/ChildAgentUpdated/ChildAgentFinished`。压缩只投影 running child 的 ID、description、elapsed 和 wait/cancel 工具名。

写能力暂不暴露。后续开放时只允许以下二选一：child 使用独立 git worktree；或根 control plane 取得整个 workspace 的独占写 lease。提示词声明“不要冲突”不构成隔离。

#### 测试矩阵

| 层 | 必测内容 |
|---|---|
| registry | 并发 reserve 不超限；spawn 失败释放；终态/close 释放一次；depth 与 total 上限 |
| inheritance | child 权限为交集；fresh history 不含父专属提示/私有 transcript；provider 配置只读 |
| lifecycle | parent cancel 级联；sibling failure 隔离；wait/cancel 幂等；恢复 orphaned/terminal |
| result | 只导入 final text + usage；超长截断；child transcript 不进入父 projection |
| workspace | 首版写工具不可见；未来 worktree/lease 冲突测试 |
| cache | child 固定只读工具集有独立稳定前缀和 `prompt_cache_key`；task 是尾部 user message；父只追加 final result，不导入 transcript |

## Prompt Cache 影响总表

Onemore 的完整 `prompt_fingerprint` 包含 profile、model、system、排序后的工具定义和 messages（`onemore-cli/src/provider/mod.rs:189-203`），所以正常追加历史也会改变它；稳定的 `prompt_cache_key` 只包含 profile、model、system 和工具定义（`onemore-cli/src/provider/mod.rs:206-225`）。缓存复用的关键是保持后者及实际请求前缀稳定，动态运行状态应尽量只追加到消息尾部。

| 特性 | 会改变稳定前缀/工具 schema 的内容 | 会话内策略 | 预期 cache 影响 |
|---|---|---|---|
| Todo | 新增固定 `update_plan` schema；可选增加稳定使用纪律 | 工具定义一次加入；计划快照只在 ToolUse/Result 与压缩尾部出现 | 功能首次发布形成新稳定前缀/cache key；之后计划更新不应使 system/tools 抖动 |
| Skills | metadata catalog 会进入 system/developer instructions | 启动扫描一次并冻结；正文仅 `load_skill` 后进入尾部 | catalog 相同则前缀稳定；技能文件变化只影响下一会话；避免每 turn 重扫和全正文预载 |
| MCP | 远端工具 name/description/schema 本身就是 provider tools | 完整目录发布后冻结；状态/失败不重建 schema；刷新开新会话 | 目录越大前缀越大；稳定排序允许复用；热刷新会改变稳定 cache key，首版禁用 |
| 后台进程 | 三个固定工具 schema | task 状态只以 Facts 投影到尾部/压缩 reminder | 仅发布功能时改变一次稳定 cache key；频繁 poll 仍增加普通历史 token，但不破坏之前的可缓存前缀 |
| 子代理 | 三个固定父工具；child 有固定只读工具集 | 父只接收最终摘要；child task 是其 user 尾部 | 不导入 transcript 显著降低父上下文增长；每个 child 可复用相同 system/tool 前缀 |

为了维持当前“普通长会话约 80% cache reuse”的产品目标，验收时应同时记录：

1. 相同配置连续请求的 `prompt_cache_key` 是否稳定；完整 `prompt_fingerprint` 是否只因预期的 messages 追加而变化；
2. 新工具/目录带来的固定 uncached prefix 增量；
3. 计划更新、任务状态、MCP server status 是否意外改动 system/tools 或 `prompt_cache_key`；
4. compaction 前后动态 reminder 是否只出现在消息尾部；
5. provider 实际上报的 cache read/write tokens，而不只看本地估算。

本研究不建议增加显式 cache write、模型专用 cache 控制或绕过现有稳定 `prompt_cache_key` 的逻辑。

## 跨阶段实现规则

1. **先定义 reducer，再定义工具**：所有模型输入先过纯函数校验，工具 handler 不能直接散写运行时字段。
2. **状态变化原子落库**：Fact、ToolResult 和对外 event 的顺序固定为 validate → commit → mutate runtime handle/index → emit；若 handle 必须先创建，失败时执行补偿并记录 terminal Fact。
3. **动态内容不进入稳定 system prefix**：计划、任务状态、child 结果和 server status 放在消息/事件层；只有冻结目录进入 system/tool definitions。
4. **统一限制**：每个列表、正文、schema、结果、分页、并发、深度、等待和 token 消耗都有常量与配置上限，并有边界测试。
5. **权限只收紧不放宽**：MCP annotation、Skill 文本、child role 和模型参数都不能越过当前 workspace、approval 和 execution policy。
6. **恢复时保守陈述**：无法重新附着的外部 handle 标为 `orphaned`；不能根据旧 `running` Fact 假装仍可取消。
7. **顺序确定**：catalog、工具、Facts 投影和批量结果都使用显式排序或源顺序，避免 HashMap 顺序改变稳定前缀、`prompt_cache_key` 或请求内容。
8. **可观察但不污染历史**：状态用结构化 `AgentEvent` 呈现；只有模型继续工作所需的最小摘要才进入模型上下文。

## 最终建议

先实现阶段 1，并把它作为后续所有持久运行状态的模板：`PlanUpdated` Fact、纯 reducer、提交后 `AgentEvent`、最小模型投影、有界提醒、恢复与压缩测试。这个阶段可以验证 Onemore 的三层架构是否足以承载“非聊天状态”，风险和缓存成本都最低。

Skills 紧随其后，因为它只需要只读发现和懒加载，且能直接验证“冻结目录保护 cache”的策略。MCP 再复用同一个 catalog/adapter 思路，但必须先把连接生命周期、审批和结果上限做完整。后台进程提供通用 Task 控制面后，最后才引入 child model loop；否则会同时调试模型历史、并发、进程句柄、权限和恢复，难以定位失败。

首个实现 PR 的范围应严格停在阶段 1，不顺带加入 Skills/MCP/子代理骨架。阶段 1 的完成定义是：非法计划无法落库、合法计划可恢复、UI 只显示真实状态、压缩不丢活动项、turn 不会被无限 gate，并且计划内容变化不改变 system/tools 或 `prompt_cache_key`。

**一、当前基础判断**

Onemore 的 MVP 骨架是成立的，已有这些正确边界：

- Provider 差异集中在 `src/provider/`。
- Runtime、Provider、ToolRegistry、Storage 之间没有具体工具名分支。
- `runtime.rs:275-308` 会为取消后的未执行 ToolUse 补 ToolResult，并尝试把 assistant + tool result 一起事务提交。
- `tools/mod.rs:91-122` 将未知工具和工具失败转为模型可见 Observation，而不是让 Runtime 崩溃。
- `storage.rs:214-245` 使用 SQLite transaction 写入一批消息。
- `tests/wire.rs` 已覆盖 Messages、Responses 两条请求/响应链路。

但这些保证目前依赖“单 Runtime 线程、线性会话、顺序工具、同步 listener”这一组隐含前提。一旦引入审批、后台任务、并发工具、压缩或多 Agent，现有类型不能继续承载全部语义。

**二、可靠性缺口**

| 优先级 | 缺口 | 源码证据 | 影响 |
|---|---|---|---|
| P0 | Provider 没有终止完备协议 | `provider/mod.rs:40-98` 仍是 callback + `Result<Option<TurnOutput>>` | 取消、setup 失败、流中断、正常结束需要多套恢复逻辑 |
| P0 | EOF 可能被当成成功 | `anthropic.rs`、`openai_responses.rs` 在无 terminal 事件时拒绝组装 `TurnOutput` | 首包后断流可能被误判为正常回答 |
| P0 | 目前没有最终失败 assistant message | 失败通常直接 `AgentEvent::Error`，取消是 `Ok(None)` | Provider 错误无法统一进入历史和下一个请求 |
| P0 | Runtime 没有 ActiveRun、事件归约或 idle settlement | `runtime.rs:196-318` 是单个阻塞 `run_turn` | 无法安全支持重入、steering、follow-up、异步监听者 |
| P0 | 路径不是安全边界 | `workspace.rs:40-55` 明确允许绝对路径；`workspace.rs:86` 直接 `fs::write` | `..`、symlink/junction、workspace 外写入、命令绕过都未治理 |
| P0 | 进程清理存在实际不稳定 | 本机 `cargo test` 为 54/55 通过，`tools::run_command::tests::timeout_kills` 用时约 30 秒后失败 | 子进程树或管道仍可能存活，不能把 timeout 当作已清理 |
| P1 | 工具结果只有字符串 | `tools/mod.rs:56`、`tools/mod.rs:118` | 模型正文、UI details、diff、artifact、统计信息相互污染 |
| P1 | 没有统一 schema preflight | `Tool::execute` 直接收到 `Value` | 额外字段、错误类型、边界值在执行后才暴露 |
| P1 | 输出统一中间截断 | `tools/mod.rs:36-38` | 大日志、文件、搜索结果不可恢复，可能静默丢数据 |
| P1 | 没有工具进度和迟到 update 语义 | `Tool` 无 update callback，`AgentEvent` 只有开始/结束 | 长命令无法观察，未来并发后难以判断状态 |
| P1 | Session 仍是线性消息表 | `storage.rs:325-333`、`context/conversation.rs:13-49` | 无 typed entry、分支、move、compaction、UI-only 记录 |
| P1 | 没有 schema version/migration/audit | `storage.rs:312-334` 只有 `CREATE TABLE IF NOT EXISTS` | 旧数据库升级、写入失败恢复、审计不可解释 |
| P1 | 持久化失败后 Runtime 继续运行 | `runtime.rs:322-327` 只发 Error | 用户可能继续产生副作用，但历史已不可信 |
| P2 | Retry 只有简单次数和等待 | `runtime.rs:350-376` | 没有 jitter、最大等待、Retry-After 毫秒、请求级/turn 级边界 |
| P2 | 没有跨 Provider 的统一 history normalization | 各 adapter 各自过滤 thinking/tool pair | 换模型时孤立 ToolUse、opaque thinking、非法 ID 可能重新出现 |
| P2 | Provider 没有 ModelProfile/Compat 数据 | `ProviderSettings` 只有 api、url、model、max_tokens | 方言差异会逐渐扩散为 URL/模型名分支 |
| P2 | 没有权限、审批、Hook | `event.rs:40-72` 无审批事件，Runtime 无等待状态 | 无法安全引入写入、删除、外部命令审批 |
| P3 | 无 steering/follow-up、后台任务、动态工具 | 现有 Registry 为静态 `Vec<Box<dyn Tool>>` | 08b 的 Todo、Skills、Subagent、Background、MCP 无稳定落点 |

Pi 对应的关键证据是：

- `example\pi\packages\agent\src\agent-loop.ts:155-278`：steering/follow-up 双层循环、`length` 时禁止执行全部工具。
- `agent-loop.ts:281-377`：partial assistant 只用于流式显示，final message 才进入事实消息。
- `agent.ts:161-204,482-590`：唯一 ActiveRun、AbortController、状态归约、订阅者结算后才 idle。
- `harness\types.ts:319`：`Storage owns bytes; Session owns conversation-tree semantics`。
- `harness\session\session.ts:155-275`：`parentId + leafId + appendTail` 保证并发 append 仍形成确定链。
- `harness\messages.ts`：AgentMessage、UI-only 消息与模型 Message 通过 `convertToLlm` 单向投影。

**三、分阶段实施路线**

### 阶段 0：建立失败路径测试网

- 要解决的问题：当前只有 Provider wire 测试，没有 Runtime 状态机、持久化崩溃点、权限矩阵、上下文 golden 测试。
- 模块：`runtime.rs`、`provider/`、`storage.rs`、`tools/`，新增测试夹具目录。
- 核心类型：`ScriptedProvider`、`ScriptStep`、`EventTrace`、`ToolPairValidator`、存储 fault point。
- 不变量：每个 ToolUse 恰好一个 ToolResult；重试不重复副作用；事务失败后不产生半提交历史。
- 验收：成功、Provider error、abort、工具失败、工具取消、assistant 写入失败、tool-result 写入失败均能确定性重放。
- 依赖：无。它是所有后续切片的门禁。

### 阶段 1：Provider 终止协议与 Runtime 生命周期闭合

- 要解决的问题：统一 `Start/Delta/Done/Error/Aborted`，禁止 EOF 静默成功。
- 模块：`provider/mod.rs`、两个 adapter、`runtime.rs`、`event.rs`、TUI 兼容映射。
- 核心类型：`StreamTerminal`、`FailedTurn`、`ProviderEvent::{Started,Delta,Finished,Failed}`；后续加入 `RunId/ActiveRun`。
- 不变量：每次调用只有一个终止事件；所有终止路径都有 final assistant；`length` 时所有 ToolUse 都不执行；cancel 不提交 partial。
- 验收：setup 失败、首包前断流、首包后断流、未知 terminal、正常 stop、abort 均有完整终止序列；两种 wire 测试不回归。
- 依赖：阶段 0。

### 阶段 2：类型化 Tool Pipeline

- 要解决的问题：字符串结果、无 schema preflight、无进度、无可恢复输出。
- 模块：`tools/mod.rs`、五个内置工具、`workspace.rs`、`runtime.rs`、`event.rs`。
- 核心类型：`ToolContext`、`ToolCapabilities`、`ToolOutput`、`ToolError`、`ContentBlock`、`ArtifactRef`、`ToolProgress`。
- 不变量：参数兼容转换后再校验；校验失败不执行；工具无论失败/取消都只产生一个 ToolResult；模型内容与 UI details 分离。
- 验收：非法 schema 不调用 execute；进度在 settle 后被忽略；read 采用可分页输出；bash 保留 tail 并外置完整日志；错误有稳定 code。
- 依赖：阶段 0；阶段 1 的终止语义最好先完成。

### 阶段 3：Permission 与最小 Hook 管线

- 要解决的问题：workspace 外路径、symlink/junction、命令执行目前没有真正的安全门。
- 模块：新增 `src/permission/`、新增 `src/hooks/`，修改 Runtime、Event、Workspace。
- 核心类型：`PermissionDecision::{Allow,Deny,Ask,Modify}`、`ApprovalRequestId`、`ApprovalScope`、`HookResult`、`HookContext`。
- 不变量：参数规范化和 hard deny 先于 Hook；Ask 状态暂停执行；拒绝仍补 ToolResult；Hook 不能绕过 hard deny；审批作用域明确。
- 验收：`..`、绝对路径、symlink、junction、UNC、不存在目标父目录均有测试；审批期间 Runtime 可接收响应；deny 时工具实现零调用。
- 依赖：阶段 2 的结构化 ToolSpec/ToolError；阶段 1 的生命周期状态。

### 阶段 4：事实日志、模型视图与持久化版本化

- 要解决的问题：当前 `ChatMessage` 同时承担运行、模型、UI、存储四种语义，且上下文始终全量发送。
- 模块：`message.rs`、`context/`、`storage.rs`、`runtime.rs`、Session UI。
- 核心类型：`SessionEntry`、`SessionEntryPayload`、`ModelContext`、`ContextTransform`、`ContextBudget`、`PromptHistory`、`CompactionCheckpoint`。
- 不变量：事实日志 append-only；模型上下文是单向投影；UI-only entry 不进 Provider；ToolUse/ToolResult 不被裁成半批；entry 与 leaf/统计在同一事务提交。
- 验收：旧线性消息可迁移；迁移失败保留原库；压缩后事实条数不减少；`/session` 恢复完整事实；token 估算有真实 usage 基线和尾部估算。
- 依赖：阶段 2；阶段 0 的 crash-point 测试。

### 阶段 5：Recovery、ActiveRun、Steering/Follow-up

- 要解决的问题：当前输入只能阻塞等待；重试没有统一状态；没有 `waitForIdle` 语义。
- 模块：`runtime.rs`、`event.rs`、`provider/`。
- 核心类型：`ActiveRun`、`CancellationToken`、`QueueMode`、`RecoveryState`、`RetryPolicy`。
- 不变量：同一 Agent 最多一个 ActiveRun；steering 只在完整工具批后注入；follow-up 只在原任务将停止时注入；idle 等待所有事件消费者完成；重试不重复已经提交的副作用。
- 验收：重入被拒绝；one-at-a-time 队列顺序稳定；取消清理队列和子进程；Retry-After、jitter、最大等待和 abort 可测试。
- 依赖：阶段 1、3、4。

### 阶段 6：受控并发与资源锁

- 要解决的问题：当前工具严格串行；未来简单改成并发会造成同路径写冲突和历史顺序不稳定。
- 模块：`runtime.rs`、`tools/`、`workspace.rs`。
- 核心类型：`ToolExecutionMode::{Sequential,ParallelSafe}`、`FileMutationQueue`、`BatchExecution`。
- 不变量：preflight 按源顺序；UI end 按完成顺序；历史 ToolResult 按 ToolUse 源顺序；任一 sequential 工具使整批串行；同 canonical path 的 read-modify-write 仍串行。
- 验收：快工具先完成但历史顺序不变；取消后每个调用仍有结果；同文件 write/edit 不交错；并发上限和单工具 timeout 生效。
- 依赖：阶段 2、3、5。

### 阶段 7：Todo、Skills、Task System

- 要解决的问题：让计划、规则和跨会话工作单元拥有明确状态，而不是塞进 prompt 字符串或全局变量。
- 模块：新增 `planning/`、`skills/`、`tasks/`，扩展 `tools/`、`context/`、`storage/`。
- 核心类型：`TodoStore`、`TodoItem`、`SkillRecord`、`TaskRecord`、`TaskStatus`、乐观版本号。
- 不变量：session/workspace 隔离；Todo 原子替换且最多一个 InProgress；Skill 只按已验证 name 加载；Task claim 使用事务 CAS，不能重复领取；不可信 Skill/Memory 不能改变权限。
- 验收：两个会话状态不串；malformed 更新不覆盖旧状态；项目级 Skill 覆盖规则明确；Task DAG cycle、missing dependency、崩溃 lease 均有测试。
- 依赖：阶段 3、4、6。

### 阶段 8：Background、同步只读 Subagent、MCP

建议严格拆成三个独立切片：

1. **Background command**：`BackgroundTaskManager`、`BackgroundStatus`、`BackgroundStarted/Finished`、artifact tail；原 ToolUse 只配对一次，完成后只能是独立通知。
2. **同步只读 Subagent**：`AgentFactory`、`AgentSpec`、depth/max-turn/token budget、窄工具集；先禁止可写并发和递归 spawn。
3. **MCP**：动态 Registry、generation、命名空间、连接生命周期、pending request 失败、schema 缓存失效；未知外部工具默认 Ask。

依赖阶段 2、3、4、5、6。不要在前面阶段未稳定时实现这三项。

**四、风险/收益排序**

| 顺序 | 工作 | 风险 | 收益 |
|---|---|---:|---:|
| 1 | 阶段 0 测试网 | 低 | 极高，降低所有后续改造风险 |
| 2 | 阶段 1 Provider 终止与生命周期 | 中 | 极高，修复最核心协议缺口 |
| 3 | 阶段 2 ToolOutput/ToolError/校验 | 中 | 极高，为权限、并发、artifact 铺路 |
| 4 | 阶段 3 Permission/Hook | 中高 | 极高，直接降低副作用风险 |
| 5 | 阶段 4 Context/Session 分离 | 高 | 极高，支持长任务、压缩、恢复 |
| 6 | 阶段 5 Recovery/队列 | 中高 | 高，改善长运行交互 |
| 7 | 阶段 6 并发/资源锁 | 高 | 中高，提升吞吐但扩大状态空间 |
| 8 | 阶段 7 Todo/Skills/Task | 中 | 高，提升复杂任务体验 |
| 9 | 阶段 8 Background/Subagent/MCP | 很高 | 高，但必须建立在前述契约之上 |

**五、推荐第一个纵向切片**

推荐从“**Provider 终止协议 + Runtime 运行闭合**”开始，范围保持窄：

1. 先加入 `ScriptedProvider` 和终止路径测试，不改存储模型。
2. 为 Provider 增加统一终止结果，两个 adapter 都禁止 EOF 静默成功。
3. 让 abort/error 都生成可取得的 final assistant。
4. Runtime 保留现有 TUI 事件，通过兼容映射逐步增加 started/finished 语义。
5. 明确 `length` 不执行任何工具调用。
6. 保持现有 SQLite 线性格式、同步架构和 wire 测试不变。

选择理由：它直接修复当前最高风险的错误语义，影响范围集中在 `provider/`、`runtime.rs`、`event.rs` 和测试，不需要先引入异步运行时或重写 Session；后续 ToolOutput、Permission、Recovery、Context Compact 都可以建立在这个终止协议上。

当前本机验证结果：`cargo test --locked` 通过 115 个单测和 5 个 wire 测试，`cargo clippy --locked --all-targets -- -D warnings` 与 release 构建也通过。

**六、值得迁移与不应照搬的 Pi 机制**

值得迁移：

- 四类所有权边界：AI Message、Agent/Runtime Message、AgentEvent、Session Entry。
- 终止完备的 Provider stream 和 final assistant。
- ActiveRun、状态先归约再通知、listener 结算后的 idle。
- `ToolOutput + ToolError + details + artifact + progress`。
- 参数兼容转换、schema 校验、before/after hook。
- `length` 时禁止执行工具；并发完成顺序与历史顺序分离。
- `transformContext -> convertToLlm` 单向投影。
- Session 的 append-only entry、parent/leaf、append 串行队列。
- ModelProfile、Compat、跨 Provider history normalization。
- Background 独立生命周期、动态工具 generation、资源级 mutation queue。

不应照搬：

- TypeScript declaration merging；Rust 应采用稳定 enum 或受控 `Custom { kind, payload }`。
- `AsyncDisposable`、Promise tail、AbortController 的字面翻译；Rust 用 owner、channel、RAII、取消令牌表达同一所有权。
- Pi `ExecutionEnv` 的绝对路径能力；它不是 sandbox，Onemore 仍需 canonical path、symlink 和审批策略。
- 无条件 `Promise.all`；只能迁移 preflight、顺序历史提交和资源锁语义。
- 立即把 SQLite 换成 JSONL；当前应保留 SQLite，只迁移版本、事务、append 和 shutdown 语义。
- Pi 教学切片中未完整实现的自动 compaction、自动 memory、OAuth/catalog 等机制。
- 在 Runtime、Provider、TUI 中按具体工具名增加分支。


# 01 · 从 Zerone 到 Pi:工程化架构

> 对应 Zerone 基线:[01 · 架构总览](../01-architecture.md)。
> Pi 重点源码:`packages/ai/src/types.ts`、`packages/agent/src/types.ts`、
> `packages/agent/src/agent.ts`、`packages/agent/src/harness/`。

## 先给结论

Zerone 的架构目标是**让一条数据流容易读懂**;Pi 的架构目标是在保留这条数据流的
同时,让不同宿主、Provider、工具环境和存储后端可以独立演化。

两者的核心 Loop 并没有数量级差异。真正的差距在 Loop 周围:

- Zerone 用一套 `ChatMessage` 服务 Runtime、Provider、存储和界面;
- Pi 区分模型可理解的 `Message`、应用可扩展的 `AgentMessage`、运行事件
  `AgentEvent`、公开快照 `AgentState` 与持久事实 `SessionTreeEntry`;
- Zerone 主要靠模块边界维持职责;Pi 进一步用窄接口定义资源所有权、异步结算和
  生命周期。

Pi 的工程化不是“类型更多”本身,而是让每种状态只有一个权威拥有者。

## Zerone 当前是什么

Zerone 是一个刻意压低复杂度的单进程 Rust harness:

```text
TUI / --once
    ⇅ AgentCommand / AgentEvent
Runtime
    ├─ ContextProvider[]
    ├─ Provider
    ├─ ToolRegistry ── Workspace
    └─ SessionManager ── SQLite
```

它已经有三条正确边界:

1. Provider 差异停在 `src/provider/`;
2. 工具通过 `ToolRegistry` 和 `Workspace` 执行;
3. Runtime 与 TUI 只通过命令/事件交互。

当前局限也很明确:Runtime 同时负责会话推进、上下文装配、工具调度、重试和持久化;
`ChatMessage` 同时扮演运行消息、模型消息和存储记录;工具只返回字符串;Session 是
线性消息表。这些选择非常适合教学 MVP,但功能增加后会让每次扩展都碰到 Runtime。

## Pi 的四层不是四个目录,而是四种所有权

### 1. AI 层拥有“模型协议”

[packages/ai/src/types.ts](../../example/pi/packages/ai/src/types.ts) 定义:

- `Model`:API、provider、输入模态、上下文窗口、最大输出、费用和兼容参数;
- `Context`:system prompt、模型消息、工具声明;
- `Message`:`user | assistant | toolResult`;
- `AssistantMessageEvent`:流式开始、内容块增量和唯一终止事件;
- `ProviderStreams`:每种 API 模块共同实现的 `stream/streamSimple` 契约。

这一层不知道 Agent 是否有队列、Session 是否落盘、工具如何访问文件。它只回答:
“给定模型与上下文,怎样得到一条合法的 assistant 事件流?”

### 2. Agent Loop 拥有“本次运行的因果顺序”

[agent-loop.ts](../../example/pi/packages/agent/src/agent-loop.ts) 是低层状态机。它拥有:

- 什么时候开始/结束一个 agent run 和一个 turn;
- partial assistant 如何被 final assistant 替换;
- 工具何时校验、执行、回填;
- steering 和 follow-up 在哪个检查点进入;
- 什么情况下继续下一次模型调用。

它不拥有长期 transcript,也不直接写存储。输入是 `AgentContext` 快照,输出是有序事件
和本次新增消息。

### 3. Agent 对象拥有“进程内公开状态”

[agent.ts](../../example/pi/packages/agent/src/agent.ts) 是有状态外壳。它拥有:

- 当前 `AgentState`;
- 唯一活动运行 `ActiveRun` 与 `AbortController`;
- steering/follow-up 队列;
- 事件订阅者;
- `prompt / continue / abort / waitForIdle / reset` 这些宿主 API。

`Agent` 通过消费 Loop 事件归约自己的状态,而不是让 Loop 随意修改字段。这使 TUI、
服务器或测试都可以订阅同一事实流。

### 4. Harness 拥有“世界与事实日志”

`packages/agent/src/harness/` 又分成两个方向:

- `ExecutionEnv` 拥有文件系统和进程能力,工具只依赖它;
- `SessionStorage` 拥有字节,`Session` 拥有会话树语义,
  `SessionRepository` 拥有 create/open/list/delete/fork 生命周期。

这里最重要的一句注释是:

```text
Storage owns bytes; Session owns conversation-tree semantics.
```

JSONL 和内存后端只负责可靠保存/读取 entry;哪个 entry 是当前叶子、怎样回溯分支、
compaction 如何进入模型上下文,属于 `Session` 聚合对象,不能散落到存储驱动里。

## 五种数据形态为什么不能合并

| 形态 | 权威拥有者 | 用途 | 是否都进模型 |
|---|---|---|---|
| `Message` | AI 层 | Provider 可编码的消息 | 是 |
| `AgentMessage` | 应用/Agent | 允许宿主扩展的运行消息 | 否,先 `convertToLlm` |
| `AgentEvent` | Loop | 一次运行中已经发生的事实 | 否 |
| `AgentState` | Agent | 供 UI/宿主读取的当前快照 | 否 |
| `SessionTreeEntry` | Session | 可恢复、可分支的事实日志 | 由 projector/transform 决定 |

Zerone 的 `ChatMessage` 很适合统一三家 API,这一点应保留。需要升级的不是删除统一
消息,而是在它外面补出两层:

```text
事实日志(SessionEntry)
       │ build model view
       ▼
应用消息(AgentMessage) ── convert ──► Provider 消息(ChatMessage/Message)
```

否则 UI 通知、后台任务完成、会话标签、压缩记录只能被迫伪装成 User 消息,或者完全
无法持久化。

## 一次请求在 Pi 中怎样穿过各层

```text
1  宿主调用 Agent.prompt(input)
2  Agent 建立 ActiveRun + AbortController,把状态设为 isStreaming
3  Agent 复制 system/messages/tools,形成 AgentContext 快照
4  runAgentLoop 发 agent_start / turn_start / prompt message 事件
5  transformContext 在 AgentMessage 域修剪或注入上下文
6  convertToLlm 过滤 UI-only 消息,转成 AI Message[]
7  createProvider 按 model.api 选 API 模块
8  API 模块产生 AssistantMessageEventStream
9  Loop 将 provider 增量转成 message_update 事件
10 final assistant 替换 partial;若有工具调用则进入工具管线
11 工具通过 ExecutionEnv 访问文件/进程,结果成为 ToolResultMessage
12 Loop 在 turn_end 后检查 next-turn hook、停止条件与两个队列
13 agent_end 是最后一个 Loop 事件
14 Agent 等完所有订阅者,清理 ActiveRun 后才真正变为 idle
15 Session 由上层事件消费者追加事实 entry,不由低层 Loop 偷偷落盘
```

第 14 步是成熟度差异的典型例子。Pi 不把“最后一个事件已发出”误当成“运行已结算”;
异步监听者可能还在写 Session、刷新 UI 或记录审计日志。`waitForIdle()` 必须等这些
监听者完成。

## Pi 用接口隔离变化,但没有假装所有东西都能替换

| 变化 | 稳定边界 | 具体做法 |
|---|---|---|
| 换模型 API | `ProviderStreams` | 按 `model.api` dispatch |
| 换宿主 | `AgentEvent` + `AgentState` | 订阅事件,调用公开方法 |
| 换文件/进程环境 | `ExecutionEnv` | Node 实现只是一个 adapter |
| 换 Session 后端 | `SessionStorage/Repository` | 内存与 JSONL 共用语义测试 |
| 加应用消息 | `CustomAgentMessages` | declaration merging + `convertToLlm` |
| 改上下文策略 | transform/projector | 不改事实日志 |

同时,Pi 没有把一切都做成插件。Loop 的 turn 语义、事件顺序和 Session entry 类型是
核心协议。工程化的关键不是“任意替换”,而是明确哪些东西必须稳定。

## 系统级不变量

读 Pi 源码时,下面六条比目录结构更重要:

1. **一个 Agent 同时最多一个 ActiveRun**。并发输入进入显式队列,不能重入
   `prompt()`。
2. **事件先归约内部状态,再通知外部监听者**。监听者看到的 `AgentState` 已与事件
   一致。
3. **终止也是协议事件**。Provider 失败返回 `error` 事件和最终
   `AssistantMessage`,不靠 reject 让上层猜测半成品。
4. **模型视图不是事实日志**。`transformContext`、`convertToLlm` 和 Session projector
   都只构造视图。
5. **同一资源的写操作有单一顺序**。Agent run、Session append、同路径文件修改都
   有各自的串行化队列。
6. **清理是生命周期的一部分**。Agent、ExecutionEnv、Repository 都有明确的
   idle/cleanup/dispose 语义。

## Zerone 应该怎样迁移

不要先把目录拆成多个 crate。先把所有权拆清楚,收益更大、风险更小。

### 第一阶段:在现有单体中补协议

1. 将 provider 增量升级为有 `Start/Delta/Done/Error` 的终止完备流;
2. 将 `AgentEvent` 作为 Runtime 的事实输出,补一个独立 `AgentStateReducer`;
3. 将工具返回从 `String` 升级为 `ToolOutput { model_text, details, ... }`;
4. 明确 `TurnFinished` 与“所有事件消费者已处理完成”的差异。

### 第二阶段:拆模型视图与事实日志

1. 保留 `ChatMessage` 作为 Provider 统一模型;
2. 新增可扩展的 `RuntimeMessage`/`SessionEntry`;
3. 引入 `build_model_context(entries)` 单向投影;
4. 让 SQLite 保存 entry,而不是只保存最终模型消息。

### 第三阶段:拆资源所有权

1. 将 `Workspace` 扩成文件能力接口,命令执行成为独立能力;
2. 将 `SessionManager` 拆成 Repository、Storage、Session 语义;
3. 给 Runtime 增加显式 shutdown/idle settlement;
4. 最后才评估是否值得拆 crate。

## 不要照搬的部分

- `CustomAgentMessages` 的 declaration merging 是 TypeScript 技巧。Rust 更适合稳定枚举
  加受控 `Custom { kind, payload }`,或由应用层包一层泛型。
- `AsyncDisposable`、Promise tail 和 `AbortController` 不必逐字翻译。Rust 可以用
  owner task、channel、RAII guard 和 cancellation token 表达同一所有权。
- Pi 的 `ExecutionEnv` 允许绝对路径,它是能力抽象,不是 workspace 沙箱。Zerone 若要
  权限隔离,仍须单独设计 canonical path、symlink 和审批策略。
- 多层抽象都有维护成本。Zerone 只有一个存储后端时,先稳定接口和测试,不必立即做
  动态 dispatch。

## 架构验收

改造后至少能回答这些问题,而且答案都只有一个:

- 谁拥有当前 transcript?
- 谁决定一条记录是否进入模型?
- 谁保证同一会话的 append 顺序?
- Provider 失败时,最终消息从哪里取得?
- `agent_end` 后谁还可能在工作?
- 工具能访问哪些环境能力,由谁提供?
- Session 分支、压缩和存储格式分别属于哪一层?

若同一个问题需要回答“Runtime 和 TUI 都管一点”,边界仍未完成。


# 02 · Pi 的 Agent Loop:从循环到可结算状态机

> 对应 Zerone 基线:[02 · Agent Loop](../02-agent-loop.md)。
> Pi 重点源码:[agent-loop.ts](../../example/pi/packages/agent/src/agent-loop.ts)、
> [agent.ts](../../example/pi/packages/agent/src/agent.ts)、
> [types.ts](../../example/pi/packages/agent/src/types.ts)。

## Zerone 当前实现

Zerone 的 `Agent::run_turn` 是单层、阻塞、顺序执行的 Loop:

```text
用户消息 → build prompt → stream model → assistant message
        → 逐个执行 ToolUse → User{ToolResult...} → 下一次模型调用
```

它已经认真处理了三个关键问题:

- provider 流中止时不提交半截 assistant;
- assistant 一旦提交,每个 ToolUse 都必须配 ToolResult;
- 只有尚未产生任何增量的失败才自动重试。

它缺少的是运行时产品能力:进行中输入、等待当前任务结束的 follow-up、可插拔工具
审批、异步事件订阅、动态切模型/上下文、工具进度以及有约束的并行。

## Pi 为什么分低层 Loop 和有状态 Agent

Pi 没有把所有行为塞进一个类:

- `runAgentLoop` 接收上下文快照和回调,只实现因果顺序;
- `Agent` 持有 transcript、队列、公开状态、订阅者和当前取消控制器;
- `agentLoop` 再把 callback 版本包装成可异步迭代的 `EventStream`。

这个拆分让同一个 Loop 有两种消费方式:

```text
直接 await runAgentLoop(..., emit)   # 宿主掌握事件结算
for await (event of agentLoop(...))  # 流式消费
```

更重要的是,低层 Loop 可以用纯快照测试;有状态 Agent 单独测试重入、队列、订阅者和
idle 语义。

## 事件协议比“文本 delta”更完整

Pi 的 `AgentEvent` 有四个层级:

```text
agent_start
  turn_start
    message_start
    message_update*                # 仅 assistant 流式阶段
    message_end
    tool_execution_start*
    tool_execution_update*
    tool_execution_end*
    message_start/message_end*     # ToolResultMessage 也是消息
  turn_end
agent_end
```

`agent_end` 是一次 run 的最后一个 Loop 事件,但不是 `Agent` 已 idle。`Agent.processEvents`
会先归约内部状态,再**按订阅顺序 await 每个 listener**;直到最后一个 `agent_end`
listener 结束,`finishRun()` 才清掉 `ActiveRun`。

这解决了 Zerone 事件通道尚未表达的问题:Session writer、审计和 UI 刷新若是异步的,
“事件已发送”不代表“副作用已提交”。

## 双层循环:工具续跑与任务续排是两回事

Pi 的 `runLoop` 有内外两层:

```text
pending = drain steering
while true {                                      # 外层:follow-up
    hasMoreToolCalls = true
    while hasMoreToolCalls || pending not empty { # 内层:当前工作
        inject pending
        assistant = call model
        if error/aborted: end run
        execute tool calls
        emit turn_end
        prepare next turn / should stop
        pending = drain steering
    }
    pending = drain follow-up
    if pending empty: break
}
```

两类队列语义不同:

| 队列 | 注入点 | 用途 |
|---|---|---|
| steering | 每个 assistant + 工具批次完成后 | 改变正在进行的任务方向 |
| follow-up | Agent 原本将停止时 | 等当前任务完成后再做下一件事 |

两者都支持 `all` 与 `one-at-a-time`;默认一次取最老的一条。`prompt()` 在活动运行中
直接报错,调用方必须明确选择 `steer()` 或 `followUp()`。这比把输入悄悄塞进一个
无语义队列可靠得多。

### 为什么 steering 不抢断当前工具

Pi 等当前 assistant 请求的所有工具完成后才注入 steering。这样不会制造“模型要求
写文件,用户中途改口,文件到底写没写”的隐式状态。真正紧急的停止走 `abort()`;
方向调整走 turn 边界。两者不可混为一谈。

## 流式 assistant:partial 是临时对象,final 才是事实

`streamAssistantResponse` 的处理顺序很精确:

1. Provider 发 `start`,Loop 把 partial 放到当前 context 尾部并发
   `message_start`;
2. 每个 delta 都用新 partial 替换尾部,发 `message_update`;
3. `done/error` 到达后调用 `response.result()`;
4. final message 替换 partial,发 `message_end`;
5. `Agent.processEvents(message_end)` 才把 final message追加到公开 transcript。

因此 UI 可以展示半截内容,但 `AgentState.messages` 只收到已结束消息。Provider 即使在
异步 setup 阶段失败,也必须生成 error final message,不能让 stream 无结果结束。

## 工具调用是一条五段管线

Pi 没有把 `tool.execute()` 直接写在 for 循环里:

```text
lookup
  → prepareArguments 兼容旧参数
  → validate schema
  → beforeToolCall(可阻止)
  → execute(可发 partial update)
  → afterToolCall(可修订结果)
  → tool_execution_end
  → ToolResultMessage
```

失败统一转为 `AgentToolResult` + `isError=true`,包括:

- 工具不存在;
- 参数校验失败;
- before hook 阻止;
- execute 抛错;
- after hook 抛错;
- 已收到 abort。

Hook 的位置很重要:`beforeToolCall` 在 schema 校验后,所以权限策略拿到的是已验证参数;
`afterToolCall` 在事件和历史提交前,所以可以脱敏、外置大结果或标记终止。

## `length` 不是普通停止:截断参数绝不能执行

如果 assistant 的 `stopReason` 是 `length`,Pi 会拒绝执行该消息里的**全部工具调用**。
原因是流式 JSON 修复器可能把截断参数修成“语法合法但语义不完整”的对象。例如:

```json
{"path":"src/important.rs","content":"前半段...
```

若修复后通过 schema,直接执行可能覆盖文件。Pi 为每个调用生成错误 ToolResult,要求
模型重新发完整参数。这一策略应优先迁移到 Zerone;仅靠“JSON 能解析”不够。

## 并行不是无条件 `Promise.all`

Pi 默认 `toolExecution="parallel"`,但有严格顺序:

1. 按 assistant 源顺序逐个发 `tool_execution_start`;
2. 按源顺序 lookup、参数准备、schema 校验和 before hook;
3. 只有通过 preflight 的调用才并发 execute;
4. `tool_execution_end` 按实际完成顺序发,让 UI 及时更新;
5. `ToolResultMessage` 等全批完成后按**原 ToolUse 顺序**写入。

任何一个工具声明 `executionMode="sequential"`,当前整批都会退回顺序执行。这个策略
比 Zerone 08b 设想的读写 batch 更保守:Pi 尚未根据资源冲突把同一批细分成多个
并行段,但它至少保证不把声明为串行的工具混入并发。

并行产生两个不同的顺序:

```text
UI/观测:完成顺序     fast B → slow A
模型历史:请求顺序     result A → result B
```

这是工程上正确的分离。把完成顺序直接写进历史会让相同输入因机器调度产生不同 prompt。

## 提前终止为什么要求“全批一致”

工具结果可带 `terminate=true`,但只有本批**每个 finalized result**都为 true,
`shouldTerminateToolBatch` 才停止自动续跑。

这避免一个工具说“我已给出最终结果”就吞掉同批其他工具的有效结果。若批中只有部分
要求终止,Loop 仍把全部结果交给模型决定下一步。

## turn 后的两个 Hook

`turn_end` 后,Pi 依次提供:

- `prepareNextTurn`:替换下一请求的 context/model/thinking level;
- `shouldStopAfterTurn`:当前 turn 完整结束后优雅停止。

它们解决不同问题。前者适合 context compact、动态模型切换或工具集变化;后者适合
预算、策略或产品层停止。都不能跳过当前工具结果的提交。

## 取消与异常:Pi 和 Zerone 的不同选择

### Pi 的路径

- 同一 run 共用一个 `AbortSignal`;
- Provider 以 final `AssistantMessage{stopReason:"aborted"}` 结束;
- 工具和 hook 自己尊重 signal;
- 顺序工具在一个调用结束后发现 aborted 会停止准备后续调用;
- Agent 外壳若捕获到低层抛错,补发完整
  `message_start → message_end → turn_end → agent_end` 失败序列。

Pi 的 provider 消息转换层还会为孤立 ToolCall 合成错误 ToolResult,并过滤
`error/aborted` assistant,从而在下次请求前修复模型历史。

### Zerone 的路径

Zerone 在 Runtime 工具循环中立刻为尚未执行的调用补结果。这让**内存历史本身**始终
合法,而不是等 Provider 转换时修复。

对 Zerone 来说,后者更容易推理,建议保留。可以借鉴 Pi 的终止事件和 AbortToken,
但不必把历史修复延迟到 adapter 边界。无论选哪条路,必须只有一个权威修复点,并有
跨三种 API 的测试。

## Hook 的可靠性边界

Pi 对 `convertToLlm`、`transformContext`、队列 getter、`shouldStopAfterTurn` 等回调
写了“不得 throw/reject”的契约。低层不会给每个 callback 都包恢复逻辑;违反契约时
由 Agent 外壳转成一次失败运行。

迁移到 Zerone 时应区分:

- **业务拒绝**:如权限 deny,是普通 ToolResult;
- **可恢复回调失败**:如可选审计 sink 失败,按策略降级或停止;
- **编程错误**:状态不变量被破坏,终止 run 并保留完整生命周期事件。

不要把三者都压成 `String`。

## 推荐迁移顺序

### 第一步:终止完备的事件协议

补齐 `AgentStarted/AgentFinished`、`MessageStarted/Updated/Finished` 和工具 update。
规定每次 run 无论成功、取消还是异常都只有一个终止事件。

### 第二步:有状态外壳与重入保护

保留现有 Runtime 线程,但显式引入:

```rust
struct ActiveRun {
    id: RunId,
    cancel: CancellationToken,
}
```

新 `UserInput` 在 active 时必须被分类成 steering、follow-up 或拒绝,不能靠 mpsc
排队时机隐式决定。

### 第三步:工具前后 Hook

先做 `BeforeToolCall` 权限与 `AfterToolCall` 输出处理,都使用已验证的参数和类型化结果。

### 第四步:队列

先实现 `one-at-a-time`,检查点只放在完整 turn 后。为 abort、clear、session switch
规定队列清理语义。

### 第五步:受控并行

先给工具加 `Sequential/ParallelSafe`,保证:

- preflight 顺序稳定;
- UI 看完成顺序;
- 历史按源顺序;
- abort 后每个 ToolUse 仍有结果;
- 同路径写由工具层再次串行化。

## 验收测试

Pi 的测试给出了很好的最低集合,对应文件是
[agent-loop.test.ts](../../example/pi/packages/agent/test/agent-loop.test.ts) 与
[agent.test.ts](../../example/pi/packages/agent/test/agent.test.ts):

- 完整生命周期在成功、throw 和 abort 时都闭合;
- async subscriber 未结束时 `prompt()` 和 `waitForIdle()` 不提前返回;
- settled 工具后到达的 update 被忽略;
- 并行工具 end 按完成顺序,结果消息按源顺序;
- 任一 sequential 工具使整批顺序执行;
- `length` 截断的工具调用一个都不执行;
- steering 在整批工具之后注入;
- follow-up 只在原任务将停止时注入;
- `one-at-a-time` 不会一次排空整个队列;
- `prepareNextTurn` 的新快照确实用于下一次请求;
- 只有全批 terminate 才提前停止。

Agent Loop 是否“只有一百行”并不决定可靠性。真正的标准是:每个中断点都有确定的
历史、事件、队列和资源状态。


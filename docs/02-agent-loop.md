# 02 · Agent Loop

对应源码:`src/runtime.rs`(核心)、`src/event.rs`(契约)。

## 循环本体

`Agent::run_turn` 是全项目最重要的函数,骨架如下(与源码一一对应):

```text
push 用户消息进历史;emit UserMessage / TurnStarted
for round in 0..max_turns {
    prompt = build_prompt()            # ① 组装上下文
    output = call_model(prompt)        # ② 流式调模型(含重试/取消)
    历史.push(output.message)          # ③ 提交 assistant 消息
    calls = output.message.tool_uses()
    if calls.is_empty() { break }      # ④ 没有工具调用 → 本轮结束
    for call in calls {                # ⑤ 逐个执行
        emit ToolCallStarted
        outcome = registry.execute(call)
        emit ToolCallFinished
        results.push(ToolResult)
    }
    历史.push(User 消息{results})       # ⑥ Observation 写回
}                                      # ⑦ 回到 ①
emit TurnFinished
```

几个容易被低估的细节:

- **⑥ 的工具结果是一条 User 角色消息**。这是统一模型(Anthropic 风格)的
  表达;Chat Completions 适配器会把它拆成 `role:"tool"` 消息,Responses
  适配器拆成 `function_call_output` item(见 04)。Loop 层不关心。
- **③ 先提交消息、再执行工具**。顺序不能反:若工具执行到一半用户取消,
  历史里已有 ToolUse,必须能补上 ToolResult(见下文取消一节)。
- **④ 的判断依据是"消息里有没有 ToolUse 块"而不是 stop_reason**。
  部分兼容服务的 finish_reason 不可靠,内容才是事实。

## 停止条件

一轮(turn)在以下情况结束:

| 条件 | 表现 |
|---|---|
| 模型没有再调用工具 | 正常结束,`TurnFinished{cancelled:false}` |
| 用户 Esc 取消 | `TurnFinished{cancelled:true}`(两个取消点见下) |
| 模型调用失败且不可重试 | `Error` + `TurnFinished` |
| 连续调用达到 `max_turns` | `Notice` 提示 + 结束(用户可输入"继续") |
| 输出撞 `max_tokens` 且无工具调用 | `Notice` 提示可能截断 + 正常结束 |

## 取消语义(历史合法性的关键)

取消标志是 `Arc<AtomicBool>`,TUI 按 Esc 时置位,Runtime 在两类检查点响应:

**A. 流式接收中途**(`provider/*.rs` 每个 SSE 事件之间):
适配器返回 `Ok(None)`,`run_turn` **直接丢弃半截输出**并收尾。
此时历史停在用户消息上——完全合法,下一轮把同样的历史再发就行。
(TUI 画面上已流出的半截文字保留展示,但它不在历史里;
这与 Claude Code 处理中断的方式一致:画面 ≠ 历史。)

**B. 工具执行序列中途**(`run_turn` 的工具 for 循环顶部):
已执行的工具照常记录结果;**尚未执行的调用补一条
`"[用户取消,本工具未执行]"` 的错误结果**,然后收尾。
为什么不能直接扔掉?因为 assistant 消息里的 ToolUse 已进历史,
三种 API 都要求每个工具调用必须有配对结果,缺一个下轮请求就 400。

`run_command` 内部还有第三层响应:子进程轮询循环发现取消标志后
`taskkill /T /F` 杀掉整棵进程树(见 03)。

每条新命令开始前,`spawn` 的工作线程会复位取消标志——
上一轮的取消不会波及下一轮。

## 重试策略

`call_model` 里,重试只发生在同时满足三个条件时:

1. 错误被标为可重试(HTTP 408/429/5xx、网络传输错误——`provider/mod.rs`);
2. **本次调用尚未产生任何流事件**(重试幂等:吐了半截再重来,
   用户会看到重复文本,历史也可能脏);
3. 尝试次数 < 3。

退避 2s/4s,若响应带 `Retry-After` 则听它的;退避睡眠切成 100ms 片,
期间可被取消。中途断流(已开播)不重试,直接报 `Error`——
这是刻意的保守,续传/断点恢复留作扩展。

## 事件与命令(event.rs)

方向 | 类型 | 说明
---|---|---
前端→Runtime | `UserInput` | 开启一轮
 | `ClearConversation` / `SwitchProvider` / `SetModel` | 会话与 provider 管理(命令在通道里排队,turn 进行中会等它结束)
 | `ListSessions` / `LoadSession` | 按当前 workspace 列出/恢复 SQLite 会话
 | `Shutdown` | Runtime 线程退出
Runtime→前端 | `UserMessage` | 回显(前端不自己上屏,统一走事件,保证 headless 一致)
 | `TurnStarted` / `TurnFinished{cancelled}` | 轮次边界
 | `AssistantDelta` / `ThinkingDelta` / `AssistantMessage` | 流式文本(Message 是全文,前端用来兜底校正)
 | `ToolCallPending` / `ToolCallStarted` / `ToolCallFinished` | 工具生命周期(Pending 仅状态栏,参数还在流式拼接)
 | `Usage` | 会话累计 token
 | `SessionsListed` / `SessionLoaded` | 会话列表/用于重建前端的完整历史
 | `Notice` / `Error` / `ConversationCleared` / `ProviderChanged` | 其他

`main.rs::run_once` 用五十行就把同一事件流变成了命令行输出——
这就是"未来接 GUI/Web 共用 Runtime"的证明:再写一个消费者而已。

## 与 Claude Code 的差距(即你的扩展方向)

这个 Loop 已经能做完整的 coding agent 工作,它与成熟产品的差距全在
"循环之外":

| 能力 | 挂载点 |
|---|---|
| 运行中追加输入(steering)| 命令通道已就绪:定义新命令,在工具间隙检查并插入历史 |
| 上下文压缩 | `Conversation::contribute`(见 05) |
| 权限/审批 | `ToolRegistry::execute` 之前(见 03) |
| 子代理 | Runtime 本身就是"命令进事件出"的独立单元,递归 spawn 即可 |
| 存储迁移与修复 | 已有一会话一 SQLite 库；下一步给 `storage.rs` 加 schema version、迁移和损坏诊断 |
| 队列化多轮任务 | 前端把输入排队,`TurnFinished` 后发下一条 |

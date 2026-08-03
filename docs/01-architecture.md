# 01 · 架构总览

> 这套文档的目标:读完之后,你可以不依赖任何人,独立扩展这个系统的任意一层。
> 建议先通读本篇建立地图,再按 02→05 深入各层,06/07 是动手教程。

## 设计原则

整个项目只遵守四条规则,所有代码组织都是它们的推论:

1. **可读性 > 扩展性 > 性能**。全部阻塞 IO、两个线程、零 unsafe、无宏魔法;
   任何一段数据流都能用手指沿着函数调用追到头。
2. **内部只有一套消息模型**(`message.rs`)。三种 API 的差异被压死在
   `provider/` 的边界上,越过这条边界,系统里不存在"OpenAI 消息"或
   "Anthropic 消息",只有 `ChatMessage`。
3. **Runtime 从不接触界面**(`event.rs`)。它只收命令、发事件;
   TUI 和 `--once` headless 是同一事件流的两个消费者。
4. **每层留缝**。工具、上下文、提供商都是 trait + 注册点,
   加东西 = 实现 trait + 注册一行,不改既有层。

## 分层图

```
                     ┌───────────────────────────────┐
   用户按键/命令行     │  前端(可替换,已有两个)         │
                     │  · tui/     交互式界面          │
                     │  · main.rs  --once 打印器       │
                     └────────┬──────────────▲────────┘
                   AgentCommand│              │AgentEvent      ← event.rs 是唯一契约
                     ┌────────▼──────────────┴────────┐
                     │  runtime.rs  Agent Loop         │
                     │  组装上下文→调模型→执行工具→回填→循环│
                     └────────────────────────────────┘
                        ├─ context/    模型看什么(instructions / 环境 / 历史)
                        ├─ tools/      模型能做什么(Tool trait / 五个工具)
                        │    └─ workspace.rs  一切文件访问
                        ├─ provider/   模型是谁(Provider trait / 三个适配器)
                        └─ storage.rs  SQLite 会话历史(~/.zerone/sessions/*.db)
```

三个竖井分别回答 agent 的三个基本问题,这也是往后一切扩展的归类依据:
想改"模型知道什么"→ context;想改"模型能干什么"→ tools;想换"脑子"→ provider。

## 一次对话的完整时序

以"用户输入`读一下 main.rs`"为例,标注了每一步所在的文件:

```
1  TUI 捕获 Enter                          tui/mod.rs  submit()
2  ──AgentCommand::UserInput──►  Runtime 线程
3  emit UserMessage(回显)                  runtime.rs  run_turn()
4  组装 PromptContext:
     instructions + 环境信息 + 全部历史      context/*  contribute()
5  Provider 编码请求体,POST,读 SSE 流       provider/anthropic.rs 等
6  每个 text 增量 ──AssistantDelta──► TUI 实时渲染
7  流结束:得到 ChatMessage{文本+ToolUse}    provider 返回 TurnOutput
8  ToolUse 非空 → emit ToolCallStarted
9  ToolRegistry.execute("read_file",…)      tools/mod.rs → read_file.rs
     └ 经 Workspace 读文件、加行号            workspace.rs
10 emit ToolCallFinished(截断后的预览)
11 结果作为 Block::ToolResult 写回历史       runtime.rs
12 ToolUse + ToolResult 在同一事务落库          storage.rs
13 回到第 4 步(历史里多了一轮工具往返)
14 模型这次直接回答 → 无 ToolUse → emit TurnFinished
```

第 4~12 步就是需求里的 `User → LLM → Tool Call → Execute → Observation → LLM`
循环;`max_turns`(默认 50)是它唯一的保险丝。

## 模块地图

| 路径 | 职责 | 关键类型 | 详见 |
|---|---|---|---|
| `src/message.rs` | 统一消息模型 | `ChatMessage` `Block` `Usage` `StopReason` | 04 |
| `src/workspace.rs` | root、路径解析、文件原语 | `Workspace` | 03 |
| `src/tools/mod.rs` | 工具契约与分发 | `Tool` `ToolRegistry` `ToolSpec` | 03 |
| `src/tools/*.rs` | 五个内置工具 | — | 03 |
| `src/context/mod.rs` | 上下文契约 | `ContextProvider` `PromptContext` | 05 |
| `src/context/*.rs` | instructions / 环境 / 历史 | `Conversation` 等 | 05 |
| `src/provider/mod.rs` | 提供商契约、HTTP、工厂 | `Provider` `ProviderEvent` `ProviderError` | 04 |
| `src/provider/sse.rs` | SSE 解析器(三家共用) | `SseReader` | 04 |
| `src/provider/{anthropic,openai_chat,openai_responses}.rs` | 三个适配器 | — | 04 |
| `src/event.rs` | Runtime↔前端契约 | `AgentCommand` `AgentEvent` | 02 |
| `src/runtime.rs` | Agent Loop、线程装配 | `Agent` `RuntimeHandle` `spawn` | 02 |
| `src/storage.rs` | `~/.zerone` 路径、SQLite 会话存储 | `AppPaths` `SessionManager` | 02 |
| `src/tui/` | 交互前端 | `App` `Transcript` `InputBox` | — |
| `src/config.rs` | config.toml 解析、key 查找 | `Config` `ProviderSettings` | — |
| `src/util.rs` | 清洗/截断/摘要 | — | 03 |
| `tests/wire.rs` | 三接口 mock 集成测试 | `MockServer` | 07 |

## 线程模型与取消

只有两个长命线程:

- **主线程**:TUI 事件循环(33ms 一拍:收 AgentEvent → 收按键 → 重绘);
- **`agent-runtime` 线程**:阻塞地跑 Agent Loop(网络、工具都在这)。

两条 `mpsc` 通道 + 一个 `Arc<AtomicBool>` 取消标志把它们连起来
(`runtime.rs::spawn`)。取消为什么不走命令通道?因为 Runtime 忙着读流时
不会回头看通道,而原子标志可以在每个 SSE 事件之间、每个工具执行之间被
检查到(`provider/*.rs` 流循环顶部、`runtime.rs` 工具循环顶部、
`run_command.rs` 的子进程轮询里)。

工具内部还有短命线程:`run_command` 为 stdout/stderr 各开一个读线程
(防管道死锁),这属于工具的实现细节,不进入全局模型。

## 系统级不变量

改代码前先记住这三条,破坏它们的扩展一定会在某个 API 上炸:

1. **历史合法性**:每个 `ToolUse` 必须有配对的 `ToolResult`。
   取消/出错路径也要维持(02 讲了具体如何做到)。
2. **重试幂等**:只有"尚未产生任何流事件"的调用失败才能自动重试。
3. **一切进模型/进画面的文本都过 `util::sanitize` + 截断**,
   否则子进程的 ANSI 码会画花 TUI,超长输出会撑爆上下文。
4. **有关联的持久化必须原子提交**:带 `ToolUse` 的 assistant 消息与配对的
   `ToolResult` 在同一个 SQLite 事务中写入,崩溃后不能恢复出非法历史。

## 代码量

核心约 4000 行(含注释与单测),分布大致:provider 三分之一、
tui 四分之一、tools 五分之一,其余是 runtime/context/config/基础设施。
这个比例本身就说明:**harness 工程的重头是提供商适配与交互,
循环本体其实很小**——`run_turn` 不到 120 行。

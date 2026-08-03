## 从零开始的harness构建
本文档是一个摘要。此仓库是 我（WEP-56） 基于rust制作的一个极简但完整的基底 agent，用于在实践中完善它的harness工程，最终制作出一个可以持续运行，完整且可靠的coding agent。它本身不会拥有mcp、skill等功能，只有必要的工具及其提示词。这个项目不是一个教学，而是我个人的学习记录。

如果你想看很牛的教程——如何做出agent，建议先看这个教程：[learn claude](https://learn.shareai.run/zh/timeline/) ；它很底层，且面向基础设施，然后再看那些开源项目 codex、opencode、pi、grok build 的源代码，同时，结合你自己的实践。

## 目录跳转
- 关于基底agent,你可以直接查看基底agent的代码：[src](src)； 它模仿了最最最早期的[Pi](https://github.com/earendil-works/pi) ，极致简约，暴露三类必要的工具，其余依靠LLM自身的能力。

代码本身拥有非常详细的注释，结构也一目了然，所以不需要文档解释，但在通读前你可能需要先了解rust（如果有LLM帮助的话，不了解也行）

1. 它向LLM暴露的工具（agent的肌肉）：[tools](src/tools) :
- edit_file:精确字符串替换,agent 修改代码的主力工具。
- list_dir:列目录。支持 depth 递归,自动跳过体积巨大的常见目录
- read_file:带行号读取文本文件,支持 offset/limit 分段读大文件。
- run_command:在 Windows 上执行命令(唯一"能跑任意东西"的工具)。
- write_file:整文件写入(新建或覆盖),自动创建父目录。
- [mod.rs](src/tools/mod.rs)  这是工具的注册表，关联于system hint

2. 它的基础system hint，向LLM定义它是谁、能用什么tools、它在哪（harness工程最需要扩展的地方）：[context](src/context)，在这部分最应该注意的是：[workspace_info.rs](src/context/workspace_info.rs)

3. 提供商适配，这其实是最重最难做的部分（agent的大脑）。我让它支持
- OpenAI Responses API
- OpenAI Chat Completions API
- Anthropic Messages API

三种协议，且都是流式。你可以直接查看代码：[provider](src/provider)

4. 一些必要的基础设置
- 配置文件及其存取：[config.rs](src/config.rs)
- loop与runtime(agent的心脏): [event.rs](src/event.rs) 、 [runtime.rs](src/runtime.rs)
- 

5. 最后，是一个简单的tui，斜杠命令、消息滚动与渲染。我不喜欢在tui里加入鼠标交互，所以键盘就够了。[tui](src/tui)

如果你想用其他语言构建一个这样的底座型agent loop，同时你有很多token。可以查看并调整这个提示词：[需求提示词](我的需求.md)

大概会会花费一到两小时

6. 如果你需要根据此agent加以扩展，请用力的看这七个文档：[docs](docs)

## 对于harness工程，你与我都可能需要这些

### 源码

[Pi](https://github.com/earendil-works/pi)

[Codex](https://github.com/openai/codex)

[Opencode](https://github.com/anomalyco/opencode)

[Grok build](https://github.com/xai-org/grok-build)

### 文档、教程
[rust_agent](https://zhenbodou.github.io/rust_agents/)

[learn claude](https://learn.shareai.run/zh/timeline/)

[buidl your own openclaw](https://build-your-own-openclaw.kiyo-n-zane.com/)

## 最后，生成这样的agent，我用到的提示词
```
完整复制或修改成你所需的技术栈
```

我想学习 Agent Harness Engineering，而不是直接获得一个完整产品。
请基于 Rust 为我实现一个：
"极小但结构正确" 的 Agent Harness。

目标：这个项目应该是 Claude Code / Codex / Pi 的最小可运行雏形（MVP）。

重点：

- 容易阅读
- 容易扩展
- 所有核心概念明确分层
- 代码宁可少也不要隐藏逻辑

不要为了功能而增加复杂度。


#### 功能边界

1. Agent Loop
支持：
User
 -> LLM
 -> Tool Call
 -> Tool Execute
 -> Observation
 -> LLM
循环直到结束。
2. Tool System，实现可扩展 trait：Tool、ToolRegistry、默认只提供：
- 读工具：
  - read_file
  - list_dir
- 写工具：
  - write_file
  - edit_file（基于字符串替换即可）

- 执行工具：
  - run_command

Windows 优先。

3. TUI使用：ratatui + crossterm，提供：
- 聊天区
- 输入框
- 状态栏

支持：
- Streaming 输出
- Tool 调用显示
- 错误显示
不要实现复杂布局。

4. Model Layer 抽象为：Provider Trait。要支持：
- OpenAI Responses API
- OpenAI Chat Completions API
- Anthropic Messages API

要求：新增 Provider 时不需要修改 Agent Loop。

5. Context System不要做复杂 Memory。只实现：ContextProvider trait

默认提供：
- Conversation Context

并预留：

- Workspace Context
- Planning Context

的接口位置。

6. Workspace实现：Workspace struct。它负责：
- root path
- path resolve
- file access

所有 Tool 必须通过 Workspace。

不要直接访问文件系统。


7. Event System：Agent Loop 不直接操作 TUI。
要求：
Agent Runtime -> 发 Event -> TUI -> 监听 Event

例如：

- UserMessage
- AssistantMessage
- ToolCallStarted
- ToolCallFinished
- Error

这样未来方便：GUI、Web、TUI共用 Runtime。


8. 请把项目当教学项目设计。制作完毕后，你必须编写如下文档：

01-architecture.md、
02-agent-loop.md、
03-tool-system.md、
04-provider.md、
05-context.md、
06-how-to-add-tool.md、
07-how-to-add-provider.md、

让普通rust工程师在阅读文档后可以独立扩展整个系统。


#### 不要做的东西
- MCP
- Memory
- RAG
- Planner
- Sub-Agent
- Sandbox
- Guardrails
- Approval System
- Git Tool
- Hooks
- Skills
仅预留扩展点。


#### 代码要求

优先保证：
可读性 > 扩展性 > 性能
生成完整目录结构。
先输出：
1. 架构图
2. 目录树
3. 每个模块职责
然后开始实现。

项目总代码量尽量控制：
3000~5000 行以内。

目标：
让我未来可以逐步实现：
- Planning
- Memory
- Workspace Map
- Guardrails
- Reflection
- Compression
- SubAgents

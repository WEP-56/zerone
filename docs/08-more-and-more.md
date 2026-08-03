# 08 · 更多扩展

> 拿到了这个 agent 的代码，我该怎么把它提升至生产级?
> 这篇文档专注于抛砖引玉。编号仅用于文档美观度，不代表优先级。

## 1. 工具扩充

### 1.1 `todo_write`:把计划写下来

给 Agent 一个复杂任务:"把所有 Python 文件改成 snake_case 命名,然后跑测试,修好失败。"
Agent 开始干活,改了 3 个文件,跑了个测试,发现 2 个失败,开始修。修着修着,
它忘了最初是"改成 snake_case"——测试失败把注意力全吸走了。对话越长越严重:
工具结果不断填满上下文,系统提示的影响力被稀释。一个 10 步重构,
做完 1-3 步就开始即兴发挥,因为 4-10 步已经被挤出注意力了。

所以,开始之前,让 LLM 自己制定一个工作清单。Agent 收到任务后的典型流程:

1. 先调用 `todo_write` 列出所有步骤(全 `pending`)
2. 做一个步骤,改成 `in_progress`
3. 做完改成 `completed`
4. 看下一个 `pending`,继续

连续 3 轮没有调用 `todo_write` 时,循环会在下一次 LLM 调用前追加一条 reminder。

`todo_write` 不给 Agent 增加任何执行能力,它增加的是**规划能力**。
这已经是 harness 的一个必要组件之一了。

### 1.2 `spawn_subagent`:开一个新终端

就算有了 TODO 的提醒,长上下文下还是会影响 Agent 的注意力。对你来说:
你修 bug 的时候,会"开一个新终端"来追踪调用链;追踪完了,终端关掉,
结果写进笔记,回到原来的终端继续修 bug。Agent 也需要这个能力:
开一个独立的子进程,给它一个独立的消息列表,让它专心做一件事。

子 Agent 的任务由主 Agent 分配。如果你想的话(我推荐),建议让它的工具受限:
只读 / 不写 todo……

### 1.3 `load_skill`:按需加载知识与规范

你有自己的代码管理流程、开发规范,但它们很长、很复杂。你可能会想到把它塞进
system prompt,但这样一来,Agent 每次调用 LLM 都带着这些文档——99% 的内容和当前任务
无关,白白消耗 token。那么,不如把这些约束放进一个无论是我还是 LLM 随时呼出来的东西,
按需调用?

为此,你需要给 Agent 提供一个 `/skill` 目录并注册。skill 的调用,推荐用两层设计:

| 层 | 注入方式 | 时机 | 成本 | 作用 |
|---|---|---|---|---|
| 1. 目录 | system prompt | 启动时注入(harness 扫描 `skills/`) | ~100 tokens/skill,每轮都带 | 给 LLM 提供 skill 的名字、描述 |
| 2. 内容 | tool_result | Agent 调用 `load_skill` 时;`SKILL.md` 可指引后续的 `read_file`/`bash` 调用,用于按需访问额外资源 | ~2000 tokens/skill,按需提供 | 给 LLM 提供 skill 的实际内容:无论是用户要求还是 agent 主动 |

skills 目录最好长这样:

```
skills/
  agent-builder/SKILL.md
  code-review/SKILL.md
  mcp-builder/SKILL.md
  pdf/SKILL.md
```

有了 skill 的加持,一些轻量的 workflow 已经可以实现——但不够硬。

## 2. 硬性扩充

> 一些关键的基建型代码补充。

### 2.1 System Prompt

在这个仓库中,系统提示词只告诉了 LLM:它是谁、它在哪,顺便告诉它:
你拥有什么工具,该怎么做。很简陋是吧?LLM 完全可能很难知道自己输出的 token
要往哪块猜,所以 system prompt 真的很重要。

但系统提示词不能硬塞,它要智能地拼接:

| 片段 | 时机 | 内容 |
|---|---|---|
| `identity` | 始终给 | 你是谁、怎么做事 |
| `tools` | 始终给 | 可用工具列表(`enabled_tools`) |
| `workspace` | 始终给 | 工作目录 |

### 2.2 错误处理

Agent 失败了、API 调用失败了,loop 要怎么做、Agent 自己要怎么做?
生产环境中 API 错误是常态。三种最常见的故障模式:

1. **输出被截断**:模型话说一半,token 用完了
2. **上下文超限**:压缩后还是太长
3. **临时故障**:429 限流 / 529 过载

一个不处理错误的 Agent,就像一个一碰就熄火的车。

#### 上下文压缩

在引入上下文压缩前,你需要先定义一些东西:

1. `max_tokens`:所用模型的最大上下文、最大输出
2. 压缩阈值:到达这个阈值会触发压缩;当然,用户手动压缩也可以

对于上下文压缩,这是一个深刻的学问,或许查看源码最能学习到东西。
这是 Claude Code 源码定义的 compact 逻辑(`query.ts`):

| 函数 | 行号 | 作用 |
|---|---|---|
| `applyToolResultBudget` | L379 | 先处理大结果,确保完整内容落盘 |
| `snipCompact` | L403 | 裁中间消息 |
| `microcompact` | L414 | 旧结果占位 |
| `contextCollapse` | L441 | 独立的上下文管理系统 |
| `autoCompact` | L454 | LLM 全量摘要 |

#### 调用错误处理

遇到错误码 429、529、请求体错误……处理起来很简单:**指数退避 + 抖动**
(reconnect 10/10……你应该看过这个东西)。

推荐的参数是:

```
ESCALATED_MAX_TOKENS = 64000
MAX_RETRIES          = 10
BASE_DELAY_MS        = 500
FALLBACK_MODEL       = ...
```

你得加入这些函数:`with_retry`、`retry_delay`、`reactive_compact`、
`is_prompt_too_long_error`、`RecoveryState`。

整个 loop 会变成:`try/except` 包裹 + `continue` 重试。

### 2.3 记忆管理

上下文压缩会把当前目标、剩余工作、用户约束写进摘要,但细节会丢失:
"用 tab 缩进不要用空格"可能被简化成"用户有代码风格偏好"。
而且新开一个会话,连摘要也没了。LLM 没有持久状态,所有信息都在上下文窗口里;
上下文满了要压缩,压缩就有损。需要一层**不参与压缩、跨会话保留**的存储。

所以,给 LLM 一个 `/memory`,里面存放一些 markdown 文件,类似:

```markdown
---
name: user-preference-tabs
description: User prefers tabs for indentation
type: user
---

User prefers using tabs, not spaces, for indentation.
**Why:** Consistency with existing codebase conventions.
**How to apply:** Always use tabs when writing or editing files.
```

再给这些 md 一个索引:`MEMORY.md`:

```markdown
- [user-preference-tabs](user-preference-tabs.md) — User prefers tabs for indentation
```

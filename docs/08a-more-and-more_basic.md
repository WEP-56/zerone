# 08a · 从 MVP 到可靠 Agent:底层篇

> 前七篇解释了 Zerone **现在为什么能工作**。这一篇讨论另一件事:
> 如果目标是接近 Codex / Claude Code 这类可以长期使用的 coding agent,
> 哪些底层契约必须继续演进,以及应该按什么顺序演进。

本文的组织方式参考了
[Learn Claude Code](https://learn.shareai.run/en/s01/) 的递进思路:
每一层只解决一个问题,新能力挂在稳定的 Agent Loop 外面。参考课程用 Python
演示机制;本文不复述它的代码,而是把这些机制映射到 Zerone 当前的 Rust 架构。

这不是一张“把功能名打勾”的清单。可靠性来自**不变量、失败路径和可验证边界**,
不是来自工具数量。

---

## 先定位:什么算“底层修改”

如果一个能力只需实现 `Tool` 并注册,它属于 [08b tools 篇](08b-more-and-more_tools.md)。
如果它会改变下面任意一项,它就属于底层:

- 一轮对话如何推进、暂停、重试或结束;
- 工具执行前后必须经过哪些检查;
- 模型每轮看到哪些消息,哪些内容可以丢;
- Runtime 与 TUI 之间新增什么命令或事件;
- 哪些状态必须跨进程保存,崩溃后如何恢复;
- 多个任务、线程或 Agent 如何共享资源而不互相踩踏。

Zerone 已经有一个不错的起点:

| 已有基础 | 当前落点 | 还缺什么 |
|---|---|---|
| 稳定 Agent Loop | `runtime.rs::run_turn` | 权限、Hooks、恢复状态机 |
| 厂商无关消息 | `message.rs` | 压缩视图、附件引用、schema 演进 |
| ContextProvider | `context/` | 动态 prompt、计划、记忆、压缩 |
| 工具注册表 | `tools/mod.rs` | 元数据、审批、并发策略 |
| 命令/事件边界 | `event.rs` | 审批、后台通知、恢复事件 |
| SQLite 会话 | `storage.rs` | schema version、迁移、修复、审计 |
| 取消与有限重试 | `runtime.rs` / provider | 错误分类、抖动、熔断、降级 |

## 四条不能破坏的不变量

后面每增加一层,先检查这四条。它们比任何具体实现都重要。

1. **消息合法性**:每个已提交的 `ToolUse` 都必须有且仅有一个配对的
   `ToolResult`;取消、拒绝、超时也必须补结果。
2. **副作用至多一次**:网络重试不能重复执行已经成功的写文件、命令或外部操作。
3. **持久化先定义事务边界**:内存状态、SQLite 和界面事件不能各自成功一半。
4. **安全策略优先于扩展逻辑**:Hook、skill、MCP 注解和模型判断都不能绕过
   硬性 deny 规则。

可以把目标架构先画成下面这样:

```text
用户输入
  -> PromptAssembler
  -> ContextBudget / Compact
  -> Provider + RecoveryPolicy
  -> ToolUse
  -> PermissionPolicy
  -> PreToolUse Hooks
  -> ToolRegistry
  -> PostToolUse Hooks
  -> 原子写入 ToolUse + ToolResult
  -> 回到 Provider

旁路状态:
  Session DB | Long-term Memory | Background Tasks | Audit Log
```

---

## 1. 先建立“改坏就会响”的测试网

直接往 `run_turn` 塞功能,最后一定会得到一段没人敢改的代码。第一步不是加功能,
而是把现有不变量变成测试。

建议先增加四组测试夹具:

| 测试 | 必须证明什么 |
|---|---|
| Runtime scripted provider | 不联网即可控制模型依次返回文本、工具调用、错误 |
| Crash-point storage test | 在 user / assistant / tool result 写入点模拟失败,恢复后历史仍合法 |
| Permission matrix | 同一调用在 allow / ask / deny 下不会走错分支 |
| Context golden test | 同一历史在压缩前后,保留目标、约束和 ToolUse 配对 |

当前 `tests/wire.rs` 很适合验证 provider 编解码,但它不是 Runtime 状态机测试。
可以新增一个只在内存里工作的 `ScriptedProvider`:

```rust
// 结构示意,不是当前已有 API
enum ScriptStep {
    Output(TurnOutput),
    Error(ProviderError),
    Cancel,
}

struct ScriptedProvider {
    steps: VecDeque<ScriptStep>,
}
```

以后每加一条恢复路径,都先写一段脚本证明“消息有没有提交、工具有没有重复执行、
最终发了什么事件”。没有这层测试,后面的重构只能靠真实 API 碰运气。

---

## 2. System Prompt:从字符串升级为运行时装配

### 问题

当前 `Instructions` 只有一个内置字符串,`config.toml` 的 `system_prompt` 会整体替换它。
这对 MVP 很清楚,但能力增多后会出现两个极端:

- 把权限、计划、skill、记忆、workspace map 全塞进一个字符串,无法独立测试;
- 每轮无条件注入所有内容,无关说明消耗 token,还会稀释当前任务。

### 目标结构

系统提示应由**稳定片段**和**动态片段**组成:

| 片段 | 是否稳定 | 加载条件 |
|---|---|---|
| identity / behavior | 稳定 | 始终 |
| tool policy | 稳定 | 始终,但只描述真实注册的能力 |
| workspace | 动态 | 路径、shell、仓库状态变化时 |
| planning | 动态 | 有 TODO / task 时 |
| skill catalog | 动态 | 扫描到 skill 时 |
| memory index | 动态 | 当前 workspace 有记忆时 |
| permission mode | 动态 | 模式变化时 |

Zerone 已有 `ContextProvider`,不需要另造一套 prompt 框架。更自然的升级是:

```text
extra_context:
  Instructions
  ToolPolicyContext
  WorkspaceInfo
  PlanningContext
  SkillCatalogContext
  MemoryIndexContext
conversation:永远最后
```

### 实现步骤

1. 保留 `PromptContext { system_sections, messages }`,不要把厂商格式带进来。
2. 把 `system_prompt` 的语义从“整体替换”改成明确模式:
   `replace`、`prepend` 或 `append`;默认值必须清楚。
3. 为动态片段定义小而稳定的输入状态,不要让它读取整个 `Agent`。
4. 用确定性 key 缓存装配结果;key 来自真实状态,不要在消息文本里猜关键词。
5. 工具列表变化、skill 目录变化、permission mode 变化时显式使缓存失效。

这里有两个常见误区:

- **工具 JSON Schema 已由 Provider 单独发送**,system prompt 不应再复制一遍完整 schema;
  它只需要解释全局策略和工具之间的选择原则。
- “缓存字符串拼接”不等于厂商的 prompt cache。后者要求稳定前缀字节完全一致,
  动态内容最好放在稳定边界之后。

### 验收

- 未启用 memory 时,prompt 中没有 memory 空壳说明;
- 注册/注销工具后,下一轮工具目录和 provider specs 同步变化;
- 同一状态连续组装得到逐字节相同结果;
- 三个 provider 看到的 system 语义一致。

参考:[s10 System Prompt](https://learn.shareai.run/en/s10/)。

---

## 3. Permission:在副作用前建立真正的门

### 为什么 `Workspace::resolve()` 不等于安全

当前 `Workspace` 会归一化路径,但明确允许绝对路径和 workspace 逃逸;
`run_command` 更能执行任意命令。模型“通常不会乱来”不是安全边界。

权限至少要区分四种结果:

```rust
enum PermissionDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: String, choices: Vec<ApprovalScope> },
    Modify { args: serde_json::Value }, // 可选:收窄参数后继续
}
```

推荐的检查顺序:

```text
1. 参数和路径规范化
2. hard deny policy
3. project / user rules
4. PreToolUse hooks
5. 用户审批(若仍为 Ask)
6. execute
```

**Hook 的 Allow 不能覆盖前两步的 Deny。** 否则一个方便的自动化脚本就能把
安全策略变成摆设。

### 放在哪一层

不要把审批弹窗写进 `Tool::execute`;工具层不应该认识 TUI。建议新增:

```text
src/permission/
  mod.rs        PermissionPolicy trait + decision types
  rules.rs      user/project/policy rules
  command.rs    run_command 的结构化判定
  path.rs       canonical path / symlink boundary
```

Runtime 在 `ToolRegistry::execute` 前调用 policy。TUI 只消费事件:

```rust
AgentEvent::PermissionRequested { request_id, tool, summary, reason }
AgentCommand::PermissionResponse { request_id, decision }
```

当前 Runtime 工作线程只在一条命令处理完后才回到 `cmd_rx.recv()`。如果在
`run_turn` 中等待审批,普通命令通道无法同时收回复。可选方案有两个:

1. 给 `RuntimeHandle` 增加独立 `approval_tx/approval_rx`;
2. 把 Runtime 改成显式状态机,turn 可以暂停为 `WaitingForApproval`。

教学实现可以先选方案 1;长期要支持 steering、后台通知、多个审批时,方案 2 更稳。

### 路径策略不能只做字符串前缀

可靠的 workspace 边界至少考虑:

- `..` 与绝对路径;
- 符号链接 / junction 指向 workspace 外;
- 目标尚不存在时,应 canonicalize 最近存在的父目录;
- Windows 大小写、UNC 路径和盘符;
- 读、写、删除、执行使用不同默认策略。

### 验收

- deny 的工具实现从未被调用;
- ask 被拒绝时仍生成配对的 `ToolResult{is_error:true}`;
- “本次允许”和“本会话允许”作用域不会混淆;
- symlink/junction 逃逸测试通过;
- permission hook 无法绕过硬 deny。

参考:[s03 Permission](https://learn.shareai.run/en/s03/)。

---

## 4. Hooks:扩展挂在循环上,不要写进循环里

权限之后很快还会出现日志、格式化、自动测试、输出审查、Stop 检查。如果每个功能
都在 `run_turn` 加一个 `if`,Agent Loop 会失去可读性。

先只支持四个关键事件就够了:

| Hook | 时机 | 典型用途 |
|---|---|---|
| `UserPromptSubmit` | 用户消息提交后、调模型前 | 校验、附加上下文 |
| `PreToolUse` | 权限硬规则之后、执行前 | 审计、软阻止、参数收窄 |
| `PostToolUse` | 工具返回后、写历史前 | 输出检查、附加提示 |
| `Stop` | 模型准备结束本轮时 | 验收、清理、请求继续 |

结构可以保持很小:

```rust
enum HookResult {
    Continue,
    Block(String),
    AddContext(ChatMessage),
    ReplaceToolArgs(serde_json::Value),
    PreventStop(String),
}

trait Hook: Send {
    fn event(&self) -> HookEvent;
    fn run(&mut self, ctx: &HookContext) -> anyhow::Result<HookResult>;
}
```

三个设计要求:

1. Hook 按注册顺序执行,结果合并规则必须确定;
2. Hook 自己失败时要定义 fail-open 还是 fail-closed,权限 Hook 默认 fail-closed;
3. `Stop` 阻止结束后要设置 `stop_hook_active`,否则模型修正一次、Hook 再阻止一次,
   会形成无限循环。

不要一开始实现几十个事件。四个事件覆盖完整周期,等真实扩展需要新位置时再加。

参考:[s04 Hooks](https://learn.shareai.run/en/s04/)。

---

## 5. Error Recovery:错误不是字符串,而是状态转移

### 当前实现已经做对了什么

Zerone 的 `call_model` 已经遵守一条关键原则:只有**尚未发出任何流事件**的
retryable 错误才自动重试。这避免用户看到重复文本。

但现在只有 `MAX_ATTEMPTS=3` 和 2s/4s 退避。下一步不要简单改成重试 10 次,
而要先分类:

| 故障 | 是否重试同一请求 | 恢复动作 |
|---|---|---|
| 连接失败 / 408 / 429 / 5xx | 是,限次 | Retry-After 或指数退避+抖动 |
| 流开始后的断线 | 通常否 | 保留合法历史,提示用户继续 |
| `MaxTokens` | 视策略 | 提高输出上限或提交部分输出后 continuation |
| context too long | 否 | reactive compact 后构造新请求 |
| 400 schema / tool history 错误 | 否 | 修复/隔离非法消息,不能盲重试 |
| 工具错误 | 不重调 API | 作为 Observation 交给模型自愈 |

建议把散落的计数收进 `RecoveryState`:

```rust
struct RecoveryState {
    transient_attempts: u32,
    context_compactions: u8,
    continuations: u8,
    overload_streak: u8,
    current_model: String,
}
```

### 三条主要恢复路径

#### 5.1 临时错误

延迟公式可从下面开始:

```text
delay = min(base * 2^attempt, max_delay) + random(0..25%)
```

服务端 `Retry-After` 优先。抖动不是装饰:多个 Agent 同时收到 429 时,
没有抖动会在相同时间再次撞上服务。

#### 5.2 输出截断

截断后有两种合法策略,必须二选一:

- **丢弃本次 assistant**,提高 `max_tokens` 后重试原请求;
- **提交部分 assistant**,追加明确 continuation 消息后继续。

不能既把部分输出写入历史,又无提示地重试原请求,否则内容重复。连续 continuation
还需要收益检测和上限,不能无限“继续”。

#### 5.3 上下文溢出

它不是瞬时错误,等待不会变好。应触发第 6 节的 reactive compact,最多一次或两次;
仍失败就停止,并把诊断信息留给用户。

### 降级模型不是无成本切换

不同模型可能支持不同工具 schema、thinking block 和上下文长度。切换模型前要验证:

- 目标 provider 能否编码当前历史;
- 未完成工具调用是否已配对;
- reasoning 私有块能否安全丢弃;
- 新模型的 context/output 上限。

### 验收

- retryable 错误最终成功时,副作用工具只执行一次;
- `Retry-After` 被采用,取消能打断退避;
- 400 不进入重试风暴;
- context overflow 只触发有限次压缩;
- 每条终止路径都有明确 `AgentEvent` 和持久化状态。

参考:[s11 Error Recovery](https://learn.shareai.run/en/s11/)。

---

## 6. Context Compact:压缩“模型视图”,不要销毁“事实日志”

### 先分清两个东西

现在 `Conversation` 同时承担:

1. 会话事实历史;
2. 下一次请求要发送的上下文。

数据量小时二者相同,做压缩后必须分开:

```text
SQLite Session Log     完整、可审计、尽量不删
Conversation State     当前会话的结构化状态
Prompt History View    预算内真正发给模型的消息
```

**不要为了省 token 删除 SQLite 中的原始消息。** `/session` 恢复、调试和未来重新
生成摘要都需要事实日志。压缩结果应是新的 summary/checkpoint,或
`Conversation::contribute` 生成的投影视图。

### 四层管线:便宜的先做

推荐顺序不是随意的:

1. **tool result budget**:超大结果先持久化,上下文只留引用和预览;
2. **structural trim**:裁掉中间的低价值旧轮次;
3. **micro compact**:旧 ToolResult 替换成“已省略,可重新读取”的占位;
4. **LLM summary**:仍超阈值时才花一次模型调用生成摘要。

为什么大结果持久化要在 micro compact 之前?后者一旦把正文替换掉,前者就没有完整
内容可保存了。

### 需要先改 Conversation

当前 `Conversation.messages` 是私有 `Vec`,只有 `push/clear/contribute`。
建议增加的不是一堆随意 getter,而是明确操作:

```rust
impl Conversation {
    fn prompt_view(&self, budget: &ContextBudget) -> PromptHistory;
    fn checkpoint(&mut self, summary: Summary, covered_through: usize);
    fn validate_tool_pairs(&self) -> Result<()>;
}
```

`PromptHistory` 应保存“摘要覆盖到哪条消息”“哪些结果被外置”“预计 token 数”,
这样恢复和诊断才有依据。

### ToolUse 边界

任何裁剪算法都不能留下:

- 没有 ToolResult 的 ToolUse;
- 没有 ToolUse 的 ToolResult;
- 多工具调用只保留其中一半结果。

最简单的做法不是逐消息裁,而是先把历史切成 logical turn / tool batch,
压缩以 batch 为单位。

### token 预算

`max_tokens` 在当前配置里代表输出上限,不是模型上下文窗口。需要独立配置:

```toml
[agent.context]
window_tokens = 128000
reserve_output_tokens = 16000
compact_at_percent = 80
```

早期可以用字符数估算,但阈值要保守;后续按 provider/model 接 tokenizer。

### 大结果外置

可以放在 `.zerone` 的会话目录旁,例如:

```text
~/.zerone/session-artifacts/<session-id>/tool-results/<call-id>.txt
```

上下文保留结构化标记、摘要、大小和校验值。不要把绝对路径暴露给不可信工具;
通过受控读取工具重新加载。

### 验收

- 10 万行工具输出不会直接进入 prompt;
- 压缩前后 `validate_tool_pairs()` 都通过;
- `/session` 仍能恢复完整原始历史;
- summary 明确保留当前目标、已完成工作、未完成工作、用户约束、关键文件;
- summary 失败不会覆盖上一个有效 checkpoint;
- reactive compact 有严格次数上限。

参考:[s08 Context Compact](https://learn.shareai.run/en/s08/)。

---

## 7. Memory:会话存储、摘要和长期记忆不是一回事

Zerone 已有 SQLite 会话,但“能找回聊天记录”不等于“Agent 有记忆”。三者用途不同:

| 层 | 保存什么 | 生命周期 | 是否直接进 prompt |
|---|---|---|---|
| Session log | 完整消息和工具往返 | 单会话,跨进程 | 经压缩后选择性进入 |
| Session summary | 当前目标、进度、约束 | 单会话,跨压缩 | 是 |
| Long-term memory | 跨会话仍有价值的偏好/项目知识 | workspace 或 user | 目录常驻,正文按需 |

### 推荐存储布局

不要默认污染用户仓库。可以复用 `AppPaths` 和 `workspace_key`:

```text
~/.zerone/projects/<workspace-key>/memory/
  MEMORY.md
  user-prefers-tabs.md
  project-auth-entrypoint.md
```

每个正文文件可使用 frontmatter:

```markdown
---
name: user-prefers-tabs
description: User requires tabs for indentation
type: user
source_session: 8db0...
updated_at: 2026-08-03T12:00:00Z
---

Use tabs, not spaces, when editing this workspace.
```

`MEMORY.md` 只放短目录,作为 `MemoryIndexContext` 注入;正文按需加载。这个两层设计
与 skill 相似,目的都是避免把全部知识永久塞进 system prompt。

### 写入路径

先实现显式命令或工具 `/remember`,再考虑自动抽取。自动抽取需要回答:

- 谁说的才算事实?用户声明与模型推测权重不同;
- 新记忆是否与旧记忆重复或冲突;
- 是否包含 secret、路径、个人信息;
- 何时合并/过期/删除;
- 写入失败会不会影响主 turn。

可靠做法是把抽取放在 turn 正常停止后的低优先级任务中,先产出候选,
经过规则或用户确认再落盘。记忆内容是**不可信上下文**,不能拥有高于系统策略的权限。

### 选择与合并

MVP 可以先按 name/description 关键词选最多 5 条;再升级为一个受限 side-query。
无论使用模型还是 embedding,都要有:

- 数量和 token 上限;
- 失败回退;
- 去重与冲突策略;
- workspace 隔离;
- 可查看、可编辑、可删除的用户界面。

### 验收

- 新会话能按 workspace 找到旧偏好;
- 无关记忆不会每轮注入;
- 删除记忆后索引同步更新;
- 冲突记忆不会静默覆盖;
- memory 文件中的命令性文本不能绕过 permission policy。

参考:[s09 Memory](https://learn.shareai.run/en/s09/)。

---

## 8. Background Tasks:不要把“后台完成”伪装成第二个 ToolResult

当前 `run_command` 会阻塞 Runtime 工作线程。安装依赖、全量测试、构建等慢命令期间,
Agent 不能处理其他工作。

后台执行最容易犯的错误是:工具启动时先返回一个 ToolResult,完成后又用同一个
`tool_use_id` 返回第二个 ToolResult。这样会破坏三家 API 都依赖的配对关系。

正确协议:

```text
ToolUse(run_command, run_in_background=true)
  -> ToolResult("started: bg_123")       # 原调用立即配对

后台完成
  -> 独立 Notification("bg_123 done", output_ref)
  -> 下一次合适的 user message 注入模型
```

需要新增的不是一个线程而已,而是一套生命周期:

```rust
enum BackgroundStatus { Running, Completed, Failed, Cancelled, Lost }

struct BackgroundTask {
    id: String,
    session_id: String,
    status: BackgroundStatus,
    output_path: PathBuf,
    started_at: i64,
}
```

以及对应事件:`BackgroundStarted/Progress/Finished`。可靠版本还要处理:

- stdout/stderr 持续写文件,不能无限留内存;
- 进程退出后如何识别遗留 PID;
- Zerone 重启后 Running 任务标记为 Lost 还是重新接管;
- 交互式提示导致的卡死检测;
- 每 workspace 并发上限;
- 用户取消与进程树清理。

如果未来允许多个普通工具并发执行,给工具增加明确元数据:

```rust
struct ToolCapabilities {
    read_only: bool,
    concurrency_safe: bool,
    destructive: bool,
}
```

只并发执行**同一连续 batch 中明确标为 concurrency-safe** 的工具,结果仍按原
ToolUse 顺序组装。不能因为 `read_file` 可并发,就推断 `edit_file` 也可以。

参考:[s13 Background Tasks](https://learn.shareai.run/en/s13/)。

---

## 9. Storage 与可观测性:从“能保存”到“能迁移、能解释”

当前一会话一 SQLite 文件已经解决了进程退出丢历史的问题。可靠版本还需要:

### 9.1 Schema version 与迁移

使用 `PRAGMA user_version` 或独立 migration 表。启动时按版本逐步迁移,
不要只依靠 `CREATE TABLE IF NOT EXISTS`;它不会给旧表补新字段。

迁移原则:

- 先备份或使用事务;
- 每个版本可重复检测;
- 新二进制遇到更高版本数据库时拒绝写入;
- 迁移失败仍保留原库。

### 9.2 Event / audit log

消息历史回答“模型看到了什么”,但不能回答:

- 第几次重试成功;
- 哪条权限规则允许了命令;
- Hook 修改了什么参数;
- 工具运行多久、改了哪些文件;
- 后台任务何时丢失。

增加 append-only `events` 表,记录结构化 kind、timestamp、turn_id、tool_call_id、
payload。API key、环境变量和大块输出不能直接记日志。

### 9.3 写入失败策略

当前 `persist()` 失败后发 Error,Agent 仍继续在内存里运行。长期要明确模式:

- **strict**:无法持久化就停止产生新副作用;
- **best-effort**:继续,但状态栏持续标红并允许导出内存历史。

默认更适合 strict,否则用户以为会话安全,实际下一次启动什么都没有。

---

## 10. 两条实施路线

学习顺序和产品优先级并不完全相同。

### 学习顺序

```text
Prompt Assembly
  -> Permission
  -> Hooks
  -> Error Recovery
  -> Context Compact
  -> Memory
  -> Background / Concurrency
  -> Migration / Observability
```

每一步都能看到新机制如何挂进同一个 Loop,与参考课程的递进方式一致。

### 想尽快提高日常可用性

| 优先级 | 工作 | 原因 |
|---|---|---|
| P0 | workspace guardrail + permission | 先控制副作用 |
| P0 | Runtime scripted tests + 错误分类 | 没有它不敢继续重构 |
| P1 | context budget + tool result 外置 | 长任务不会突然报废 |
| P1 | schema migration + strict persistence | 保存的数据真正可信 |
| P1 | Hooks | 后续能力不继续污染 Loop |
| P2 | prompt assembly | 能力增多后保持上下文聚焦 |
| P2 | explicit memory | 跨会话保留用户确认的知识 |
| P2 | background tasks | 改善长命令体验 |
| P3 | automatic memory / model fallback / 并行工具 | 价值高,但状态空间也大 |

---

## 11. 每完成一层都问这六个问题

1. **状态归谁所有?** Agent、session、workspace 还是全局?
2. **何时持久化?** 单条写还是事务?崩溃点在哪里?
3. **如何取消?** 取消后历史是否合法,子进程是否还活着?
4. **如何重试?** 会不会重复副作用?
5. **如何观察?** 用户能否知道它在等待审批、压缩还是后台运行?
6. **如何测试?** 不访问真实 API 能否确定性覆盖失败路径?

能回答这六个问题,才算把一个 demo 功能变成了 harness 能力。

下一篇:[08b · tools 篇](08b-more-and-more_tools.md) 会沿用这些底层边界,
具体讨论 Todo、Subagent、Skills、Task System、后台命令和 MCP 应该怎样接入。

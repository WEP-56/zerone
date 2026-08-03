# 08b · 从五个工具到能力平台:tools 篇

> [06](06-how-to-add-tool.md) 已经完整演示了怎样实现一个 `grep` 工具。
> 本篇不再重复“新建文件、实现 trait、注册一行”,而是讨论更难的问题:
> 当工具开始拥有状态、递归调用 Agent、后台运行或来自外部服务器时,
> Zerone 的工具契约要怎样继续生长,又怎样避免把 Agent Loop 改成一团条件分支。

本文同样参考
[Learn Claude Code](https://learn.shareai.run/en/s01/) 的分层方式,重点对应
[Tool Use](https://learn.shareai.run/en/s02/)、
[TodoWrite](https://learn.shareai.run/en/s05/)、
[Subagent](https://learn.shareai.run/en/s06/)、
[Skills](https://learn.shareai.run/en/s07/)、
[Task System](https://learn.shareai.run/en/s12/)、
[Background Tasks](https://learn.shareai.run/en/s13/) 和
[MCP Tools](https://learn.shareai.run/en/s19/)。参考课程演示机制,本篇负责把机制
落到 Zerone 当前的 Rust 类型和模块边界上。

底层权限、Hooks、压缩、记忆等问题见
[08a 底层篇](08a-more-and-more_basic.md)。

---

## 1. 先看清现有工具契约

Zerone 当前的核心非常小:

```rust
pub trait Tool: Send {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn execute(
        &self,
        args: &Value,
        ws: &Workspace,
        cancel: &AtomicBool,
    ) -> Result<String, String>;
}
```

`ToolRegistry` 做两件事:

```text
specs()     Tool -> 厂商无关 ToolSpec -> Provider 编码
execute()   name -> Tool -> sanitize + truncate -> ToolOutcome
```

这已经体现了最重要的设计原则:

> 增加工具时,Agent Loop 不变。定义能力,注册 handler,完成。

只要新工具是“输入 JSON,同步返回文本”的局部能力,当前 trait 足够。比如:

- `glob`:按模式找文件;
- `grep`:搜索文本;
- `git_status`:返回结构化仓库状态;
- `read_many`:批量读取少量文件;
- `apply_patch`:应用明确 patch。

真正的复杂度从下面开始。

---

## 2. 工具复杂度分级

先判断新能力属于哪一级,再决定改多少层:

| 级别 | 特征 | 例子 | 主要改动 |
|---|---|---|---|
| L1 无状态同步工具 | 一次调用结束,无跨轮状态 | grep / glob / git_status | 新 Tool + 注册 |
| L2 有状态工具 | 状态要在后续轮次可见 | todo_write | Tool + state + Context |
| L3 复合工具 | 内部编排其他能力 | subagent / task | Tool + service/runtime |
| L4 异步工具 | 调用返回后工作仍继续 | background command | Tool + task manager + event |
| L5 动态外部工具 | 名字/schema/连接运行时出现 | MCP | 动态 registry + lifecycle + permission |

如果把 L2-L5 强行塞进当前 `execute(args, ws, cancel) -> String`,通常会出现:

- `static mut` 或全局 `Mutex<HashMap<...>>`;
- 工具直接操作 TUI channel;
- 工具自己打开 session DB;
- 子 Agent 偷偷复制一份 Runtime;
- 后台完成后伪造第二个 ToolResult;
- MCP 工具为了动态名字泄漏字符串。

这些都说明能力已经跨层,应该先升级契约。

---

## 3. 写工具前的设计卡

任何新工具动手前,先填完这张卡。大部分工具 bug 不是 Rust 写错,而是契约没想清楚。

| 问题 | 示例答案 |
|---|---|
| 模型何时应该调用? | “查找符号定义时优先于 run_command” |
| 何时不该调用? | “已经知道精确路径时直接 read_file” |
| 最少参数是什么? | `pattern` 必填,`path/max_results` 可选 |
| 输出给模型做什么决定? | 文件、行号、短预览足够继续读取 |
| 没结果是成功还是错误? | grep 无匹配是成功 Observation |
| 有什么副作用? | 读 / 写 / 删除 / 执行 / 外部网络 |
| 能否取消? | 循环中每 N 次检查 `cancel` |
| 能否并发? | 只读且没有共享游标才可并发 |
| 最大输出多大? | 超限时截断、分页还是外置 |
| 需要什么权限? | workspace 内读自动允许,外部路径询问 |
| 如何确定性测试? | 临时 workspace + 固定参数 + 不联网 |

### description 不是 README

`description` 的读者是模型。它应该包含:

1. 使用时机;
2. 与相近工具的边界;
3. 关键前置条件;
4. 结果形状。

坏例子:

```text
Search files.
```

更好的例子:

```text
在 workspace 内递归搜索包含指定文本的行,返回 path:line:preview。
找符号或配置项时优先使用;已知精确文件路径时改用 read_file。
无匹配是正常结果,不会修改文件。
```

模型不会因为 schema 正确就自动理解工具边界。description 就是工具选择器的一部分。

### schema 要窄

- 用 `enum` 表达有限选项;
- 给整数加 minimum/maximum;
- 不要同时提供三个语义重叠的参数;
- 能由工具推导的值不要让模型填写;
- 写操作尽量要求精确 old/new 或 patch,不要接模糊自然语言。

### 错误也是 Observation

当前 registry 把 `Err(String)` 转成 `ToolOutcome{is_error:true}` 回给模型。这很重要。
错误文案要告诉模型下一步:

```text
差: IO error 2
好: path "src/foo.rs" 不存在;请先 list_dir("src") 确认文件名
```

panic、堆栈和内部类型名既不能帮助模型恢复,也可能泄漏实现细节。

---

## 4. 先升级 ToolSpec:让策略看到工具属性

权限、并发和后台执行都需要工具元数据。不要靠工具名硬编码:

```rust
#[derive(Debug, Clone)]
pub struct ToolCapabilities {
    pub read_only: bool,
    pub destructive: bool,
    pub concurrency_safe: bool,
    pub supports_background: bool,
}

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub capabilities: ToolCapabilities,
}
```

这些字段不是模型承诺,而是 harness 自己用于调度和权限的声明。尤其是外部 MCP
提供的 `readOnlyHint/destructiveHint`,只能作为输入信号,不能无条件信任。

动态工具还要求把:

```rust
fn name(&self) -> &'static str
```

改为:

```rust
fn name(&self) -> &str
```

否则运行时发现的 `mcp__server__tool` 无法自然实现 `Tool`。

---

## 5. 再升级执行上下文:不要让复杂工具抓全局变量

只有 `Workspace + cancel` 时,工具拿不到 session、事件、TodoStore、TaskManager 等服务。
与其不断给 `execute` 加参数,不如引入窄的 `ToolContext`:

```rust
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub cancel: &'a AtomicBool,
    pub session_id: &'a str,
    pub services: &'a ToolServices,
}

pub struct ToolServices {
    pub todos: TodoStore,
    pub tasks: TaskStore,
    pub background: BackgroundTaskManager,
    pub skills: SkillRegistry,
}

pub trait Tool: Send {
    fn spec(&self) -> ToolSpec;
    fn execute(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}
```

这里的重点不是把所有东西塞进 service locator。每个工具只应拿到需要的服务;
可以把大 `ToolServices` 再拆成更小 trait。重要的是:

- 状态由 Agent 装配并显式传入;
- 测试可以传 fake service;
- 工具不直接认识 TUI;
- 工具不自行寻找 `~/.zerone`;
- 所有副作用仍经过 Runtime 的统一权限/Hook/审计管线。

`ToolOutput` 后续也可以从纯字符串升级:

```rust
struct ToolOutput {
    model_text: String,
    ui_summary: Option<String>,
    attachments: Vec<ArtifactRef>,
}
```

模型正文、TUI 摘要和大附件从此不必共用一个 24K 截断字符串。

---

## 6. TodoWrite:第一个有状态工具

### 它解决的不是执行,而是注意力

复杂任务进行十几轮后,模型容易被最新的测试失败吸走,忘记原目标。
`todo_write` 不增加读写能力,它把计划变成每轮可见、可更新的外部状态。

### 数据模型

```rust
#[derive(Serialize, Deserialize, Clone)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Serialize, Deserialize, Clone)]
struct TodoItem {
    id: String,
    content: String,
    status: TodoStatus,
}
```

最小不变量:

- ID 唯一且更新时不能偷偷换 ID;
- 同时最多一个 `InProgress`;
- 空列表表示明确清空,不是解析失败;
- status 必须由 enum 校验;
- 单次更新要原子替换整张列表,或定义清楚 patch 语义。

### 状态放哪里

不要使用进程全局 `CURRENT_TODOS`。它会让两个会话互相覆盖。更合理的是:

```text
Agent
  -> PlanningState(TodoStore)
  -> TodoWriteTool(共享同一个 store)
  -> PlanningContext(读取同一个 store)
```

Tool 负责更新,ContextProvider 负责每轮把短计划注入 prompt,TUI 通过事件展示。
这就是一个 L2 工具为什么不可能只改 `tools/todo_write.rs`。

### reminder 应该是状态驱动

教学版本常用“连续 3 轮没更新 TODO 就提醒”。它能展示机制,但生产实现应观察:

- 当前是否真的有未完成 TODO;
- 是否刚完成了一个 item;
- 最近工具调用是否与当前 item 相关;
- 是否处于简单任务,根本不需要计划。

reminder 适合做 `BeforeModel` Hook 或 `PlanningContext`,不要把计数散在
`run_turn` 多处。

### 是否持久化

Todo 是“当前 turn/当前目标的执行清单”。可以存进 session DB,随 `/session` 恢复;
它不是第 9 节的跨会话任务系统。

### 验收

- 两个会话的 TODO 不串;
- 恢复 session 后计划和状态一致;
- malformed list 不覆盖旧计划;
- 简单问答不会被强迫创建 TODO;
- Context 中只出现短计划,不是全部变更历史。

参考:[s05 TodoWrite](https://learn.shareai.run/en/s05/)。

---

## 7. Subagent:一个工具内部启动另一套 Agent

### 它不是“开一个终端”

Subagent 的本质是**上下文隔离**:

```text
Parent conversation
  -> ToolUse(task="追踪登录调用链")
  -> Child Agent + fresh Conversation
       -> 读 30 个文件、跑搜索、形成结论
  -> ToolResult("调用链结论...")
Parent 只保留结论,不携带 30 个文件的中间过程
```

文件系统副作用仍共享 workspace;隔离的是消息和注意力,不是世界。

### 先做同步版本

第一版让 parent 等 child 完成,更容易保证 ToolUse 配对和权限。需要抽出
`AgentFactory` 或 `AgentBuilder`,避免复制 `Agent::new`:

```rust
struct AgentSpec {
    provider: ProviderSettings,
    system_prompt: String,
    allowed_tools: Vec<String>,
    max_turns: u32,
    depth: u8,
}

trait AgentFactory {
    fn build(&self, spec: AgentSpec, workspace: Workspace) -> Result<Agent>;
}
```

当前 `Config` 不可直接 Clone、provider 是 trait object、session 创建默认落盘,
这些都会在实现 Subagent 时暴露出来。不要绕过它们复制半套构造逻辑;
应该明确 child 是否:

- 使用父 provider/model 的新实例;
- 创建可见 session,还是临时 session;
- 继承 system 稳定前缀;
- 允许写工具;
- 继承 permission mode;
- 允许继续 spawn。

### 工具集要收窄

研究型 child 通常只需要 `read_file/list_dir/grep`;写代码的 child 才需要 edit。
默认禁用 `spawn_subagent` 防止无界递归,同时保留 `depth/max_turns/token budget`
三重上限。

### 权限不能消失

“child 看不到 parent 历史”不代表“child 可以绕过审批”。Subagent 的工具调用仍走
同一 PermissionPolicy。需要用户确认时,审批请求带上 `agent_id/parent_id`,
冒泡到主 TUI。

### 返回什么

只返回结论不等于返回最后一条文本。最好要求结构化结果:

```json
{
  "summary": "...",
  "evidence": ["src/auth.rs:42", "src/router.rs:18"],
  "files_changed": [],
  "remaining_risks": []
}
```

parent 才能判断结论是否可信,也便于 TUI 展示。

### 不要过早并行

同步 child 正确后再做后台/并行。多个可写 child 共享 workspace 会产生真实竞态;
可靠方案需要 worktree 隔离、文件所有权或明确只读。

参考:[s06 Subagent](https://learn.shareai.run/en/s06/)。

---

## 8. Skills:目录常驻,正文按需加载

### 为什么不把规范全塞 system prompt

React 规范、SQL 风格、发布流程可能各有几千行。全部常驻会浪费 token,
还让无关规则干扰当前任务。

Skill 适合两层加载:

| 层 | 内容 | 注入位置 | 成本 |
|---|---|---|---|
| Catalog | name + description | system/context | 每轮少量 |
| Body | 完整 SKILL.md | load_skill 的 ToolResult | 按需 |

### 目录

可以支持两个明确来源:

```text
~/.zerone/skills/<name>/SKILL.md       # 用户级
<workspace>/.zerone/skills/<name>/SKILL.md  # 项目级
```

项目级同名覆盖用户级时要有明确优先级,并在 `/skills` 中显示来源。

### 启动扫描

使用真正的 frontmatter parser,不要手写 `split("---")`。至少读取:

```yaml
---
name: rust-review
description: Review Rust changes for correctness and unsafe assumptions
---
```

注册表按 name 索引**已经验证过的 canonical path**。`load_skill` 只接 name,
不接任意路径,避免 `../../secrets`。

```rust
struct SkillRecord {
    name: String,
    description: String,
    root: PathBuf,
    body_path: PathBuf,
    source: SkillSource,
}
```

### 与 Context 的配合

`SkillCatalogContext` 生成目录;`LoadSkillTool` 返回正文。正文里可以指导模型随后
用 `read_file/run_command` 访问 `references/`、`scripts/` 或 `assets/`,但所有访问
仍必须走普通工具权限。

Skill 文件来自项目或插件,属于**不可信输入**。它不能覆盖 system policy,
不能声明自己免审批,也不能直接获得 secret。

### 缓存失效

目录变化后必须同步更新:

- catalog context;
- `load_skill` registry;
- prompt cache key;
- TUI 列表。

只有 body 内容变化时,不应让所有无关 skill 正文进入 prompt。

参考:[s07 Skills](https://learn.shareai.run/en/s07/)。

---

## 9. Task System:不要把 TodoWrite 硬扩成项目管理器

Todo 和 Task 回答不同问题:

| | Todo | Task System |
|---|---|---|
| 用途 | 当前目标的执行清单 | 可恢复、可协调的工作单元 |
| 生命周期 | 当前 session/目标 | 跨 session |
| 依赖 | 无 | blocked_by / blocks |
| 所有权 | 当前 Agent | 可 claim / release |
| 并发 | 不处理 | 必须防重复领取 |

任务模型至少需要:

```rust
struct TaskRecord {
    id: String,
    subject: String,
    description: String,
    status: TaskStatus,
    owner: Option<String>,
    blocked_by: Vec<String>,
    version: u64,
}
```

建议暴露多个窄工具而不是一个万能 `task(action=...)`:

- `task_create`;
- `task_get`;
- `task_list`;
- `task_update` 或独立 `task_claim/task_complete`。

窄工具 schema 更清楚,权限和错误也更好解释。

### 存储和并发

既然 Zerone 已用 SQLite,任务系统不必退回“一任务一个 JSON 文件”。可以按 workspace
建 task DB,使用事务和乐观版本:

```sql
UPDATE tasks
SET owner = ?, status = 'in_progress', version = version + 1
WHERE id = ? AND status = 'pending' AND version = ?;
```

受影响行数为 0 表示已被别人领取。依赖检查和 claim 应放在同一事务,
否则两个 Agent 都可能在检查后成功领取。

还要做:

- DAG cycle 检测;
- missing dependency 视为 blocked;
- owner 崩溃后的 lease/release;
- ID 永不复用;
- 完成任务后列出刚解除阻塞的任务。

参考:[s12 Task System](https://learn.shareai.run/en/s12/)。

---

## 10. Background Command:工具返回不等于工作完成

慢命令需要一个新的生命周期,不能简单在线程里 `spawn` 后返回“started”。

给 `run_command` 增加明确参数:

```json
{
  "command": "cargo test --all",
  "run_in_background": true
}
```

模型显式选择优先于“看到 install/build 就猜很慢”的启发式。

### ToolUse 配对协议

启动时立即返回唯一结果:

```text
ToolResult: started background task bg_123; use background_get to inspect
```

完成事件是普通通知,不能复用原 `tool_use_id`:

```text
<task_notification id="bg_123" status="completed" exit_code="0">
output stored at ...
</task_notification>
```

通知进入下一条合适的 User 消息,或先通过 `AgentEvent::BackgroundFinished` 展示,
等待下一次模型调用再注入。完整 Runtime 改动见 08a。

### 配套工具

- `background_get{id, tail_lines}`;
- `background_list{status?}`;
- `background_cancel{id}`。

输出持续写文件并限制磁盘配额;`background_get` 只读 tail。任务状态应记录 session 和
workspace,不能让一个项目看到另一个项目的日志。

### 交互式卡死

后台命令也必须 stdin=null。额外检测长时间无输出和常见 `(y/n)` 提示,
否则“后台”只是把永久卡死藏起来。

参考:[s13 Background Tasks](https://learn.shareai.run/en/s13/)。

---

## 11. 多工具调用:并发不是把 for 改成 spawn

当前 Runtime 按模型返回顺序逐个执行,这是最保守也最容易验证的行为。
只有证明需要并发后再升级。

### 哪些通常可以并发

- 读取不同文件;
- 多个独立搜索;
- 无共享游标的纯查询。

### 哪些默认不能并发

- 两次 edit/write;
- 会改变 cwd、git index、依赖锁文件的命令;
- Todo/Task 的状态更新;
- 权限请求;
- 声明不清楚的 MCP 工具。

### batch 算法

不要把所有调用一起并发。按原顺序切连续 batch:

```text
[read A, read B] [edit C] [read D, grep E] [run tests]
    parallel      serial       parallel         serial
```

每个 batch 完成后再进入下一个。最终 ToolResult 仍按原 ToolUse 顺序排列,
不能按线程完成顺序写历史。

并发还需要:

- batch 级取消;
- panic/线程失败转 ToolResult;
- 并发上限;
- 单工具 timeout;
- 审批先完成再执行;
- 相同路径的读写冲突检测。

---

## 12. MCP:让工具来源从编译期变成运行期

### MCP 改变了什么

内置工具在 `default_registry()` 编译期固定。MCP 连接后才通过 `tools/list`
发现名字、description 和 schema,再通过 `tools/call` 执行。

这要求 Registry 支持:

- 动态注册/注销;
- 工具池 generation/version;
- 连接状态;
- 名字冲突处理;
- schema 校验;
- prompt/spec cache 失效。

### 命名

使用命名空间避免两个 server 都有 `search`:

```text
mcp__<server>__<tool>
```

server/tool 名先规范化为 `[A-Za-z0-9_-]`;规范化后仍冲突就拒绝连接,
不能静默覆盖。

### 适配成 Tool

```rust
struct McpTool {
    full_name: String,
    server_id: String,
    remote_name: String,
    description: String,
    schema: Value,
    client: Arc<McpClient>,
}
```

`execute` 把 args 交给 client,将 MCP content blocks 规范化成 `ToolOutput`。
transport/JSON-RPC 错误变成可行动的 ToolError,连接断开与远端业务错误要区分。

### 连接不应由模型任意决定

教学 demo 可以提供 `connect_mcp` 工具展示动态发现;可靠 CLI 更适合从
`~/.zerone/config.toml` 读取允许的 server,启动时连接或由用户命令连接。
否则模型可以自行启动未知进程或访问未知 URL。

### transport 生命周期

stdio MCP 至少处理:

- 子进程启动和握手 timeout;
- stdout 只允许 JSON-RPC,stderr 单独收集;
- 请求 ID 和并发响应;
- server crash 后 pending request 全部失败;
- shutdown / kill tree;
- 重连退避;
- schema/tool list 变化通知。

远程 transport 还要处理 OAuth、证书、代理、重定向和 secret 存储。

### 权限

MCP 注解 `readOnlyHint/destructiveHint` 进入 PermissionPolicy,但不能直接等于 Allow。
用户规则仍可 deny;未知工具默认 Ask。server 返回的文本和 skill 一样属于不可信内容。

### Prompt cache

连接/断开 MCP 会改变工具 specs。Registry 每次变化递增 generation:

```rust
struct ToolRegistry {
    generation: u64,
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
```

`generation` 进入 prompt/tool cache key。不能继续使用旧工具列表,否则模型看不到新工具,
或继续调用已经断开的工具。

参考:[s19 MCP Tools](https://learn.shareai.run/en/s19/)。

---

## 13. 输出预算:不要让 Registry 的 24K 截断成为数据黑洞

当前统一 `truncate_middle(..., 24_000)` 对 MVP 很实用,但复杂工具需要三种策略:

| 输出类型 | 策略 |
|---|---|
| 小结构化结果 | 直接返回 |
| 可分页列表 | cursor/offset + limit |
| 大文件/日志/测试结果 | 外置 artifact + 摘要 + 可重新读取引用 |

Tool 自己应该声明输出策略,Registry 负责统一执行。被截断的内容如果没有落盘,
模型无法恢复,这不是“节省上下文”,而是静默丢数据。

推荐 `ArtifactRef` 至少包含:

```rust
struct ArtifactRef {
    id: String,
    media_type: String,
    byte_len: u64,
    sha256: String,
    session_id: String,
}
```

再提供受权限控制的 `read_artifact`。不要把任意绝对路径当成 artifact ID。

---

## 14. 工具测试矩阵

一个工具只有 happy path 测试远远不够。最低矩阵:

| 维度 | 用例 |
|---|---|
| schema | 缺字段、错类型、边界值、额外字段 |
| path | 相对/绝对、`..`、symlink、Unicode、空路径 |
| side effect | 成功、部分失败、重复执行、权限拒绝 |
| output | 空、非 UTF-8、ANSI、超大、很多行 |
| cancellation | 执行前、执行中、完成瞬间 |
| history | ToolUse/ToolResult ID 配对、并发顺序 |
| persistence | 崩溃恢复、旧 schema、跨 session 隔离 |
| provider | Messages / Chat / Responses 形状都合法 |

不同级别再增加专项测试:

- Todo:两个 session 不串状态;
- Subagent:depth/tool subset/permission bubbling;
- Task:并发 claim 只有一个成功;
- Background:原 ToolResult 只有一次,完成通知独立;
- MCP:断线使 pending calls 失败并注销工具。

`tests/wire.rs` 继续负责 provider 形状;Runtime scripted provider 负责状态机;
工具自己的模块测试负责文件系统和业务逻辑。不要用真实 LLM 验证确定性逻辑。

---

## 15. 推荐实现顺序

不要从 MCP 或并行 Subagent 开始。按状态空间从小到大:

```text
1. grep/glob 等 L1 工具,练好 description/schema/error
2. ToolCapabilities + ToolContext
3. TodoWrite + PlanningContext
4. Skills catalog + load_skill
5. 持久 Task System
6. 同步、只读 Subagent
7. Background command + notifications
8. concurrency-safe batch
9. MCP dynamic registry
10. 可写并行 Subagent + worktree isolation
```

每一步都应满足:

- `run_turn` 没有出现该工具名字;
- provider 适配器没有出现该工具名字;
- TUI 只消费通用事件,不调用工具实现;
- 权限、取消、持久化和输出预算有明确答案;
- 至少一个失败路径测试。

如果新增一个工具必须同时在 Runtime、三个 Provider 和 TUI 各写一个分支,
先停下来修抽象。工具平台的目标不是“能注册更多名字”,而是让新能力沿着同一条
声明、权限、执行、观察、持久化管线进入系统。

这也是从 demo harness 走向可靠 coding agent 的关键变化:
**Loop 越来越稳定,能力越来越丰富,但每个副作用仍然可解释、可取消、可恢复。**

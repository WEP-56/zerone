# 05 · Pi 的 Context 与 Session:事实日志不等于模型视图

> 对应 Zerone 基线:[05 · Context 系统](../05-context.md)。
> Pi 重点源码:`agent/src/types.ts`、`agent-loop.ts`、
> `harness/session/`、`harness/messages.ts`、`ai/api/transform-messages.ts`。

## Zerone 当前实现

Zerone 每次调用模型前创建 `PromptContext`,依次调用三个 `ContextProvider`:

```text
Instructions     → system section
WorkspaceInfo    → system section
Conversation     → 全量 ChatMessage[]
```

`ContextProvider` 是一个清楚的装配点,但当前 `Conversation` 只是线性 Vec 的全量克隆;
SQLite 也只保存这份线性模型消息。于是四个概念暂时重合:

```text
屏幕历史 = 运行历史 = 持久历史 = 模型上下文
```

MVP 阶段这很省事。加入压缩、UI-only 通知、分支、模型切换、后台任务或长期记忆后,
四者必须分开,否则“省 token”会变成删除事实,“显示通知”会变成污染 prompt。

## Pi 没有照搬 ContextProvider

Pi 把上下文构造拆成三个连续边界:

```text
SessionTreeEntry[]
    │ Session.buildContext()
    ▼
AgentMessage[]
    │ transformContext()
    ▼
AgentMessage[]
    │ convertToLlm()
    ▼
Message[]
    │ provider-specific transform/encode
    ▼
API payload
```

每一层解决不同问题:

- Session 决定当前分支有哪些事实、压缩摘要怎样替代旧前缀;
- `transformContext` 做本轮动态修剪或外部上下文注入;
- `convertToLlm` 过滤 UI-only 自定义消息并转成模型认识的角色;
- Provider transform 处理目标模型模态、thinking 签名、ID 和协议合法性。

这不是四次重复转换。它们分别属于持久语义、应用语义、AI 语义和厂商语义。

## AgentMessage 是应用域,Message 是模型域

Pi 的 `AgentMessage` 是标准 `Message` 加应用自定义消息的联合。教学切片中注册了:

- `bashExecution`:命令、输出、退出码、是否取消、完整日志引用;
- `custom`:应用自定义内容,可控制是否显示;
- `branchSummary`:从离开的分支带回来的摘要;
- `compactionSummary`:被压缩前缀的摘要。

[harness/messages.ts](../../example/pi/packages/agent/src/harness/messages.ts) 的
`convertToLlm` 决定:

- `excludeFromContext` 的 bash 记录直接过滤;
- bash execution 格式化成 User 文本;
- branch/compaction summary 包在明确 XML-like boundary 中;
- 标准 user/assistant/toolResult 原样通过;
- 未认识的应用消息不进入模型。

因此“保存在 Session”不等于“每轮花 token”。Zerone 可用稳定枚举实现同一效果:

```rust
enum SessionEntryPayload {
    Message(ChatMessage),
    CommandExecution(CommandRecord),
    Notice(NoticeRecord),
    Compaction(CompactionRecord),
    Custom { kind: String, data: Value },
}
```

再由 projector 决定哪些记录投影成 `ChatMessage`。

## transformContext 是每次请求前的模型视图 Hook

在 `streamAssistantResponse` 中,Pi 严格先调用:

```text
transformContext(AgentMessage[]) → convertToLlm(Message[])
```

`transformContext` 适合:

- 根据 token 预算裁剪旧 observation;
- 注入当前 workspace map、计划或短期 memory;
- 对本轮做临时 redaction;
- 在不改变 Session 的前提下测试不同 context 策略。

它的契约是不得 throw/reject;失败应返回原消息或安全 fallback。原因是它处在一次模型
调用的必经路径,若第三方 transform 随机抛错,低层 Loop 无法判断应该删历史、重试还是
终止。

Zerone 当前 `ContextProvider::contribute` 是同步、只追加。可以先增加“已构造消息视图
的 transform”而不是立刻推翻 trait:

```rust
trait ContextTransform {
    fn apply(&self, view: &mut ModelContext) -> Result<(), ContextError>;
}
```

但必须规定失败策略,不能静默返回半套上下文。

## Session 是 append-only 树,不是 messages 数组

Pi 的每个 `SessionTreeEntry` 都有:

```text
id + parentId + timestamp + typed payload
```

内置 entry 类型包括:

| 类型 | 事实 |
|---|---|
| `message` | 一条 AgentMessage |
| `thinking_level_change` | 推理等级改变 |
| `model_change` | provider/model 改变 |
| `active_tools_change` | 可用工具集改变 |
| `compaction` | 前缀摘要、保留尾部、压缩前 token |
| `branch_summary` | 离开分支后带回的摘要 |
| `custom/custom_message` | 应用扩展事实 |
| `label/session_info` | 标签与会话名称 |
| `leaf` | 将当前会话指针移动到已有节点或 root |

普通 append 的 `parentId` 指向当前 leaf,然后 leaf 前进到新 entry。`moveTo(id)` 不改旧
数据,而是 append 一个 leaf 导航记录,再把内存 leaf 指向目标;随后新增消息从该目标长出
新分支。

```text
A ─ B ─ C
     └─ D ─ E   ← current leaf
```

这使“回到 B 重新问”不需要复制或删除 A/B/C。事实日志保留完整历史,模型只沿当前
leaf 的 parent 链回溯。

## Storage、Session、Repository 三层各管什么

### SessionStorage:字节与查询原语

`SessionStorage` 只提供:

- head、entry、顺序 entries;
- append;
- 当前分支查询和回溯;
- label/name/stats 投影。

它不提供 `appendMessage`、`moveTo` 或 `buildContext`;这些是 Session 语义。

### Session:聚合对象与不变量

[session.ts](../../example/pi/packages/agent/src/harness/session/session.ts) 负责:

- 生成唯一短 entry ID;
- 给新 entry 填 parent/timestamp;
- 串行化 concurrent append,保证只形成一条 parent 链;
- 校验 label/leaf 目标存在;
- append 各类 typed entry;
- 构造当前分支与模型 context。

`appendTail` 无论前一个 append 成功还是失败都会释放后续操作,避免一次 I/O 错误让
整个会话永久卡死。

### SessionRepository:集合与生命周期

`SessionRepository` 负责:

```text
create / open / list / delete / fork / dispose
```

fork 的 selection 是显式类型:

- all;
- before 某条 user message;
- through 某个 entry。

“before user message”会校验目标真的是 User,并复制到其 parent;不会接受一个任意
assistant 节点然后制造非法开头。

## JSONL 后端展示的是并发与故障语义,不是格式偏好

Pi 教学切片提供内存和 JSONL 两个后端。JSONL 文件第一行是 version 3 header,
后续每行一个 entry。关键工程机制有:

- header 与 entry 分别严格解析,malformed 文件大声失败;
- entry ID 重复立即拒绝;
- session ID 先 URL encode 再进入文件名;
- cwd 编码为独立目录,支持按 workspace 列举;
- 打开后持有 `ArraySessionIndex`,append 不重复解析全文;
- 同一 session 操作串行,不同 session 可并发;
- 默认全局最多四个后端操作;
- list 作为 barrier,等待之前已接受的操作再取得一致快照;
- dispose 等待全部已接受操作,之后拒绝新写入。

这些机制由 `KeyedOperationQueue` 表达:每个 key 有自己的 tail,另有 barrier 和全局
permit。即使一次 operation reject,tail 也转成 resolved,后续不会被毒死。

Zerone 使用 SQLite,不需要换成 JSONL。应该迁移的是相同语义:

- 每个 session 的写序列唯一;
- list/load 与 append 的一致性明确;
- schema version 与迁移明确;
- shutdown 等待已接受事务;
- 损坏一个 session 不影响列出其他 session,但打开损坏 session 必须报清楚。

## Context 是怎样从树上构造出来的

`Session.buildContext()` 的顺序是:

1. 从 current leaf 回溯到 root 或 compaction 边界;
2. 在**原始分支 entries**上推导 thinking/model/active tools 当前状态;
3. `defaultContextEntryTransform` 选择最新 compaction 与应保留尾部;
4. 运行额外 entry transforms;
5. 将每个 entry projector 成零到多条 AgentMessage;
6. 返回 messages + 当前 thinking/model/tool state。

注意第 2 步早于压缩:模型切换等控制状态不会因为旧 entry 被 summary 替代而丢失。

### compaction entry 的两种尾部表达

Pi 支持:

- `firstKeptEntryId`:summary 后重新引用原树上的完整尾部;
- `retainedTail`:直接在 compaction entry 中保存需要回放的尾部消息。

默认 transform 找最后一个 compaction,把更老事实从**模型视图**移除,但它们仍在日志中。

必须说明:当前切片保留的是 compaction 的**数据模型与上下文重建**,没有保留负责调用
模型生成摘要、选择边界和自动触发的完整 compaction service。不能把 `appendCompaction`
误读成“Pi 在这里已经自动压缩”。

## branch summary 与 compaction summary 不同

| | Compaction | Branch summary |
|---|---|---|
| 原因 | 当前分支过长 | 用户/系统离开一条分支后回到旧节点 |
| 替代对象 | 当前分支的旧前缀 | 不再位于当前 parent 链上的探索 |
| entry 字段 | tokensBefore、tail 边界 | fromId |
| 模型文案 | history was compacted | branch came back from |

如果把两者都叫“摘要”,未来无法解释模型为什么看到这段信息,也无法统计压缩节省量。

## Context token 估算不是简单字符数

[estimate.ts](../../example/pi/packages/ai/src/utils/estimate.ts) 优先使用最近一条有效
assistant usage 作为已知前缀 token,只估算其后的尾部消息;若没有 usage 才按约四字符
一 token 估算全文。它还处理:

- system prompt 与 tool schema;
- 图片的固定估算成本;
- tool result 动态增加工具后的 schema token;
- error/aborted assistant usage 不作为可靠前缀;
- 在旧 assistant 后插入了更新 timestamp 的 compaction summary 时,旧 usage 失效。

这比每轮重新粗估所有历史稳定,但仍是估算。可靠流程应是:

```text
真实 usage 基线 + 尾部估算 → 预算预警 → compaction → 再估算 → 发请求
```

## ToolUse/ToolResult 合法性放在哪里

Pi 的 Session 本身允许保存任意 entry,不在 append 时强制检查工具配对。进入 Provider
前的 message transform 会为孤立 ToolCall 合成错误 ToolResult,并过滤 error/aborted
assistant。

Zerone 则在 Runtime 和 SQLite batch 中尽早保证合法。这是更强的不变量,建议保留:

- 事实日志可记录取消/失败细节;
- 模型消息投影仍做一次 defensive repair;
- repair 发生时发诊断事件,不能静默掩盖存储损坏。

Pi 的策略适合兼容旧 session 和第三方消息;Zerone 不应因此放松新写入的事务约束。

## 把 Pi 思路迁移到 Zerone 的最小结构

SQLite 可以自然承载 append-only tree:

```sql
CREATE TABLE entries (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    id        TEXT NOT NULL UNIQUE,
    parent_id TEXT,
    kind      TEXT NOT NULL,
    payload   TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE session_state (
    session_id TEXT PRIMARY KEY,
    leaf_id    TEXT,
    schema_version INTEGER NOT NULL
);
```

关键不是表结构,而是一次事务中:

1. 校验 parent/leaf;
2. append entry 或一批因果相关 entries;
3. 更新 leaf;
4. 更新投影统计;
5. commit 后才发持久化成功事件。

## 推荐迁移顺序

### 第一阶段:视图与事实分开,仍保持线性

1. 新增 `SessionEntry` 包装现有 `ChatMessage`;
2. SQLite 改存 entry kind + payload,旧 message 行可迁移为 `message`;
3. 新增 `build_model_messages(entries)`;
4. UI notice、command execution 可以保存但默认不进模型。

### 第二阶段:上下文管线

1. `ContextProvider` 继续贡献 system/runtime section;
2. Conversation 改为从 Session projector 取模型消息;
3. 增加 token estimator 和 `ContextTransform`;
4. 先做确定性修剪/大 ToolResult 外置,再做模型摘要。

### 第三阶段:控制状态事实化

把 model、thinking、active tools、session name 等改变写成 entry,恢复会话时由 entry
重建,不再依赖 config 当前值猜测旧会话。

### 第四阶段:树、move 和 fork

只有线性事实日志稳定后再加 `parent_id + leaf`. 先实现 move/fork 和分支查询,
最后接 branch summary 与 compaction。

## 验收测试

- 两个并发 append 形成一条确定 parent 链;
- append 失败后下一条仍可写;
- 切模型/工具后恢复 Session 得到当时状态;
- UI-only entry 保存但不进入 Provider payload;
- compaction 后事实条数不减少,模型消息数减少;
- compaction tail 不切断 ToolUse/ToolResult;
- move 到旧节点后原分支仍可查询;
- fork before user 与 through entry 边界正确;
- malformed/duplicate/cycle/missing parent 明确失败;
- list 与 shutdown 对并发写有确定语义;
- 旧 schema 可迁移,失败不会半迁移;
- token 预算使用真实 usage + 尾部估算,而不是只数字符。

Context 工程化的最终目标不是“塞得更多”,而是任何时刻都能解释:
**这条事实为什么被保存、为什么被显示、为什么在本轮进入模型、又为什么可能被摘要
替代。**


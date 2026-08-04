# 03 · Pi 的工具系统:从函数注册表到执行平台

> 对应 Zerone 基线:[03 · 工具系统](../03-tool-system.md)。
> Pi 重点源码:`packages/agent/src/types.ts`、`packages/agent/src/harness/types.ts`、
> `harness/tools/`、`harness/env/nodejs.ts`、`harness/utils/`。

## Zerone 当前实现

Zerone 的工具系统有一个很好的最小骨架:

```rust
trait Tool {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    fn execute(&self, args: &Value, ws: &Workspace, cancel: &AtomicBool)
        -> Result<String, String>;
}
```

`ToolRegistry` 负责按名分发、把错误变成 Observation、清洗控制字符并统一截断;
`Workspace` 集中处理路径与文件原语。五个内置工具已经覆盖读、列目录、写、精确编辑和
命令执行。

它的工程化瓶颈不是“工具太少”,而是契约表达能力不足:

- 返回值只有字符串,模型内容、UI 展示、diff、附件和审计信息混在一起;
- schema 是未验证的 `serde_json::Value`,错误通常到 execute 内才暴露;
- 工具环境固定为 `Workspace + cancel`,扩展服务只能不断加参数;
- Registry 统一做无损恢复能力很弱的 24K 中间截断;
- 工具只能结束时返回一次结果;
- 未来并行后,两个写工具可能同时修改同一文件。

## Pi 先区分“模型工具”和“Harness 工具”

### AgentTool:Loop 真正认识的契约

[AgentTool](../../example/pi/packages/agent/src/types.ts) 在 AI 层 `Tool` 的 name、
description、parameters 之上增加:

```ts
interface AgentTool {
  label: string
  prepareArguments?(raw): ValidatedShape
  execute(id, params, signal, onUpdate): Promise<AgentToolResult>
  executionMode?: "sequential" | "parallel"
}
```

Loop 只依赖这个契约。它知道参数要校验、执行可以更新进度、工具可能要求顺序执行,
但不知道文件系统来自 Node、远程沙箱还是测试 fake。

### AgentHarnessTool:给工具注入执行环境

[harness/types.ts](../../example/pi/packages/agent/src/harness/types.ts) 再定义
`AgentHarnessTool<TContext>`:在 execute 尾部增加由 harness 提供的 context。
内置文件和 shell 工具只需要:

```ts
interface ExecutionToolContext {
  env: ExecutionEnv
}
```

环境装配发生在 Loop 之外。这样工具不会抓全局 cwd,不会直接 import Node 文件系统,
也不需要认识 Agent transcript。

对 Zerone 的直接启示是:保留 `Tool` 作为 Loop 契约,但把执行参数收拢成显式上下文:

```rust
struct ToolContext<'a> {
    workspace: &'a dyn FileSystem,
    shell: &'a dyn Shell,
    cancel: &'a CancellationToken,
    session_id: &'a SessionId,
}
```

不要把整个 `Agent` 或 `Config` 传给工具。

## 参数先兼容,再校验,最后审批

Pi 的工具调用顺序是:

```text
prepareArguments → validateToolArguments → beforeToolCall → execute
```

`prepareArguments` 是受控兼容层。例如 `edit` 现在接收 `edits[]`,但可以把旧调用的
`oldText/newText` 转成新数组;某些模型把数组错误地输出成 JSON 字符串时,也可以先
解析。转换后的对象再过 TypeBox schema 校验。

这个顺序避免两种常见坏设计:

- 为兼容旧模型把 schema 永久放宽成 `any`;
- 权限 Hook 在原始、不可信参数上判断路径,执行时却使用另一组解析结果。

Pi 当前的一个明确取舍是:`beforeToolCall` 收到的 args 可以被调用方对象引用修改,
修改后不再次校验,对应测试也固定了这一行为。Zerone 不必照搬;更稳的 Rust 设计是
让 hook 返回 `Allow(validated_args) | Deny(reason)`,若允许改参则显式再校验一次。

## ToolResult 不再是一根字符串

Pi 的 `AgentToolResult<T>` 至少包含:

| 字段 | 消费者 | 作用 |
|---|---|---|
| `content` | 模型 | 文本或图片 Observation |
| `details` | UI/日志/扩展 | diff、截断元数据、完整输出路径等 |
| `usage` | 统计 | 工具自身使用的资源,不混入主 LLM context |
| `addedToolNames` | Provider/context | 延迟加载后新增的工具 |
| `terminate` | Loop | 当前批是否建议结束自动续跑 |

`content` 支持图片,所以 `read` 遇到 PNG/JPEG/GIF/WebP 会返回一段文字加一个 base64
image block;BMP 可交给注入的 image processor 转换。模型内容和 UI details 不再竞争
同一个截断字符串。

Zerone 最值得优先迁移的类型是:

```rust
struct ToolOutput {
    model_content: Vec<ContentBlock>,
    details: Option<serde_json::Value>,
    is_error: bool,
    artifacts: Vec<ArtifactRef>,
}
```

`ToolCallFinished` 直接携带 UI summary/details,Provider 只编码 model content。

## Expected failure 有类型,工具失败仍是 Observation

Pi 在 adapter 层使用 `Result<T, FileError/ExecutionError/SessionError>`。错误有稳定 code:

- 文件:`not_found`、`permission_denied`、`not_directory`、`aborted` 等;
- 执行:`timeout`、`shell_unavailable`、`spawn_error`、`callback_error` 等;
- Session:`invalid_entry`、`invalid_fork_target`、`storage` 等。

内置工具用 `getOrThrow` 把 expected failure 提升为异常;Agent Loop 在 execute 边界捕获,
转成 `isError=true` 的 ToolResult。也就是说:

```text
环境 adapter:错误是类型化值
工具实现:失败可抛出,保持主路径清楚
Loop 边界:任何工具失败都是模型可见 Observation
```

Zerone 当前 `Result<String, String>` 已经保证 Loop 不崩,但错误 code 丢失后,权限、UI、
重试与指标只能解析文案。迁移时应保留“错误回给模型”的语义,同时把内部错误升级为
结构化 `ToolError { code, message, retryable, details }`。

## ExecutionEnv 是能力边界,不是安全策略

[ExecutionEnv](../../example/pi/packages/agent/src/harness/types.ts) 组合了完整
`FileSystem + Shell`:

- 文本/二进制/按行读取;
- 写、追加、目录、删除、临时文件;
- absolute/canonical/join/exists;
- shell exec、环境变量、cwd、timeout 和流式 stdout/stderr;
- `cleanup()` 回收临时资源和活动子进程。

Node 实现把 OS 异常映射成类型化错误,在 Unix 用独立进程组、Windows 用
`taskkill /T /F` 终止进程树;进程退出后还给 stdio 100ms grace,避免子孙进程持有
管道时永远等不到 close。

但必须看清:`absolutePath()` 接受绝对路径,`~` 和 `file://`;Pi 的 ExecutionEnv 是
**可替换能力接口**,不是 workspace confinement。Zerone 也不能因为所有工具都经过
`Workspace` 就宣称已经沙箱化。真实权限仍需额外处理:

- canonical path 与不存在目标的父目录;
- symlink/junction;
- workspace 外路径审批;
- shell 可以绕过文件 API;
- 审计与 deny 规则。

## 文件工具的工程细节

### read:头部保留 + 可继续读取

Pi 的 `read` 同时限制 2000 行和 50KB,保留文件头部。若截断,结果明确给出:

- 展示的行号范围;
- 总行数;
- 下一次 `offset`;
- 是行数限制还是字节限制。

第一行本身超过 50KB 时,它不返回半行,而是提示用 shell 精确截取。这比中间截断更
适合代码阅读:模型可以确定性地翻页,不会误以为头尾本来相邻。

### edit:一把锁里完成 read-modify-write

Pi 的 `edit` 支持一次提交多个精确 replacement,并要求:

- 每个 `oldText` 在**原文件**中唯一;
- 多个 replacement 不重叠;
- 统一在 LF 域匹配,写回恢复原换行;
- 保留 UTF BOM;
- 返回 diff、unified patch 和首个改动行作为 details。

一次调用内的多个编辑一起验证再写入,避免“前一替换改变后一匹配”的顺序歧义。
Zerone 当前单 replacement 更容易理解;可先迁移 diff details 和原子 read-modify-write,
再决定是否需要 multi-edit。

### 同一路径写操作必须串行

[file-mutation-queue.ts](../../example/pi/packages/agent/src/harness/tools/file-mutation-queue.ts)
按 `ExecutionEnv + canonical path` 建 Promise tail。`write` 和 `edit` 都把完整操作包在
`withFileMutationQueue` 中:

```text
resolve key → 等前一 mutation → read/validate/write → finally release
```

文件已存在时用 canonical path,不存在或环境不支持 canonical 时退回 absolute path。
队列用 WeakMap 绑定 env 生命周期,空队列自动删除。

这不是只读/写调度元数据的替代品,而是第二道资源锁。即使 Loop 判断两个工具可以
并发,同一文件的实际修改仍不会交错。

## bash:进度、尾部和完整输出三者同时保留

Pi 的 `bash` 与 Zerone `run_command` 的差异最大:

1. stdout/stderr chunk 实时进入 capture;
2. 每 100ms 最多发一次 `tool_execution_update`,避免 UI 被刷爆;
3. 模型结果保留最后 2000 行/50KB,因为错误通常在尾部;
4. 一旦超限,完整输出开始写临时日志;
5. 最终 details 携带 truncation 与 `fullOutputPath`;
6. 非零退出、timeout、abort 都保留已捕获输出再转成错误;
7. tool settle 后的迟到 update 被 Loop 忽略。

这里有两个不应照搬的选择:

- Pi 的 bash 默认**无 timeout**,Zerone 默认 120 秒更适合教学和本地安全;
- 当前 Node `env.exec` 自身仍累加完整 stdout/stderr 字符串,尽管上层 capture 只保留
  有界 tail。真正移植时应让底层也支持“不累计全文”的 streaming 模式,否则超大输出
  仍会占满内存。

## 输出策略为什么必须由工具决定

Pi 的截断工具区分:

| 场景 | 策略 | 恢复方式 |
|---|---|---|
| 读文件 | 保留头部完整行 | `offset` 翻页 |
| shell | 保留尾部 | 完整日志 artifact |
| diff/details | 与模型文本分离 | UI 展开 details |
| 图片 | 二进制 content block | 模型直接消费 |

这比 Registry 对所有字符串统一“保头保尾 24K”可靠。统一兜底仍可存在,但工具必须在
触发兜底前提供可恢复引用;否则截断就是数据丢失。

## Pi 的工具测试在验证什么

[tools.test.ts](../../example/pi/packages/agent/test/harness/tools.test.ts) 和
[nodejs-env.test.ts](../../example/pi/packages/agent/test/harness/nodejs-env.test.ts)
覆盖的不只是 happy path:

- read 的 offset/limit、图像、超长首行和 Unicode;
- edit 的唯一匹配、重叠、BOM、CRLF 与 diff;
- 相同文件 mutation 串行;
- bash 的 chunk update、截断日志、非零退出、timeout 与 abort;
- 工具结束后的迟到 update 不污染状态;
- shell 不存在、cwd 不存在、callback 抛错;
- cleanup 终止仍活动的子进程。

工程化工具的正确性由“失败时留下什么”决定,不是仅由成功输出决定。

## Zerone 推荐迁移顺序

1. `ToolOutput/ToolError` 类型化,先不改现有工具行为;
2. `AgentEvent::ToolCallUpdated`,给命令工具接实时输出;
3. read 改为头部分页,bash 改为尾部 + artifact;
4. edit 返回 diff details,持久内容仍由 Workspace 写;
5. 引入 `ToolContext`,把 Workspace、Shell、cancel 显式分开;
6. 工具参数在 Registry 统一 schema 校验;
7. 添加 `execution_mode`,再做只读并行;
8. 给 edit/write 加 canonical-path mutation queue;
9. 最后加入权限 policy,不要用工具名字硬编码副作用。

每一步都应保持:Runtime 不出现具体工具名、Provider 不解析 details、TUI 不直接调用
工具实现、工具失败仍然形成合法 ToolResult。

## 工具系统验收

- 模型内容、UI details 和大结果引用不再共用一个字符串;
- schema 在 before hook 与 execute 之前统一校验;
- File/Shell expected failure 有稳定 code,同时仍是模型可见 Observation;
- read、bash、列表型工具分别采用可恢复的输出策略;
- 工具进度受节流控制,settle 后的 update 不再生效;
- edit/write 的完整 read-modify-write 按 canonical path 串行;
- 取消、timeout、非零退出都保留已产生的有用输出;
- ExecutionEnv 可替换,权限策略另有明确边界;
- 并行完成顺序不会改变 ToolResult 的历史顺序;
- cleanup/shutdown 后没有遗留子进程、锁或无法继续的队列。

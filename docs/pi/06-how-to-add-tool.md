# 06 · 实战:按 Pi 的标准给 Zerone 添加工具

> 对应 Zerone 基线:[06 · 添加一个新工具](../06-how-to-add-tool.md)。
> 本篇不重复“实现 trait、注册两行”,而是把一个教学工具提升为可长期维护的能力。

## Zerone 当前接入路径

Zerone 新增工具只需实现 `Tool` 的 name、description、schema、execute,再加入
`default_registry()`. Runtime、三种 Provider 和 TUI 都不需要新增分支。文件访问经
`Workspace`,Registry 统一把 `Err(String)` 变成 Observation,并负责清洗和截断。

这是正确的最小扩展边界。本篇不是替换它,而是补齐 schema 真校验、类型化结果、
环境注入、进度、并发声明和失败测试,让“注册容易”不会以“运行时无法治理”为代价。

## 目标例子:仍然是 grep,但验收标准变了

现有 06 用递归文本搜索 `grep` 展示 Zerone 的最小扩展路径。这段实现作为教学代码
没有问题,但若准备长期使用,还需要回答:

- 大仓库如何取消,多久检查一次;
- binary、权限失败、symlink、Unicode 路径怎样处理;
- 一行 2MB 或十万条命中怎样返回;
- 搜索结果是字符串还是带分页元数据的结构;
- 未来多个只读工具并行时是否安全;
- 文件系统换成远程 sandbox 时是否还能复用;
- schema 由谁真正验证;
- 失败是否能被 UI、指标和模型分别理解。

Pi 的价值不是提供一个更花哨的 grep,而是给这些问题稳定的落点。

## 先理解 Pi 中“添加工具”发生在哪里

当前 Pi 切片没有全局 `default_registry()`. 内置工具采用 factory:

```text
createReadTool(options?)
createWriteTool()
createEditTool()
createBashTool(options?)
```

它们从 `harness/tools/index.ts` 导出,由宿主绑定 `ExecutionToolContext`,再放入
`AgentState.tools`. Loop 只认识 `AgentTool`;Provider 只读取其中 name、description、
parameters 等模型声明。

因此一个新工具有四个接入面,不是一个文件:

```text
模型声明       AgentTool
执行依赖       TContext / ExecutionEnv
运行时装配     AgentState.tools
观测与策略     Agent events + before/after hook
```

Zerone 可以继续使用 Registry,但应把“注册实例”升级为“用依赖创建实例”,避免工具自己
寻找全局服务。

## 第 0 步:写工具设计卡

在写 schema 前填写:

| 问题 | grep 的答案 |
|---|---|
| 使用时机 | 不知道精确文件时搜索符号/文本 |
| 不使用时机 | 已知文件路径时用 read_file |
| 副作用 | 只读 |
| 环境能力 | list/read/canonical,不需要 shell |
| 执行模式 | parallel safe,但需要全局并发上限 |
| 空结果 | 成功,返回 0 hits |
| 可恢复输出 | cursor/offset 或“范围 + more” |
| 取消点 | 每个目录、每个文件、每 N 行 |
| 预期错误 | root 不存在/不是目录/权限拒绝 |
| 非预期错误 | 内部状态、编码器或 invariant 失败 |
| UI details | scannedFiles、matchedLines、truncated |

这张卡决定 schema、`ToolCapabilities`、输出类型和测试。没有它,实现再短也只是把问题
推迟到 Runtime。

## 第 1 步:先升级公共契约

不要为了一个新工具在 `run_turn` 写特殊分支。先让公共类型能表达需要的信息:

```rust
pub enum ToolExecutionMode {
    Sequential,
    ParallelSafe,
}

pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    pub execution_mode: ToolExecutionMode,
}

pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<Value>,
    pub artifacts: Vec<ArtifactRef>,
}

pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
    pub retryable: bool,
    pub details: Option<Value>,
}
```

Pi 用 `AgentToolResult.content/details/usage/addedToolNames/terminate` 表达相同分离。
Zerone 第一版不需要全部字段,但 `model content` 与 `runtime details` 必须先拆开。

## 第 2 步:让 schema 变窄,并在 Registry 真正校验

工程化 grep 的 schema 可为:

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "pattern": { "type": "string", "minLength": 1 },
    "path": { "type": "string", "default": "." },
    "max_results": { "type": "integer", "minimum": 1, "maximum": 1000 },
    "case_sensitive": { "type": "boolean", "default": true }
  },
  "required": ["pattern"]
}
```

关键不是 schema 看起来完整,而是调用前真的校验。Pi 用 TypeBox schema 和
`validateToolArguments` 在 before hook 之前完成。Zerone 当前的 `require_str` 只能覆盖
execute 已读取的字段,无法拒绝额外字段、错误整数或跨字段约束。

建议 Registry 统一返回:

```text
invalid_arguments:
  max_results must be an integer between 1 and 1000;
  received -3
```

这仍是模型可行动的 ToolResult,不是 Runtime fatal error。

### 兼容旧参数要放在窄入口

Pi 的 `prepareArguments` 先把旧形状转为新形状,再校验。Zerone 若将来把 `pattern`
改成 `query`,可提供版本化 normalizer,但不要让 execute 同时永久接受五种松散形状。

```text
raw args → compatibility normalizer → schema validation → policy → execute
```

## 第 3 步:工具依赖能力接口,不依赖具体 Workspace

grep 只需要只读文件能力:

```rust
trait SearchFileSystem {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileError>;
    fn read_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError>;
    fn read_text(&self, path: &Path) -> Result<String, FileError>;
}

struct ToolContext<'a> {
    fs: &'a dyn SearchFileSystem,
    cancel: &'a CancellationToken,
    workspace: &'a WorkspacePolicy,
}
```

实际工程中不必每个工具新建 trait;可以由 `ExecutionEnv` 提供较完整原语。但权限要在
能力调用前后有统一 policy,工具不应直接 `std::fs`。

Pi 的 `ExecutionEnv` 同时提供 FileSystem/Shell,Node 实现只是一个 adapter。测试可换
in-memory/fake env,未来 remote sandbox 也不用改 grep。

## 第 4 步:实现时区分四类结果

### 1. 成功且有命中

模型内容返回稳定、紧凑的:

```text
src/runtime.rs:42: fn run_turn(...)
src/tools/mod.rs:17: pub trait Tool ...

[showing 2 of 2 matches]
```

details 返回:

```json
{
  "scanned_files": 37,
  "matched_lines": 2,
  "truncated": false,
  "next_cursor": null
}
```

### 2. 成功但无命中

`Ok(ToolOutput)` 且 `matched_lines=0`. 无匹配是领域结果,不是异常,模型下一步可能扩大
范围或换关键字。

### 3. 预期失败

root 不存在、不是目录或权限拒绝返回类型化 `ToolError`. 文案给模型下一步,code 给
UI/指标稳定分类:

```text
not_directory: path "src/main.rs" is a file; search its parent "src" or use read_file
```

### 4. 取消

取消不是“无匹配”,也不应丢掉执行到一半的事实。返回 `aborted` error,details 可带已
扫描文件数;Loop 仍生成唯一 ToolResult,维护 ToolUse 配对。

## 第 5 步:设计可恢复输出,不要依赖 Registry 兜底

Pi 的 read 用 head + offset,bash 用 tail + full log。grep 属于列表查询,更适合:

```text
max_results + next_cursor
```

第一版没有稳定 cursor 时,至少返回:

- 实际上限;
- 是否还有更多;
- 建议缩小 path/pattern;
- 每条预览的单行长度上限。

不要先生成 10MB 字符串再让 Registry 从中间截掉。扫描过程达到预算就停止,这样 CPU、
内存和 context 同时受控。

若需要完整结果,将其外置为 artifact 并返回 opaque ID;不要把任意绝对路径暴露为
artifact handle。

## 第 6 步:定义并发与资源锁

grep 声明 `ParallelSafe` 只表示它没有写副作用,不表示可以无限开:

- Runtime 有全局工具并发上限;
- 同一个大 workspace 的多个搜索可共享扫描预算;
- cancel 作用于整个 batch;
- 结果仍按 ToolUse 源顺序写入历史。

若工具是 edit/write,还必须像 Pi 的 `withFileMutationQueue` 一样按 canonical path
串行化完整 read-modify-write。`execution_mode=Sequential` 是调度提示,资源锁才是最后
防线。

## 第 7 步:注册与装配

Zerone 当前两行注册可保留,但推荐从“无参数 struct”升级为 factory:

```rust
registry.register(Box::new(GrepTool::new(GrepOptions {
    default_limit: 200,
    ignored_dirs: workspace_ignores.clone(),
})));
```

环境和 Session 级依赖由 Runtime 形成 `ToolContext` 后在每次 execute 注入。不要把
`Workspace`、当前 session 或 EventSender 存进全局 singleton。

Provider 和 TUI 仍不需要知道 grep:

- Provider 自动导出公共 `ToolSpec`;
- TUI 消费通用 start/update/end 和 details;
- 权限 Hook 读取 spec capability 与已验证 args;
- Session 保存通用 ToolUse/ToolResult。

若任何一层出现 `if tool_name == "grep"`,说明公共契约还缺字段。

## 第 8 步:增加 Hook 可观测性

Pi 的 before/after tool hook 给 Zerone 三个直接用途:

```text
before:权限、审计开始、参数脱敏、预算检查
after :输出脱敏、artifact 外置、指标、策略性 terminate
event :UI 进度,不修改执行结果
```

Hook 得到已验证 args;after 得到结构化 result。Hook 业务拒绝形成 error ToolResult;
Hook 自身编程错误则终止 run 或按显式 policy 降级,不能吞掉后继续假装成功。

## 测试矩阵

### 工具单测

- 一个/多个/零匹配;
- 大小写与 Unicode;
- max_results 边界和非法 schema;
- 空 pattern;
- binary/非 UTF-8/超大单行;
- 目录权限失败与文件中途消失;
- symlink 与 workspace policy;
- 扫描前、扫描中、达到上限瞬间取消;
- details 统计与 model content 一致。

### 环境契约测试

同一组 grep 测试至少跑 fake env 与本地 env,证明工具未依赖 Node/Rust 具体文件 API。

### Loop 测试

- bad args 不调用 execute;
- before deny 不调用 execute;
- error 仍形成 ToolResult;
- update 在 end 后被忽略;
- 多个只读工具完成顺序不改变历史顺序;
- abort 后所有 ToolUse 有配对结果。

### Provider wire 测试

不需要为 grep 写三份 adapter 分支。只验证公共 ToolSpec 在 Messages、Chat、Responses
各自的声明形状正确,ToolResult ID 配对正确。

## 完成定义

一个工具不是“模型成功调用过一次”就完成。它至少满足:

- [ ] schema 在执行前被统一校验;
- [ ] description 写清使用与不使用边界;
- [ ] 依赖显式 ExecutionEnv/ToolContext;
- [ ] expected error 有稳定 code 和可行动文案;
- [ ] 取消有检查点且形成结果;
- [ ] 输出在生成时受预算控制,截断可恢复;
- [ ] model content 与 UI details 分离;
- [ ] 并发声明与真实资源锁一致;
- [ ] Hook、事件、Session 不需要认识工具名字;
- [ ] 单测覆盖失败路径,Loop/Wire 测试覆盖公共协议。

Pi 带来的工程化标准可以浓缩成一句话:新增工具不是给模型多一个函数,而是给系统
增加一种受校验、可取消、可观测、可恢复的副作用。

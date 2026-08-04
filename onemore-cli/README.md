# Onemore

Onemore 是从 Zerone 教学基线迁移出的独立 coding agent 工程,并已按
[project.md](project.md) 的路线完成 1-6 阶段的工程化改造:Provider 终止协议、
类型化工具管线、权限与 Hook、事实日志与上下文预算、steering/follow-up 队列、
受控并发与资源锁。Zerone 的目标是"一条数据流容易读懂";Onemore 在保留这条
数据流的前提下,把目标换成了"每个中断点都有确定的历史、事件、队列和资源状态"。

## 运行

```powershell
cargo run
cargo run -- --once "你好"
cargo run -- -p ds-chat
```

首次运行会生成 `~/.onemore/config.toml`. 也可以设置 `ONEMORE_HOME` 将配置和会话
放到独立目录:

```powershell
$env:ONEMORE_HOME = "D:\onemore-data"
cargo run
```

配置样例见 `config.example.toml`. 本地 `config.toml` 可能包含 API key,已被 Git 忽略。

TUI 内常用操作:

| 操作 | 说明 |
|---|---|
| 运行中输入并回车 | steering:在当前一批工具全部完成并提交后注入,修正方向 |
| `/queue <内容>` | follow-up:排队后续任务,当前任务将停止时才注入 |
| `/compact` | 调模型生成摘要作为 Compaction 事实;模型视图缩小,事实日志不减少 |
| `Esc` | 取消当前轮(丢弃半截流式输出;未执行的工具调用补取消结果;清空排队输入) |
| `/session [ID]` | 列出/恢复会话,恢复时重建全部事实(含 UI-only 提示) |
| `/provider` `/model` | 热切换,写入 ModelChange 事实,历史保留 |

## 与 Zerone 的区别

Zerone 是刻意压低复杂度的可运行基线;Onemore 在同一架构骨架上补齐了工程化契约。
未变的部分:统一消息模型 `ChatMessage/Block`、三种 API(Messages / Chat
Completions / Responses)适配边界、`AgentCommand/AgentEvent` 事件流与双前端
(TUI + `--once`)、工具必须经 `Workspace` 访问文件、一会话一 SQLite 库。

### 1. Provider:从"可能静默成功"到终止完备协议(阶段 1)

- Zerone:`stream_turn` 返回 `Result<Option<TurnOutput>>`,取消是 `Ok(None)`,
  EOF 后可能把半截流当正常回答。
- Onemore:每次调用必然终止于 `StreamTerminal::{Done, Error, Aborted}` 之一;
  EOF 而无终止事件一律是错误;失败路径也携带可消费的 final assistant
  (`FailedTurn`)。三种适配器都有 EOF 断流 wire 测试锁定该行为。
- 重试收敛为 `RetryPolicy` 纯函数:指数退避 + 确定性 jitter + 上限,
  解析 `retry-after-ms`/`retry-after`,服务器要求等待超过 60s 直接放弃;
  "只有未产生任何流事件的失败才重试"的幂等前提不变。

### 2. 工具:从字符串到类型化管线(阶段 2)

- Zerone:`execute() -> Result<String, String>`,Registry 统一 24K 中间截断,
  模型正文、UI 展示、诊断混在一根字符串里。
- Onemore:`ToolOutput { model_text, ui_summary, details }` 与
  `ToolError { code, retryable, details }` 分离;稳定错误码(`not_found` /
  `conflict` / `timeout` / `permission_denied` …)供 UI、指标与策略消费,
  工具失败仍是模型可见 Observation。
- 参数走 `prepare_arguments(兼容转换) → JSON Schema 校验 → 执行` 管线,
  校验失败一定不会到达 execute;`length` 截断的 assistant 里所有工具调用
  一律不执行(截断参数可能"语法合法但语义不完整")。
- 工具可上报结构化进度(`ToolCallUpdated`);settle 之后的迟到进度被忽略。

### 3. 权限与 Hook:副作用有了安全门(阶段 3)

- Zerone:没有权限层,`Workspace` 允许任意绝对路径。
- Onemore:`PermissionManager` 按 `workspace_read / workspace_write /
  outside_workspace / commands` 四条规则(allow | ask | deny)评估**已校验参数**;
  设备路径等 hard deny 不可被任何配置或 Hook 覆盖;审批走独立通道,
  支持 Once / Session 两种作用域,等待审批期间可取消。
- 四个 Hook 扩展点(UserPromptSubmit / PreToolUse / PostToolUse / Stop)。
  hard deny 先于 Hook 运行;Hook 改写参数后会重新 preflight 并重新过权限,
  因此 Hook 无法绕过安全策略。

### 4. 会话与上下文:事实日志 ≠ 模型视图(阶段 4)

- Zerone:屏幕历史 = 运行历史 = 持久历史 = 模型上下文,四者是同一个
  `Vec<ChatMessage>` 全量发送;SQLite 只存最终模型消息。
- Onemore:持久层是 append-only 的事实日志(schema v2):
  `SessionEntry { id, parent_id, kind, payload }`,payload 分
  `Message(含该次真实 usage) / Notice / Compaction / ModelChange / Artifact`。
  entry、链尾(leaf)与统计在同一事务提交;带 ToolUse 的消息批在提交边界
  被强制配对完整,提交失败则内存镜像不推进、本轮立即终止——内存与磁盘
  永不分叉。旧版线性库在打开时单事务自动迁移,失败回滚保留原库。
- 模型看到什么由**单向投影**决定:UI-only 事实不进 Provider;投影时对旧库
  损坏数据做防御性工具配对修复并发出诊断。`/session` 恢复的是完整事实。
- 上下文预算(配置 `context_window` 后启用):优先用最近一条 assistant 的
  真实 usage 作基线、只估算其后尾部;超预算先在本轮视图中折叠旧 ToolResult
  (事实不动、配对不拆),仍超预算则**明确拒绝发请求**并提示 `/compact`,
  绝不静默删消息。`/compact` 生成的摘要是新增事实,压缩后事实条数只增不减。

### 5. 运行时:ActiveRun 与两个输入队列(阶段 5)

- Zerone:输入只能阻塞等待,turn 进行中的命令靠 mpsc 排队时机隐式决定。
- Onemore:一次运行是一个显式 ActiveRun。运行期间到达的输入在检查点被
  显式分类:steering 只在**完整工具批提交后**注入(不打断执行中的工具,
  紧急停止走 Esc);follow-up 只在**当前任务将停止时**注入;两者都是
  one-at-a-time。`/clear`、`/provider` 等命令延迟到本轮结束执行;
  运行中收到退出请求会先取消当前轮再退出;取消清空全部排队输入并提示。

### 6. 受控并发与资源锁(阶段 6)

- Zerone:工具严格串行。
- Onemore:preflight(校验/权限/审批)按源顺序;全批都是 ParallelSafe 且
  多于一个才并发(上限 4),任一 Sequential 工具使整批退回串行。
  `ToolCallFinished` 按完成顺序发出(UI 及时),历史 ToolResult 始终按
  ToolUse 源顺序写入(相同输入产生相同 prompt)。取消传播到每个调用的
  组合标志,未启动的调用直接补取消结果——每个 ToolUse 无论如何都有配对。
- 第二道资源锁:`write_file`/`edit_file` 的完整 read-modify-write 在同
  canonical path 的 mutation 锁内进行,即使调度层允许并发也不会交错。
- 可配置单工具超时(`[agent] tool_timeout_secs`):逾期置组合取消标志,
  因此中止的结果报 `timeout`;工具无视标志坚持完成的保留真实结果。

### 尚未实现(对应路线阶段 7-8)

Todo/Skills/Task 系统、Background 命令、子代理、MCP、树形会话的 move/fork、
自动触发的 compaction(当前需手动 `/compact`)。

## 配置增量

相对 Zerone 新增的配置项:

```toml
[agent]
tool_timeout_secs = 300        # 可选:单工具执行超时,默认不限制

[permissions]                  # allow | ask | deny
workspace_read = "allow"
workspace_write = "allow"
outside_workspace = "ask"
commands = "ask"

[providers.xxx]
context_window = 200000        # 可选:配置后启用上下文预算与 /compact 提示
```

## 存储

```text
~/.onemore/
  config.toml
  sessions/
    <session-id>.db            # schema v2:entries 事实日志 + session 元数据
```

Onemore 不读取 `~/.zerone`,也不识别 `ZERONE_HOME`,因此两个程序的配置、密钥和会话
互不污染。每个会话仍使用独立 SQLite 数据库,并按 workspace 隔离;v1(线性
messages 表)数据库在打开时自动迁移为 v2,迁移失败回滚、原库保持可用。

## npm 包

默认 npm 包名是 `onemore-agent`,安装后命令是 `onemore`:

```powershell
.\scripts\package-npm.ps1 -Pack
npm install --global .\dist\npm\onemore-agent-0.1.0.tgz
onemore --help
```

本地打包只包含当前平台二进制。跨平台组包可通过 `-ArtifactsDir` 提供对应产物。

## 验证

```powershell
cargo fmt --check
cargo test --locked          # 112 单测 + 6 wire 测试
cargo build --release --locked
.\scripts\package-npm.ps1 -Pack
```

## License

MIT

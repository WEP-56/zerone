# Zerone（zero to one）

这是一个极简但结构完整的**基底 Agent**(Rust)。在4300行左右代码下打造了一个非生产级的类 Claude Code / Codex / Pi 等coding agent 的最小可运行雏形:一个正确的 Agent Loop + 五个工具 + 三种 LLM 接口的流式适配 + 一个简约 TUI,没有其他。

这个仓库其实更适合：在此项目的基础上，扩展其harness工程，制作一个属于自己的可靠的agent。而不是从零构建agent的教程，如果需要的话，请看：[abstract](abstract.md) 内推荐的教程

```
TUI / --once ◄──AgentEvent──┐
     └──AgentCommand──► Runtime(Agent Loop)
                          ├─ context/   每轮"模型看到什么"(instructions + 环境 + 历史)
                          ├─ tools/     read_file · list_dir · write_file · edit_file · run_command
                          └─ provider/  Messages(Anthropic) │ Chat Completions │ Responses(OpenAI)
                                        —— 内部只有一套消息模型,三家只是编解码方言
```

## 快速开始

```bash
cargo run                  # 首次运行生成 ~/.zerone/config.toml 模板
# 编辑全局 config.toml 选择 provider,配好 API key(或设对应环境变量)
cargo run                  # 进入 TUI
cargo run -- --once 你好    # 无界面跑一轮,最快的连通性测试
cargo run -- -p deepseek   # 临时切换 provider
```

代理走 `HTTPS_PROXY`/`HTTP_PROXY` 环境变量;`base_url` 可指向任意兼容服务(中转、ollama、vLLM)。

## 按键与命令

| 按键 | 作用 | 命令 | 作用 |
|---|---|---|---|
| Enter | 发送 | `/provider [名]` | 列出/热切换 provider(历史保留) |
| 行尾 `\` + Enter | 换行(Shift+Enter 仅部分终端;多行粘贴自动识别) | `/model 名` | 改模型 |
| Esc | 取消当前轮 / 回底部 / 清输入 | `/session [ID]` | 列出/恢复当前 workspace 的会话 |
| Ctrl+L | 强制重绘 | `/clear` | 清空当前会话 |
| PgUp·PgDn / ↑·↓ | 滚动 / 输入历史 | `/help` `/quit` | 帮助 / 退出 |
| Ctrl+C ×2 | 退出(任何时候有效) | | |

## 结构与文档

```
src/message.rs   统一消息模型     src/provider/  三接口流式适配 + SSE
src/workspace.rs 文件访问唯一入口  src/event.rs   Runtime↔前端唯一契约
src/tools/       工具系统         src/runtime.rs Agent Loop 本体
src/storage.rs   全局路径 + SQLite 会话存储
src/context/     上下文组装       src/tui/       事件流的一个消费者
tests/wire.rs    三接口 mock 集成测试(不碰真实网络)
```

- 每个源文件头部有设计动机注释;扩展前请读 [docs/](docs):01 架构 → 02 循环 → 03 工具 → 04 提供商 → 05 上下文 → 06 加工具 → 07 加提供商。
- 项目摘要与学习推荐见 [abstract.md](abstract.md)。

## 刻意的取舍——为了更好扩展性

零 unsafe、无异步运行时(阻塞 IO + 两线程);`run_command` 不保留 `cd` 状态(有 `cwd` 参数);无权限/审批/记忆/MCP/子代理——它们是留给使用者的扩展作业,各篇文档结尾有挂载点分析。

配置和会话位于用户目录的 `.zerone/`。每个会话保存为 `sessions/<uuid>.db`，
并记录其 workspace；在同一路径启动 Zerone 后用 `/session` 查找，再用
`/session ID` 恢复。可设置 `ZERONE_HOME` 覆盖数据目录，便于测试或便携安装。

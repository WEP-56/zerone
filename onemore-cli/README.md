# Onemore

Onemore 是从 Zerone 可运行基线迁移出的独立 coding agent 工程。它保留当前已经验证的
Agent Loop、五个内置工具、三种 LLM API 适配、SQLite 会话和 TUI,后续将在这个工程
内按 Pi 的工程实践逐步升级可靠性与实用性。

当前阶段只完成项目隔离与命名迁移,尚未声称已经具备 Pi 的完整工程能力。

## 运行

```powershell
cargo run
cargo run -- --once "你好"
cargo run -- -p deepseek
```

首次运行会生成 `~/.onemore/config.toml`. 也可以设置 `ONEMORE_HOME` 将配置和会话
放到独立目录:

```powershell
$env:ONEMORE_HOME = "D:\onemore-data"
cargo run
```

配置样例见 `config.example.toml`. 本地 `config.toml` 可能包含 API key,已被 Git 忽略。

## 存储

```text
~/.onemore/
  config.toml
  sessions/
    <session-id>.db
```

Onemore 不读取 `~/.zerone`,也不识别 `ZERONE_HOME`,因此两个程序的配置、密钥和会话
互不污染。每个会话仍使用独立 SQLite 数据库,并按 workspace 隔离。

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
cargo test --locked
cargo build --release --locked
.\scripts\package-npm.ps1 -Pack
```

## 当前边界

这一初始版本的行为仍与 Zerone 基线一致。后续工程化改造将在不破坏 Provider、工具与
历史合法性契约的前提下,逐步引入类型化工具输出、终止完备事件流、Context transform、
Session 事实日志、steering/follow-up 和受控并发。

## License

MIT

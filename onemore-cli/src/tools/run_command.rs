//! run_command:在 Windows 上执行命令(唯一"能跑任意东西"的工具)。
//!
//! ## Shell 的选择(Windows 优先)
//! 模型最熟悉 bash 语法,所以探测顺序是:
//! 1. Git Bash(装了 Git for Windows 就有)——注意要避开
//!    `System32\bash.exe`,那是 WSL,会把命令跑进 Linux 子系统;
//! 2. PowerShell(系统必有,fallback)。
//!
//! 也可在 config.toml 里用 `shell = "powershell" | "gitbash" | "cmd"` 强制指定。
//! 工具的 description 会告诉模型实际用的是哪个 shell,模型会照着写对应语法。
//!
//! ## 为什么 PowerShell 走临时 .ps1 文件而不是 -Command 内联
//! 内联要穿越三层引号规则(Rust → CreateProcess → PowerShell),
//! 多行/含引号的命令几乎必炸。写成临时脚本文件(带 UTF-8 BOM,
//! 否则 PS 5.1 按 ANSI 读)可以完全绕开引号地狱。
//! Git Bash 的 `-c` 单参数语义干净,直接内联即可。
//!
//! ## 其他硬边界
//! - 超时(默认 120s,上限 600s)与用户取消都会 `taskkill /T /F` 杀掉整棵进程树,
//!   只杀父进程会留下孤儿子进程占着端口;
//! - stdin 接 null:任何等待输入的交互式命令会立即拿到 EOF 而不是挂死;
//! - stdout/stderr 用两个线程并发读:单线程先读完一个管道,另一个写满
//!   64KB 缓冲后子进程就会永久阻塞(经典管道死锁);
//! - PowerShell 前导码强制 UTF-8 输出,否则中文 Windows 上是 GBK 乱码。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{
    optional_u64, require_str, Tool, ToolCapabilities, ToolContext, ToolError, ToolErrorCode,
    ToolOutput, ToolPermissionSpec, ToolSpec,
};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
const PIPE_CHUNK_BYTES: usize = 8 * 1024;
const MAX_DRAIN_CHUNKS_PER_TICK: usize = 256;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const PROGRESS_TAIL_BYTES: usize = 2_000;

/// 实际执行命令的 shell。
#[derive(Debug, Clone)]
pub enum Shell {
    /// Git for Windows 自带的 bash(路径指向 bash.exe)。
    GitBash(PathBuf),
    PowerShell,
    Cmd,
    /// 非 Windows 平台的 /bin/sh(保留可移植性)。
    Sh,
}

impl Shell {
    pub fn label(&self) -> &'static str {
        match self {
            Shell::GitBash(_) => "Git Bash",
            Shell::PowerShell => "PowerShell 5.1",
            Shell::Cmd => "cmd.exe",
            Shell::Sh => "sh",
        }
    }

    /// 给模型的语法提示。
    fn syntax_hint(&self) -> &'static str {
        match self {
            Shell::GitBash(_) => {
                "命令由 Git Bash 执行:用 POSIX 语法(&&、|、$VAR、/dev/null),\
                 路径正斜杠;Windows 盘符写作 /e/xxx 或直接 E:/xxx"
            }
            Shell::PowerShell => {
                "命令由 Windows PowerShell 5.1 执行:不支持 && 与 ||(用 ; 或 if $?),\
                 环境变量 $env:NAME,null 设备是 $null"
            }
            Shell::Cmd => "命令由 cmd.exe 执行:用 batch 语法(&、%VAR%、nul)",
            Shell::Sh => "命令由 /bin/sh 执行,POSIX 语法",
        }
    }
}

/// 按配置探测 shell。`preference`: auto | gitbash | powershell | cmd。
pub fn detect_shell(preference: &str) -> Shell {
    if !cfg!(windows) {
        return Shell::Sh;
    }
    match preference {
        "powershell" => Shell::PowerShell,
        "cmd" => Shell::Cmd,
        "gitbash" => find_git_bash()
            .map(Shell::GitBash)
            .unwrap_or(Shell::PowerShell),
        _ => find_git_bash()
            .map(Shell::GitBash)
            .unwrap_or(Shell::PowerShell),
    }
}

/// 在常见安装位置找 Git Bash,刻意不搜 PATH:PATH 里的 bash.exe
/// 可能是 System32 下的 WSL 入口,行为完全不同。
fn find_git_bash() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        if let Ok(v) = std::env::var(var) {
            roots.push(PathBuf::from(v));
        }
    }
    for root in roots {
        for sub in ["Git\\bin\\bash.exe", "Programs\\Git\\bin\\bash.exe"] {
            let p = root.join(sub);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub struct RunCommand {
    shell: Shell,
}

impl RunCommand {
    pub fn new(shell: Shell) -> Self {
        RunCommand { shell }
    }
}

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: format!(
                "执行一条 shell 命令并返回 stdout/stderr 与退出码。{}。命令必须是非交互式的(stdin 已接 null,任何等待输入的命令会得到 EOF);默认超时 {}s,可用 timeout_secs 调整(上限 {}s);cwd 可指定工作目录(默认为项目根目录,且每次调用互相独立、不保留 cd 状态)。",
                self.shell.syntax_hint(),
                DEFAULT_TIMEOUT_SECS,
                MAX_TIMEOUT_SECS
            ),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "command": { "type": "string", "minLength": 1, "description": "要执行的命令" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "description": "超时秒数,默认 120" },
                    "cwd": { "type": "string", "description": "工作目录,默认项目根目录" }
                },
                "required": ["command"]
            }),
            capabilities: ToolCapabilities::COMMAND,
            permission: ToolPermissionSpec::opaque_side_effect(&["cwd"]),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let command = require_str(args, "command")?;
        let timeout = Duration::from_secs(
            optional_u64(args, "timeout_secs")?
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );
        let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(p) => {
                let dir = ctx.workspace.resolve(p);
                if !dir.is_dir() {
                    return Err(ToolError::new(
                        ToolErrorCode::NotDirectory,
                        format!("cwd {} 不是目录", dir.display()),
                    ));
                }
                dir
            }
            None => ctx.workspace.root().to_path_buf(),
        };

        let (mut cmd, temp_script) =
            build_invocation(&self.shell, command).map_err(ToolError::execution)?;
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(path) = temp_script.as_ref() {
                    let _ = std::fs::remove_file(path);
                }
                return Err(ToolError::execution(format!(
                    "启动 {} 失败: {}",
                    self.shell.label(),
                    error
                )));
            }
        };

        // 两个管道各开一个读线程,chunk 经 channel 回到执行线程。这样既避免管道死锁,
        // 又保证 progress callback 永远不会逃出 ToolContext 的同步借用期。
        let (pipe_tx, pipe_rx) = mpsc::channel();
        let out_handle = spawn_pipe_reader(child.stdout.take(), Pipe::Stdout, pipe_tx.clone());
        let err_handle = spawn_pipe_reader(child.stderr.take(), Pipe::Stderr, pipe_tx.clone());
        drop(pipe_tx);

        // 主循环轮询:退出 / 超时 / 用户取消
        let started = Instant::now();
        let mut last_progress = started;
        let mut progress_dirty = false;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let ending = loop {
            progress_dirty |= drain_pipe_chunks(
                &pipe_rx,
                &mut stdout,
                &mut stderr,
                MAX_DRAIN_CHUNKS_PER_TICK,
            ) > 0;
            if progress_dirty && last_progress.elapsed() >= PROGRESS_INTERVAL {
                report_command_progress(ctx, &stdout, &stderr, started.elapsed());
                progress_dirty = false;
                last_progress = Instant::now();
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ending::Exited(status.code()),
                Ok(None) => {}
                Err(error) => {
                    kill_tree(&mut child);
                    break Ending::WaitFailed(error.to_string());
                }
            }
            if ctx.cancel.load(Ordering::Relaxed) {
                kill_tree(&mut child);
                break Ending::Cancelled;
            }
            if started.elapsed() > timeout {
                kill_tree(&mut child);
                break Ending::TimedOut(timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(40));
        };
        // 子进程结束后管道关闭,读线程自然退出。先回收进程,再 join 并排空 channel,
        // 确保退出前写入的最后一批字节不会丢失。
        let _ = child.wait();
        let stdout_read = join_pipe_reader(out_handle, "stdout");
        let stderr_read = join_pipe_reader(err_handle, "stderr");
        progress_dirty |= drain_pipe_chunks(&pipe_rx, &mut stdout, &mut stderr, usize::MAX) > 0;
        if progress_dirty {
            report_command_progress(ctx, &stdout, &stderr, started.elapsed());
        }
        if let Some(p) = temp_script {
            let _ = std::fs::remove_file(p);
        }

        if let Err(error) = stdout_read.and(stderr_read) {
            return Err(ToolError::io(error));
        }

        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();

        let mut out = String::new();
        if !stdout.trim().is_empty() {
            out.push_str(&stdout);
        }
        if !stderr.trim().is_empty() {
            if !out.is_empty() {
                out.push_str("\n--- stderr ---\n");
            }
            out.push_str(&stderr);
        }
        if out.trim().is_empty() {
            out.push_str("(无输出)");
        }

        match ending {
            Ending::Exited(Some(0)) => Ok(ToolOutput {
                model_text: out,
                ui_summary: Some("命令执行成功".into()),
                details: Some(json!({
                    "command": command,
                    "cwd": ctx.workspace.display(&cwd),
                    "exit_code": 0,
                    "elapsed_ms": started.elapsed().as_millis(),
                })),
            }),
            Ending::Exited(code) => Err(ToolError {
                code: ToolErrorCode::ProcessExit,
                message: format!(
                    "命令退出码 {}\n{}",
                    code.map(|value| value.to_string())
                        .unwrap_or_else(|| "未知".into()),
                    out
                ),
                retryable: false,
                details: Some(json!({ "exit_code": code, "command": command })),
            }),
            Ending::TimedOut(secs) => Err(ToolError {
                code: ToolErrorCode::Timeout,
                message: format!("命令超时({}s),进程树已终止\n{}", secs, out),
                retryable: false,
                details: Some(json!({ "timeout_secs": secs, "command": command })),
            }),
            Ending::Cancelled => Err(ToolError {
                code: ToolErrorCode::Aborted,
                message: format!("命令被用户取消,进程树已终止\n{}", out),
                retryable: false,
                details: Some(json!({ "command": command })),
            }),
            Ending::WaitFailed(error) => Err(ToolError {
                code: ToolErrorCode::ExecutionFailed,
                message: format!("等待进程失败: {}\n{}", error, out),
                retryable: false,
                details: Some(json!({ "command": command })),
            }),
        }
    }
}

enum Ending {
    Exited(Option<i32>),
    TimedOut(u64),
    Cancelled,
    WaitFailed(String),
}

/// 组装具体的进程调用;PowerShell 会落一个临时 .ps1(见模块注释),
/// 返回其路径以便执行后清理。
fn build_invocation(shell: &Shell, command: &str) -> Result<(Command, Option<PathBuf>), String> {
    match shell {
        Shell::GitBash(bash) => {
            let mut c = Command::new(bash);
            c.arg("-c").arg(command);
            Ok((c, None))
        }
        Shell::PowerShell => {
            let script = format!(
                "$ErrorActionPreference='Continue'\n\
                 $ProgressPreference='SilentlyContinue'\n\
                 [Console]::OutputEncoding=[System.Text.Encoding]::UTF8\n\
                 $OutputEncoding=[System.Text.Encoding]::UTF8\n\
                 {}\n\
                 if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 0 }}\n",
                command
            );
            let path = temp_script_path("ps1");
            // UTF-8 BOM:没有它 PowerShell 5.1 会按 ANSI 读取脚本,中文全坏
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(script.as_bytes());
            std::fs::write(&path, bytes).map_err(|e| format!("写临时脚本失败: {}", e))?;
            let mut c = Command::new("powershell.exe");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&path);
            Ok((c, Some(path)))
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(command);
            Ok((c, None))
        }
        Shell::Sh => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            Ok((c, None))
        }
    }
}

fn temp_script_path(ext: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("onemore-cmd-{}-{}.{}", std::process::id(), n, ext))
}

#[derive(Clone, Copy)]
enum Pipe {
    Stdout,
    Stderr,
}

struct PipeChunk {
    pipe: Pipe,
    bytes: Vec<u8>,
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    stream: Pipe,
    sender: Sender<PipeChunk>,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            return Ok(());
        };
        loop {
            let mut buffer = vec![0; PIPE_CHUNK_BYTES];
            let read = pipe.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            buffer.truncate(read);
            if sender
                .send(PipeChunk {
                    pipe: stream,
                    bytes: buffer,
                })
                .is_err()
            {
                return Ok(());
            }
        }
    })
}

fn join_pipe_reader(
    handle: std::thread::JoinHandle<std::io::Result<()>>,
    stream: &str,
) -> Result<(), String> {
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("读取 {} 失败: {}", stream, error)),
        Err(_) => Err(format!("读取 {} 的线程异常退出", stream)),
    }
}

fn drain_pipe_chunks(
    receiver: &Receiver<PipeChunk>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    limit: usize,
) -> usize {
    let mut received = 0;
    while received < limit {
        let Ok(chunk) = receiver.try_recv() else {
            break;
        };
        match chunk.pipe {
            Pipe::Stdout => stdout.extend_from_slice(&chunk.bytes),
            Pipe::Stderr => stderr.extend_from_slice(&chunk.bytes),
        }
        received += 1;
    }
    received
}

fn report_command_progress(
    ctx: &mut ToolContext<'_>,
    stdout: &[u8],
    stderr: &[u8],
    elapsed: Duration,
) {
    let preview = output_tail(stdout, stderr);
    let last_line = preview
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("等待输出");
    let summary = format!(
        "运行 {:.1}s: {}",
        elapsed.as_secs_f64(),
        crate::util::ellipsis(last_line, 160)
    );
    ctx.report_progress(ToolOutput {
        model_text: preview,
        ui_summary: Some(summary),
        details: Some(json!({
            "elapsed_ms": elapsed.as_millis(),
            "stdout_bytes": stdout.len(),
            "stderr_bytes": stderr.len(),
        })),
    });
}

fn output_tail(stdout: &[u8], stderr: &[u8]) -> String {
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(&lossy_tail(stdout));
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push_str("\n--- stderr (tail) ---\n");
        }
        output.push_str(&lossy_tail(stderr));
    }
    output
}

fn lossy_tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(PROGRESS_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// 终止整棵进程树。Windows 用 taskkill /T,其余平台退回单进程 kill。
fn kill_tree(child: &mut Child) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use std::sync::atomic::AtomicBool;

    fn ws() -> Workspace {
        Workspace::new(std::env::temp_dir())
    }

    fn run(tool: &RunCommand, args: Value) -> Result<ToolOutput, ToolError> {
        let workspace = ws();
        let cancel = AtomicBool::new(false);
        tool.execute(
            &args,
            &mut ToolContext {
                workspace: &workspace,
                cancel: &cancel,
                session_id: "test",
                current_plan: crate::plan::PlanSnapshot::default(),
                progress: &mut |_| {},
                effects: Vec::new(),
            },
        )
    }

    #[test]
    fn echo_roundtrip() {
        let tool = RunCommand::new(detect_shell("auto"));
        let r = run(&tool, json!({"command": "echo hello-onemore"}));
        assert!(
            r.as_ref().unwrap().model_text.contains("hello-onemore"),
            "{:?}",
            r
        );
    }

    #[test]
    fn nonzero_exit_is_error() {
        let tool = RunCommand::new(detect_shell("auto"));
        let r = run(&tool, json!({"command": "exit 3"}));
        let error = r.unwrap_err();
        assert_eq!(error.code, ToolErrorCode::ProcessExit);
        assert!(error.message.contains("退出码 3"));
    }

    #[test]
    fn timeout_kills() {
        let tool = RunCommand::new(detect_shell("auto"));
        let t0 = Instant::now();
        let r = run(&tool, json!({"command": "sleep 30", "timeout_secs": 2}));
        let error = r.unwrap_err();
        assert_eq!(error.code, ToolErrorCode::Timeout);
        assert!(error.message.contains("超时"));
        assert!(t0.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn output_is_reported_before_command_settles() {
        let tool = RunCommand::new(detect_shell("auto"));
        let command = match &tool.shell {
            Shell::GitBash(_) | Shell::Sh => "echo progress-onemore; sleep 0.4",
            Shell::PowerShell => "Write-Output progress-onemore; Start-Sleep -Milliseconds 400",
            Shell::Cmd => "echo progress-onemore & ping 127.0.0.1 -n 2 > nul",
        };
        let workspace = ws();
        let cancel = AtomicBool::new(false);
        let mut updates = Vec::new();
        let result = {
            let mut progress = |update| updates.push(update);
            tool.execute(
                &json!({ "command": command }),
                &mut ToolContext {
                    workspace: &workspace,
                    cancel: &cancel,
                    session_id: "test",
                    current_plan: crate::plan::PlanSnapshot::default(),
                    progress: &mut progress,
                    effects: Vec::new(),
                },
            )
        };

        assert!(result.is_ok(), "{result:?}");
        assert!(
            updates
                .iter()
                .any(|update| update.model_text.contains("progress-onemore")),
            "应在命令结束前收到 stdout progress: {updates:?}"
        );
        assert!(updates.iter().all(|update| update.details.is_some()));
    }
}

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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{optional_u64, require_str, Tool};
use crate::workspace::Workspace;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

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
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> String {
        format!(
            "执行一条 shell 命令并返回 stdout/stderr 与退出码。{}。\
             命令必须是非交互式的(stdin 已接 null,任何等待输入的命令会得到 EOF);\
             默认超时 {}s,可用 timeout_secs 调整(上限 {}s);\
             cwd 可指定工作目录(默认为项目根目录,且每次调用互相独立、不保留 cd 状态)。",
            self.shell.syntax_hint(),
            DEFAULT_TIMEOUT_SECS,
            MAX_TIMEOUT_SECS
        )
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令" },
                "timeout_secs": { "type": "integer", "description": "超时秒数,默认 120" },
                "cwd": { "type": "string", "description": "工作目录,默认项目根目录" }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, args: &Value, ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
        let command = require_str(args, "command")?;
        let timeout = Duration::from_secs(
            optional_u64(args, "timeout_secs")?
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );
        let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(p) => {
                let dir = ws.resolve(p);
                if !dir.is_dir() {
                    return Err(format!("cwd {} 不是目录", dir.display()));
                }
                dir
            }
            None => ws.root().to_path_buf(),
        };

        let (mut cmd, temp_script) = build_invocation(&self.shell, command)?;
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 {} 失败: {}", self.shell.label(), e))?;

        // 两个管道各开一个读线程,避免管道写满导致的死锁
        let out_handle = spawn_pipe_reader(child.stdout.take());
        let err_handle = spawn_pipe_reader(child.stderr.take());

        // 主循环轮询:退出 / 超时 / 用户取消
        let started = Instant::now();
        let ending = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ending::Exited(status.code()),
                Ok(None) => {}
                Err(e) => break Ending::WaitFailed(e.to_string()),
            }
            if cancel.load(Ordering::Relaxed) {
                kill_tree(&mut child);
                break Ending::Cancelled;
            }
            if started.elapsed() > timeout {
                kill_tree(&mut child);
                break Ending::TimedOut(timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(40));
        };
        // 杀掉后管道关闭,读线程自然结束
        let stdout = out_handle.join().unwrap_or_default();
        let stderr = err_handle.join().unwrap_or_default();
        let _ = child.wait(); // 确保回收
        if let Some(p) = temp_script {
            let _ = std::fs::remove_file(p);
        }

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
            Ending::Exited(Some(0)) => Ok(out),
            Ending::Exited(code) => Err(format!(
                "命令退出码 {}\n{}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "未知".into()),
                out
            )),
            Ending::TimedOut(secs) => Err(format!("命令超时({}s),进程树已终止\n{}", secs, out)),
            Ending::Cancelled => Err(format!("命令被用户取消,进程树已终止\n{}", out)),
            Ending::WaitFailed(e) => Err(format!("等待进程失败: {}\n{}", e, out)),
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

fn spawn_pipe_reader<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
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
    use std::sync::atomic::AtomicBool;

    fn ws() -> Workspace {
        Workspace::new(std::env::temp_dir())
    }

    #[test]
    fn echo_roundtrip() {
        let tool = RunCommand::new(detect_shell("auto"));
        let r = tool.execute(
            &json!({"command": "echo hello-onemore"}),
            &ws(),
            &AtomicBool::new(false),
        );
        assert!(r.as_ref().unwrap().contains("hello-onemore"), "{:?}", r);
    }

    #[test]
    fn nonzero_exit_is_error() {
        let tool = RunCommand::new(detect_shell("auto"));
        let r = tool.execute(
            &json!({"command": "exit 3"}),
            &ws(),
            &AtomicBool::new(false),
        );
        assert!(r.unwrap_err().contains("退出码 3"));
    }

    #[test]
    fn timeout_kills() {
        let tool = RunCommand::new(detect_shell("auto"));
        let t0 = Instant::now();
        let r = tool.execute(
            &json!({"command": "sleep 30", "timeout_secs": 2}),
            &ws(),
            &AtomicBool::new(false),
        );
        assert!(r.unwrap_err().contains("超时"));
        assert!(t0.elapsed() < Duration::from_secs(20));
    }
}

//! 可执行入口:参数解析、配置引导、装配 Agent,然后二选一:
//! - 默认:Runtime 进工作线程 + TUI 前端;
//! - `--once "提示词"`:headless 单轮模式,直接在当前线程跑完打印退出。
//!
//! headless 模式的存在不只是为了方便调试:它和 TUI 消费**同一条事件流**,
//! 证明 Runtime 与前端确实解耦(未来接 GUI/Web 就是再写一个消费者)。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};

use onemore::config::{Config, EXAMPLE_CONFIG};
use onemore::event::{AgentCommand, AgentEvent};
use onemore::runtime::{self, Agent};
use onemore::storage::AppPaths;
use onemore::workspace::Workspace;

const HELP: &str = "Onemore —— 可靠、实用的 Coding Agent

用法:
  onemore                     启动 TUI
  onemore --once <提示词...>   无界面跑一轮(方便调试/脚本化)

选项:
  -c, --config <路径>    配置文件(默认 ~/.onemore/config.toml,不存在会生成模板)
  -p, --provider <名字>  覆盖 [agent].provider
  -h, --help             显示本帮助
";

struct Args {
    config: PathBuf,
    provider: Option<String>,
    once: Option<String>,
}

fn parse_args(default_config: PathBuf) -> Result<Option<Args>> {
    let mut config = default_config;
    let mut provider = None;
    let mut once = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-c" | "--config" => {
                config = PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--config 缺参数"))?,
                )
            }
            "-p" | "--provider" => {
                provider = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--provider 缺参数"))?,
                )
            }
            "--once" => {
                // 其后所有参数拼成提示词
                let rest: Vec<String> = it.by_ref().collect();
                if rest.is_empty() {
                    bail!("--once 需要提示词");
                }
                once = Some(rest.join(" "));
            }
            other => bail!("未知参数 {:?},-h 查看用法", other),
        }
    }
    Ok(Some(Args {
        config,
        provider,
        once,
    }))
}

fn main() -> Result<()> {
    let paths = AppPaths::discover()?;
    let Some(args) = parse_args(paths.config.clone())? else {
        print!("{}", HELP);
        return Ok(());
    };
    paths.ensure()?;

    // 首次运行:生成配置模板后退出,提示用户填 key
    if !args.config.exists() {
        if let Some(parent) = args.config.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&args.config, EXAMPLE_CONFIG)?;
        println!(
            "已生成 Onemore 配置模板 {}。\n填好 API key(或设置对应环境变量)后重新运行 onemore。",
            args.config.display()
        );
        return Ok(());
    }

    let mut cfg = Config::load(&args.config)?;
    if let Some(p) = args.provider {
        // 提前校验,给出可用列表
        cfg.resolve_provider(&p)?;
        cfg.active_provider = p;
    }

    let workspace = Workspace::new(std::env::current_dir()?);
    let agent = Agent::new(cfg, workspace)?;

    match args.once {
        Some(prompt) => run_once(agent, prompt),
        None => onemore::tui::run(runtime::spawn(agent)),
    }
}

/// headless 前端:事件流直接打到终端。
/// 助手正文走 stdout(方便管道),过程信息走 stderr。
fn run_once(mut agent: Agent, prompt: String) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let mut failed = false;
    let mut streamed = false;
    {
        let mut emit = |ev: AgentEvent| match ev {
            AgentEvent::UserMessage(t) => eprintln!("❯ {}", t),
            AgentEvent::AssistantDelta(t) => {
                streamed = true;
                print!("{}", t);
                let _ = std::io::stdout().flush();
            }
            AgentEvent::AssistantMessage(_) if streamed => {
                println!();
                streamed = false;
            }
            AgentEvent::AssistantMessage(_) => {}
            AgentEvent::ToolCallStarted { name, summary, .. } => {
                eprintln!("● {}({})", name, summary);
            }
            AgentEvent::ToolCallUpdated { output, .. } => {
                eprintln!("  … {}", onemore::util::ellipsis(output.ui_text(), 120));
            }
            AgentEvent::ToolCallFinished { output, error, .. } => {
                let shown = output.ui_text();
                let first = shown.lines().next().unwrap_or("");
                let more = shown.lines().count().saturating_sub(1);
                eprintln!(
                    "  └ {}{}{}",
                    if error.is_some() { "✖ " } else { "" },
                    onemore::util::ellipsis(first, 120),
                    if more > 0 {
                        format!(" (+{} 行)", more)
                    } else {
                        String::new()
                    }
                );
            }
            AgentEvent::PermissionRequested { request } => {
                eprintln!(
                    "? {}({}) 需要审批: {}",
                    request.tool, request.summary, request.reason
                );
            }
            AgentEvent::PermissionResolved { allowed, .. } => {
                eprintln!("  {}", if allowed { "已允许" } else { "未允许" });
            }
            AgentEvent::PlanUpdated {
                revision,
                items,
                explanation,
            } => {
                eprintln!("· 计划 #{}", revision);
                if let Some(explanation) = explanation {
                    eprintln!("  {}", explanation);
                }
                for item in items {
                    eprintln!("  [{}] {}: {}", item.status.as_str(), item.id, item.text);
                }
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache,
            } => {
                if let Some(cache) = cache {
                    eprintln!(
                        "· 用量 ↑{} ↓{} · cache read {} write {}",
                        input_tokens, output_tokens, cache.read_tokens, cache.write_tokens
                    );
                } else {
                    eprintln!("· 用量 ↑{} ↓{}", input_tokens, output_tokens);
                }
            }
            AgentEvent::Notice(t) => eprintln!("· {}", t),
            AgentEvent::Error(t) => {
                failed = true;
                eprintln!("✖ {}", t);
            }
            _ => {}
        };
        agent.handle_command(AgentCommand::UserInput(prompt), &mut emit, &cancel);
    }
    if failed {
        bail!("本轮出现错误");
    }
    Ok(())
}

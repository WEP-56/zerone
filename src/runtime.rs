//! # Agent Runtime:整个项目的心脏
//!
//! [`Agent::run_turn`] 就是教科书上的 Agent Loop:
//!
//! ```text
//! 用户输入
//!   └─► 组装上下文(ContextProvider 们)
//!        └─► 调模型(Provider,流式)
//!             ├─ 没有工具调用 ──► 本轮结束
//!             └─ 有工具调用 ──► 逐个执行(ToolRegistry)
//!                  └─► 结果作为 Observation 写回历史 ──► 回到"调模型"
//! ```
//!
//! Runtime 对外只有两条通道(见 `event.rs`)+ 一个取消标志,
//! 由 [`spawn`] 起一个工作线程承载;`--once` 模式则直接在当前线程
//! 调 [`Agent::handle_command`]——同一份循环,两种前端。
//!
//! 这里还负责两件容易被忽视的"工程正确性":
//! - **历史必须始终合法**:每个 ToolUse 都要有配对的 ToolResult。
//!   所以取消发生在工具执行阶段时,剩下没执行的调用会补上
//!   "已取消"结果再收尾;取消发生在流式阶段时,直接丢弃半截
//!   assistant 消息(历史停在上一条 user 消息,依然合法)。
//! - **重试要幂等**:只有"一个字都还没吐出来"的失败才自动重试,
//!   吐了一半再重试会让用户看到重复内容。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::context::conversation::Conversation;
use crate::context::instructions::Instructions;
use crate::context::workspace_info::WorkspaceInfo;
use crate::context::{ContextProvider, PromptContext};
use crate::event::{AgentCommand, AgentEvent};
use crate::message::{Block, ChatMessage, Role, StopReason, Usage};
use crate::provider::{build_provider, Provider, ProviderEvent};
use crate::tools::{default_registry, detect_shell, ToolRegistry, ToolSpec};
use crate::util;
use crate::workspace::Workspace;

/// 自动重试的最大尝试次数(含首次)。
const MAX_ATTEMPTS: u32 = 3;
/// 工具调用在聊天区显示的结果预览截断长度。
const EVENT_OUTPUT_MAX: usize = 4000;

pub struct Agent {
    workspace: Workspace,
    tools: ToolRegistry,
    /// 对话历史。Runtime 具体持有(要写入),同时它也是一个 ContextProvider。
    conversation: Conversation,
    /// 其余上下文源。想加 Planning/Memory/Workspace Map,往这里 push 即可。
    extra_context: Vec<Box<dyn ContextProvider>>,
    provider: Box<dyn Provider>,
    config: Config,
    usage_total: Usage,
}

impl Agent {
    pub fn new(config: Config, workspace: Workspace) -> anyhow::Result<Agent> {
        let shell = detect_shell(&config.shell);
        let extra_context: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(Instructions::new(config.system_prompt.clone())),
            Box::new(WorkspaceInfo::new(&shell)),
            // ← 未来的 PlanningContext / MemoryContext 插在这里
        ];
        let settings = config.resolve_provider(&config.active_provider)?;
        Ok(Agent {
            workspace,
            tools: default_registry(shell),
            conversation: Conversation::new(),
            extra_context,
            provider: build_provider(settings),
            config,
            usage_total: Usage::default(),
        })
    }

    pub fn provider_label(&self) -> String {
        self.provider.label()
    }

    /// 处理一条命令;返回 false 表示 Runtime 应当退出。
    pub fn handle_command(
        &mut self,
        cmd: AgentCommand,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> bool {
        match cmd {
            AgentCommand::UserInput(text) => {
                self.run_turn(text, emit, cancel);
                true
            }
            AgentCommand::ClearConversation => {
                self.conversation.clear();
                self.usage_total = Usage::default();
                emit(AgentEvent::ConversationCleared);
                true
            }
            AgentCommand::SwitchProvider(name) => {
                match self.config.resolve_provider(&name) {
                    Ok(settings) => {
                        self.provider = build_provider(settings);
                        emit(AgentEvent::ProviderChanged {
                            label: self.provider.label(),
                        });
                        emit(AgentEvent::Notice(format!(
                            "已切换到 {}(历史保留)",
                            self.provider.label()
                        )));
                    }
                    Err(e) => emit(AgentEvent::Error(format!("切换失败: {:#}", e))),
                }
                true
            }
            AgentCommand::SetModel(model) => {
                self.provider.set_model(model);
                emit(AgentEvent::ProviderChanged {
                    label: self.provider.label(),
                });
                emit(AgentEvent::Notice(format!(
                    "模型已设为 {}",
                    self.provider.label()
                )));
                true
            }
            AgentCommand::Shutdown => false,
        }
    }

    /// 组装本轮 prompt:依次让每个 ContextProvider 注入,历史最后。
    fn build_prompt(&self) -> PromptContext {
        let mut prompt = PromptContext::default();
        for c in &self.extra_context {
            c.contribute(&mut prompt, &self.workspace);
        }
        self.conversation.contribute(&mut prompt, &self.workspace);
        prompt
    }

    /// Agent Loop 本体。
    fn run_turn(&mut self, input: String, emit: &mut dyn FnMut(AgentEvent), cancel: &AtomicBool) {
        emit(AgentEvent::UserMessage(input.clone()));
        self.conversation.push(ChatMessage::user_text(input));
        emit(AgentEvent::TurnStarted);

        let specs: Vec<ToolSpec> = self.tools.specs();
        for _round in 0..self.config.max_turns {
            if cancel.load(Ordering::Relaxed) {
                emit(AgentEvent::TurnFinished { cancelled: true });
                return;
            }

            // ---- 1. 调模型(带"未开播才重试"的重试) ----
            let prompt = self.build_prompt();
            let output = match self.call_model(&prompt, &specs, emit, cancel) {
                CallResult::Cancelled => {
                    // 半截 assistant 输出直接丢弃:历史停在 user 消息上,仍然合法
                    emit(AgentEvent::TurnFinished { cancelled: true });
                    return;
                }
                CallResult::Failed(msg) => {
                    emit(AgentEvent::Error(msg));
                    emit(AgentEvent::TurnFinished { cancelled: false });
                    return;
                }
                CallResult::Done(out) => out,
            };

            self.usage_total.add(output.usage);
            emit(AgentEvent::Usage {
                input_tokens: self.usage_total.input_tokens,
                output_tokens: self.usage_total.output_tokens,
            });

            // ---- 2. 提交 assistant 消息进历史 ----
            let text = output.message.text();
            if !text.is_empty() {
                emit(AgentEvent::AssistantMessage(text));
            }
            let calls: Vec<(String, String, serde_json::Value)> = output
                .message
                .tool_uses()
                .into_iter()
                .map(|(id, name, args)| (id.to_string(), name.to_string(), args.clone()))
                .collect();
            self.conversation.push(output.message);

            // ---- 3. 没有工具调用 → 本轮结束 ----
            if calls.is_empty() {
                if output.stop == StopReason::MaxTokens {
                    emit(AgentEvent::Notice(
                        "输出撞到 max_tokens 上限,可能不完整".into(),
                    ));
                }
                emit(AgentEvent::TurnFinished { cancelled: false });
                return;
            }

            // ---- 4. 逐个执行工具,结果(Observation)写回历史 ----
            let mut results: Vec<Block> = Vec::new();
            let mut was_cancelled = false;
            for (id, name, args) in calls {
                if cancel.load(Ordering::Relaxed) {
                    // 取消后,剩余调用补"已取消"结果——保证 ToolUse/ToolResult 配对
                    was_cancelled = true;
                    results.push(Block::ToolResult {
                        tool_use_id: id,
                        content: "[用户取消,本工具未执行]".into(),
                        is_error: true,
                    });
                    continue;
                }
                emit(AgentEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                    summary: util::args_summary(&args),
                });
                let outcome = self.tools.execute(&name, &args, &self.workspace, cancel);
                emit(AgentEvent::ToolCallFinished {
                    id: id.clone(),
                    name,
                    output: util::truncate_middle(&outcome.content, EVENT_OUTPUT_MAX),
                    is_error: outcome.is_error,
                });
                results.push(Block::ToolResult {
                    tool_use_id: id,
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }
            self.conversation.push(ChatMessage {
                role: Role::User,
                blocks: results,
            });
            if was_cancelled {
                emit(AgentEvent::TurnFinished { cancelled: true });
                return;
            }
            // 回到循环顶部,把 Observation 喂给模型
        }

        emit(AgentEvent::Notice(format!(
            "连续调用模型达到上限({} 次),强制结束本轮;可直接输入\"继续\"接着跑",
            self.config.max_turns
        )));
        emit(AgentEvent::TurnFinished { cancelled: false });
    }

    /// 单次模型调用 + 重试策略。
    fn call_model(
        &self,
        prompt: &PromptContext,
        specs: &[ToolSpec],
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> CallResult {
        let mut attempt = 1u32;
        loop {
            let mut emitted_any = false;
            let mut forward = |pe: ProviderEvent| {
                emitted_any = true;
                emit(match pe {
                    ProviderEvent::TextDelta(t) => AgentEvent::AssistantDelta(t),
                    ProviderEvent::ThinkingDelta(t) => AgentEvent::ThinkingDelta(t),
                    ProviderEvent::ToolCallBegun { name } => AgentEvent::ToolCallPending { name },
                });
            };
            match self
                .provider
                .stream_turn(prompt, specs, &mut forward, cancel)
            {
                Ok(Some(out)) => return CallResult::Done(out),
                Ok(None) => return CallResult::Cancelled,
                Err(e) => {
                    let can_retry = e.retryable && !emitted_any && attempt < MAX_ATTEMPTS;
                    if !can_retry {
                        return CallResult::Failed(format!("模型调用失败: {}", e));
                    }
                    let wait = e
                        .retry_after
                        .unwrap_or(Duration::from_secs(1u64 << attempt)); // 2s, 4s
                    emit(AgentEvent::Notice(format!(
                        "{},{}s 后重试({}/{})",
                        e,
                        wait.as_secs(),
                        attempt,
                        MAX_ATTEMPTS - 1
                    )));
                    // 分片睡眠,期间可被取消
                    let mut slept = Duration::ZERO;
                    while slept < wait {
                        if cancel.load(Ordering::Relaxed) {
                            return CallResult::Cancelled;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                        slept += Duration::from_millis(100);
                    }
                    attempt += 1;
                }
            }
        }
    }
}

enum CallResult {
    Done(crate::provider::TurnOutput),
    Cancelled,
    Failed(String),
}

/// 前端持有的 Runtime 句柄。
pub struct RuntimeHandle {
    pub commands: Sender<AgentCommand>,
    pub events: Receiver<AgentEvent>,
    /// 置 true 请求取消当前轮;Runtime 会在收尾后自行复位。
    pub cancel: Arc<AtomicBool>,
    pub provider_label: String,
    pub provider_names: Vec<String>,
}

/// 把 Agent 装进工作线程,返回通道句柄。TUI 前端用这个;
/// headless 前端不需要线程,直接调 `Agent::handle_command`。
pub fn spawn(agent: Agent) -> RuntimeHandle {
    let provider_label = agent.provider_label();
    let provider_names = agent.config.provider_names();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<AgentCommand>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();

    std::thread::Builder::new()
        .name("agent-runtime".into())
        .spawn(move || {
            let mut agent = agent;
            let mut emit = |e: AgentEvent| {
                // 前端先退出时 send 会失败,忽略即可(线程随后收到 Shutdown 或通道关闭)
                let _ = evt_tx.send(e);
            };
            while let Ok(cmd) = cmd_rx.recv() {
                // 新命令开始前复位取消标志(上一轮的取消不该波及这一轮)
                cancel_worker.store(false, Ordering::Relaxed);
                if !agent.handle_command(cmd, &mut emit, &cancel_worker) {
                    break;
                }
            }
        })
        .expect("无法创建 runtime 线程");

    RuntimeHandle {
        commands: cmd_tx,
        events: evt_rx,
        cancel,
        provider_label,
        provider_names,
    }
}

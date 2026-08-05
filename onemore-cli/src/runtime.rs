//! # Agent Runtime:整个项目的心脏
//!
//! [`Agent::run_turn`] 就是教科书上的 Agent Loop:
//!
//! ```text
//! 用户输入(作为 Message 事实落库)
//!   └─► 事实日志 ──单向投影──► 模型消息 ──预算──► PromptContext
//!        └─► 调模型(Provider,流式)
//!             ├─ 没有工具调用 ──► 本轮结束
//!             └─ 有工具调用 ──► 逐个执行(ToolRegistry)
//!                  └─► 结果作为 Observation 事实落库 ──► 回到"投影"
//! ```
//!
//! Runtime 对外只有两条通道(见 `event.rs`)+ 一个取消标志,
//! 由 [`spawn`] 起一个工作线程承载;`--once` 模式则直接在当前线程
//! 调 [`Agent::handle_command`]——同一份循环,两种前端。
//!
//! 阶段 4 之后,这里遵守"事实先行"的持久化纪律:
//! - **事实日志是唯一权威**:Agent 只持有 `Vec<SessionEntry>` 内存镜像,
//!   模型视图每轮由 `session::project_model_messages` 重新投影,
//!   UI-only 事实(Notice 等)永远不会进 Provider。
//! - **历史必须始终合法**:每个 ToolUse 都要有配对的 ToolResult;
//!   带工具的批在提交边界还会被 `validate_new_message_batch` 复核。
//! - **提交失败不装作没事**:任何一批事实写库失败,内存镜像不推进、
//!   本轮立即终止并报错——宁可少跑一轮,不让内存与磁盘历史分叉。
//! - **重试要幂等**:只有"一个字都还没吐出来"的失败才自动重试。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::context::budget::{apply_budget, estimate_tokens, BudgetDecision, ContextBudget};
use crate::context::instructions::Instructions;
use crate::context::workspace_info::WorkspaceInfo;
use crate::context::{ContextProvider, PromptContext};
use crate::event::{AgentCommand, AgentEvent};
use crate::hooks::HookRegistry;
use crate::message::{Block, ChatMessage, Role, StopReason, Usage};
use crate::permission::{
    ApprovalDecision, ApprovalRequest, ApprovalResponse, ApprovalScope, PermissionDecision,
    PermissionManager,
};
use crate::provider::{build_provider, FailedTurn, Provider, ProviderEvent, StreamTerminal};
use crate::session::{
    project_model_messages, CompactionRecord, ModelChangeRecord, NoticeLevel, NoticeRecord,
    SessionEntry, SessionEntryPayload,
};
use crate::storage::{AppPaths, SessionManager};
use crate::tools::{
    default_registry, detect_shell, normalize_outcome, PreparedToolCall, ToolContext, ToolError,
    ToolErrorCode, ToolOutcome, ToolOutput, ToolRegistry, ToolSpec,
};
use crate::util;
use crate::workspace::Workspace;

/// 压缩会话时喂给模型的系统提示。
const COMPACTION_SYSTEM_PROMPT: &str = "你是会话压缩器。把给出的完整对话压缩成一段可以\
替代旧历史的摘要,必须保留:用户的目标与约束、已完成/未完成的事项、关键文件路径与\
修改内容、重要的命令输出结论、当前待办。省略寒暄与失败后已被纠正的探索。直接输出\
摘要正文,不要加任何前后缀。";

/// 请求级重试策略。只覆盖"尚未产生任何流事件"的失败(重试幂等由调用方保证);
/// 全部决策收敛在 [`RetryPolicy::delay_for`] 这一个纯函数里,便于确定性测试。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 最大尝试次数(含首次)。
    pub max_attempts: u32,
    /// 指数退避基数(第 1 次失败后等待 base,之后翻倍)。
    pub base_delay: Duration,
    /// 退避上限(含 jitter 之后)。
    pub max_delay: Duration,
    /// 服务器 Retry-After 超过它就放弃重试:不为一个请求无限期挂住 Runtime。
    pub max_retry_after: Duration,
    /// jitter 种子。相同种子产生相同序列,测试可注入固定值。
    pub jitter_seed: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(60),
            jitter_seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl RetryPolicy {
    /// `attempt` 是刚失败的第几次尝试(从 1 开始)。返回 None = 不再重试。
    /// 服务器给出的 Retry-After 优先且不加 jitter;超过上限直接放弃。
    pub fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        if let Some(server_wait) = retry_after {
            if server_wait > self.max_retry_after {
                return None;
            }
            return Some(server_wait);
        }
        let exponent = attempt.saturating_sub(1).min(20);
        let backoff = self
            .base_delay
            .saturating_mul(1u32 << exponent)
            .min(self.max_delay);
        // 加 [0,25%) 的确定性 jitter,避免多客户端整点齐射;最终仍受 max_delay 约束。
        let jitter = backoff.mul_f64(self.jitter_fraction(attempt));
        Some((backoff + jitter).min(self.max_delay))
    }

    /// splitmix64 变体:同 (seed, attempt) 恒定,取值 [0, 0.25)。
    fn jitter_fraction(&self, attempt: u32) -> f64 {
        let mut x = self
            .jitter_seed
            .wrapping_add((attempt as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        (x % 1000) as f64 / 4000.0
    }
}

pub struct Agent {
    workspace: Workspace,
    tools: ToolRegistry,
    /// 会话事实日志的内存镜像。只在对应批次成功落库后推进(见 [`Agent::commit`])。
    entries: Vec<SessionEntry>,
    /// 上下文源(system 片段)。想加 Planning/Memory/Workspace Map,往这里 push 即可。
    extra_context: Vec<Box<dyn ContextProvider>>,
    provider: Box<dyn Provider>,
    budget: ContextBudget,
    retry_policy: RetryPolicy,
    config: Config,
    usage_total: Usage,
    sessions: SessionManager,
    permissions: PermissionManager,
    hooks: HookRegistry,
    approval_rx: Option<Receiver<ApprovalResponse>>,
    /// 活动运行中收到、需要等本轮结束再执行的命令(/clear、/provider 等)。
    deferred: std::collections::VecDeque<AgentCommand>,
}

impl Agent {
    pub fn new(config: Config, workspace: Workspace) -> anyhow::Result<Agent> {
        let paths = AppPaths::discover()?;
        paths.ensure()?;
        Self::new_with_data_dir(config, workspace, paths.root)
    }

    /// 显式指定数据目录，供测试与嵌入场景隔离用户的 `~/.onemore`。
    pub fn new_with_data_dir(
        config: Config,
        workspace: Workspace,
        data_dir: std::path::PathBuf,
    ) -> anyhow::Result<Agent> {
        let shell = detect_shell(&config.shell);
        let extra_context: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(Instructions::new(config.system_prompt.clone())),
            Box::new(WorkspaceInfo::new(&shell)),
            // ← 未来的 PlanningContext / MemoryContext 插在这里
        ];
        let settings = config.resolve_provider(&config.active_provider)?;
        let budget = budget_from_settings(&settings);
        let paths = AppPaths::from_root(data_dir);
        paths.ensure()?;
        let sessions = SessionManager::create(paths.sessions, workspace.root())?;
        let permissions = PermissionManager::new(config.permission_rules);
        Ok(Agent {
            workspace,
            tools: default_registry(shell),
            entries: Vec::new(),
            extra_context,
            provider: build_provider(settings),
            budget,
            retry_policy: RetryPolicy::default(),
            config,
            usage_total: Usage::default(),
            sessions,
            permissions,
            hooks: HookRegistry::default(),
            approval_rx: None,
            deferred: std::collections::VecDeque::new(),
        })
    }

    pub fn provider_label(&self) -> String {
        self.provider.label()
    }

    pub fn session_id(&self) -> &str {
        self.sessions.current_id()
    }

    /// 处理一条命令;返回 false 表示 Runtime 应当退出。
    /// 无 inbox 版本供 --once 与测试使用:活动运行中没有可注入的输入。
    pub fn handle_command(
        &mut self,
        cmd: AgentCommand,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> bool {
        self.handle_command_with_inbox(cmd, emit, cancel, None)
    }

    /// `inbox` 是命令通道的接收端:活动运行会在检查点(完整工具批之后、
    /// 任务将停止时)排干它,把新输入分类为 steering / follow-up,
    /// 把其余命令延迟到本轮结束(见 [`Agent::take_deferred`])。
    pub fn handle_command_with_inbox(
        &mut self,
        cmd: AgentCommand,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
        inbox: Option<&Receiver<AgentCommand>>,
    ) -> bool {
        match cmd {
            // 空闲时三者等价:都开启一个新的运行。
            AgentCommand::UserInput(text)
            | AgentCommand::Steer(text)
            | AgentCommand::FollowUp(text) => {
                self.run_turn(text, emit, cancel, inbox);
                true
            }
            AgentCommand::Compact => {
                self.compact(emit, cancel);
                true
            }
            AgentCommand::ClearConversation => {
                match self.sessions.clear() {
                    Ok(()) => {
                        self.entries.clear();
                        self.usage_total = Usage::default();
                        self.permissions.clear_session_grants();
                        emit(AgentEvent::ConversationCleared);
                    }
                    Err(e) => emit(AgentEvent::Error(format!("清空会话数据库失败: {:#}", e))),
                }
                true
            }
            AgentCommand::SwitchProvider(name) => {
                match self.config.resolve_provider(&name) {
                    Ok(settings) => {
                        self.budget = budget_from_settings(&settings);
                        self.provider = build_provider(settings);
                        self.record_model_change(emit);
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
                self.record_model_change(emit);
                emit(AgentEvent::ProviderChanged {
                    label: self.provider.label(),
                });
                emit(AgentEvent::Notice(format!(
                    "模型已设为 {}",
                    self.provider.label()
                )));
                true
            }
            AgentCommand::ListSessions => {
                match self.sessions.list() {
                    Ok(sessions) => emit(AgentEvent::SessionsListed {
                        current_id: self.sessions.current_id().to_string(),
                        sessions,
                    }),
                    Err(e) => emit(AgentEvent::Error(format!("读取会话列表失败: {:#}", e))),
                }
                true
            }
            AgentCommand::LoadSession(id) => {
                match self.sessions.load(&id) {
                    Ok((entries, usage)) => {
                        self.permissions.clear_session_grants();
                        self.entries = entries.clone();
                        self.usage_total = usage;
                        emit(AgentEvent::SessionLoaded {
                            id: self.sessions.current_id().to_string(),
                            entries,
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache: usage.cache,
                        });
                    }
                    Err(e) => emit(AgentEvent::Error(format!("恢复会话失败: {:#}", e))),
                }
                true
            }
            AgentCommand::Shutdown => false,
        }
    }

    /// provider/model 变化是会话事实:恢复会话时可以据此解释历史。
    fn record_model_change(&mut self, emit: &mut dyn FnMut(AgentEvent)) {
        let payload = SessionEntryPayload::ModelChange(ModelChangeRecord {
            provider: self.provider.label(),
            model: self.provider.model().to_string(),
        });
        self.commit(vec![payload], emit);
    }

    /// 把一批事实原子落库;成功则推进内存镜像并返回 true。
    /// 失败时内存镜像不动(与磁盘保持一致),调用方必须终止当前活动。
    fn commit(
        &mut self,
        payloads: Vec<SessionEntryPayload>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> bool {
        match self.sessions.append_payloads(payloads, self.usage_total) {
            Ok(mut appended) => {
                self.entries.append(&mut appended);
                true
            }
            Err(e) => {
                emit(AgentEvent::Error(format!(
                    "保存会话失败,本批事实未写入,已停止本轮以避免内存与磁盘历史分叉: {:#}",
                    e
                )));
                false
            }
        }
    }

    /// 组装本轮 prompt 的 system 部分(messages 由事实投影 + 预算决定)。
    fn build_system_prompt(&self) -> PromptContext {
        let mut prompt = PromptContext::default();
        for c in &self.extra_context {
            c.contribute(&mut prompt, &self.workspace);
        }
        prompt
    }

    /// 投影 + 预算:决定本轮真正发给模型的消息。
    /// 返回 None 表示超出预算被拒绝(事件已发出),调用方应结束本轮。
    fn project_for_model(
        &self,
        prompt: &PromptContext,
        specs: &[ToolSpec],
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Option<Vec<ChatMessage>> {
        let projection = project_model_messages(&self.entries);
        for diagnostic in &projection.diagnostics {
            // 防御性修复只该发生在旧库/损坏数据上;必须让用户看见,而不是静默掩盖。
            emit(AgentEvent::Notice(format!("历史投影修复: {}", diagnostic)));
        }
        let system_chars = prompt.system_text().chars().count() as u64;
        let tools_chars = tool_spec_chars(specs);
        match apply_budget(&self.budget, system_chars, tools_chars, projection) {
            BudgetDecision::Send {
                messages, notices, ..
            } => {
                for notice in notices {
                    emit(AgentEvent::Notice(notice));
                }
                Some(messages)
            }
            BudgetDecision::Refuse {
                estimated_tokens,
                available_tokens,
            } => {
                emit(AgentEvent::Error(format!(
                    "上下文估算约 {} tokens,超出可用预算 {}(窗口扣除输出预留)。\
                     未发送请求;请用 /compact 压缩历史,或 /clear 重新开始。",
                    estimated_tokens, available_tokens
                )));
                None
            }
        }
    }

    /// Agent Loop 本体。一次调用是一个完整的"运行"(ActiveRun):
    /// 单线程结构保证同一 Agent 同时最多一个运行;运行期间到达的命令由
    /// [`Agent::drain_inbox`] 在检查点显式分类,不靠 mpsc 排队时机隐式决定。
    fn run_turn(
        &mut self,
        input: String,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
        inbox: Option<&Receiver<AgentCommand>>,
    ) {
        let mut queues = RunQueues::default();
        emit(AgentEvent::UserMessage(input.clone()));
        let prompt_hooks = self
            .hooks
            .run_user_prompt(&input, self.sessions.current_id());
        emit_hook_warnings(prompt_hooks.warnings, emit);
        let mut submitted = vec![SessionEntryPayload::message(
            ChatMessage::user_text(input),
            None,
        )];
        for message in prompt_hooks.added_context {
            submitted.push(SessionEntryPayload::message(message, None));
        }
        if !self.commit(submitted, emit) {
            self.finish_run(inbox, queues, false, emit);
            return;
        }
        emit(AgentEvent::TurnStarted);
        if let Some(reason) = prompt_hooks.block {
            emit(AgentEvent::Error(reason));
            self.finish_run(inbox, queues, false, emit);
            return;
        }

        let mut specs: Vec<ToolSpec> = self.tools.specs();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        let mut stop_hook_active = false;
        for _round in 0..self.config.max_turns {
            if cancel.load(Ordering::Relaxed) {
                self.finish_run(inbox, queues, true, emit);
                return;
            }

            // ---- 1. 投影 + 预算 + 调模型(带"未开播才重试"的重试) ----
            let prompt = {
                let mut prompt = self.build_system_prompt();
                let Some(messages) = self.project_for_model(&prompt, &specs, emit) else {
                    self.finish_run(inbox, queues, false, emit);
                    return;
                };
                prompt.messages = messages;
                prompt
            };
            let output = match self.call_model(&prompt, &specs, true, emit, cancel) {
                CallResult::Cancelled(failed) => {
                    emit(AgentEvent::Error(failed.error.to_string()));
                    // 半截 assistant 输出直接丢弃:历史停在 user 消息上,仍然合法
                    self.finish_run(inbox, queues, true, emit);
                    return;
                }
                CallResult::Failed(failed) => {
                    emit(AgentEvent::Error(failed.error.to_string()));
                    self.finish_run(inbox, queues, false, emit);
                    return;
                }
                CallResult::Done(out) => out,
            };

            self.usage_total.add(output.usage);
            emit(AgentEvent::Usage {
                input_tokens: self.usage_total.input_tokens,
                output_tokens: self.usage_total.output_tokens,
                cache: self.usage_total.cache,
            });

            // ---- 2. assistant 消息成为事实(携带本次真实 usage) ----
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
            if let Some((id, name, _)) = calls
                .iter()
                .find(|(id, name, _)| id.trim().is_empty() || name.trim().is_empty())
            {
                emit(AgentEvent::Error(format!(
                    "模型返回了无效工具调用(id={:?},name={:?});\
                     本次 assistant 消息未写入历史，会话仍可继续",
                    id, name
                )));
                self.finish_run(inbox, queues, false, emit);
                return;
            }
            let assistant_message = output.message;
            let assistant_payload = SessionEntryPayload::message_with_prompt(
                assistant_message.clone(),
                output.usage,
                output.prompt_fingerprint,
            );

            // ---- 3. 没有工具调用 → 当前任务将停止 ----
            if calls.is_empty() {
                if !stop_hook_active {
                    let stop = self
                        .hooks
                        .run_stop(&assistant_message, self.sessions.current_id());
                    emit_hook_warnings(stop.warnings, emit);
                    if let Some(reason) = stop.prevent_stop {
                        let continuation = ChatMessage::user_text(format!(
                            "[Stop Hook 要求继续] {}。请完成检查后给出最终答复。",
                            reason
                        ));
                        if !self.commit(
                            vec![
                                assistant_payload,
                                SessionEntryPayload::message(continuation, None),
                            ],
                            emit,
                        ) {
                            self.finish_run(inbox, queues, false, emit);
                            return;
                        }
                        emit(AgentEvent::Notice(reason));
                        stop_hook_active = true;
                        continue;
                    }
                }
                let mut payloads = vec![assistant_payload];
                if output.stop == StopReason::MaxTokens {
                    // UI-only 事实:提示截断,但绝不进入模型上下文。
                    payloads.push(SessionEntryPayload::Notice(NoticeRecord {
                        text: "输出撞到 max_tokens 上限,可能不完整".into(),
                        level: NoticeLevel::Warning,
                    }));
                }
                if !self.commit(payloads, emit) {
                    self.finish_run(inbox, queues, false, emit);
                    return;
                }
                if output.stop == StopReason::MaxTokens {
                    emit(AgentEvent::Notice(
                        "输出撞到 max_tokens 上限,可能不完整".into(),
                    ));
                }
                // steering 仍属于当前工作;follow-up 只在这里(任务将停止时)注入。
                self.drain_inbox(inbox, &mut queues, emit, cancel);
                let next = queues
                    .steering
                    .pop_front()
                    .or_else(|| queues.follow_up.pop_front());
                match next {
                    Some(text) if !cancel.load(Ordering::Relaxed) => {
                        if !self.inject_queued_input(text, emit) {
                            self.finish_run(inbox, queues, false, emit);
                            return;
                        }
                        continue;
                    }
                    _ => {
                        self.finish_run(inbox, queues, cancel.load(Ordering::Relaxed), emit);
                        return;
                    }
                }
            }

            // ---- 4. 执行工具批:preflight 按源顺序,执行可受控并发,
            //         结果(Observation)按 ToolUse 源顺序作为事实写回 ----
            let mut items: Vec<BatchItem> = Vec::with_capacity(calls.len());
            for (id, name, args) in calls {
                emit(AgentEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                    summary: util::args_summary(&args),
                });
                let truncated = output.stop == StopReason::MaxTokens;
                let item = self.preflight_tool_call(id, name, &args, truncated, emit, cancel);
                if let BatchItemState::Settled(outcome) = &item.state {
                    // preflight 定案(校验失败/拒绝/截断):立即闭合该调用的事件。
                    emit(AgentEvent::ToolCallFinished {
                        id: item.id.clone(),
                        name: item.name.clone(),
                        output: outcome.output.clone(),
                        error: outcome.error.clone(),
                    });
                }
                items.push(item);
            }
            self.execute_tool_batch(&mut items, emit, cancel);

            let mut was_cancelled = false;
            let mut stop_after_commit = None;
            let mut results: Vec<Block> = Vec::with_capacity(items.len());
            for item in items {
                let outcome = item.outcome.unwrap_or_else(|| {
                    // 防御:执行器保证每个调用都有结果;若缺失,补错误而不是丢配对。
                    ToolOutcome::failure(ToolError::new(
                        ToolErrorCode::Internal,
                        "[内部错误:工具执行器未产生结果]",
                    ))
                });
                if stop_after_commit.is_none() {
                    stop_after_commit = item.hook_stop;
                }
                was_cancelled |= outcome
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == ToolErrorCode::Aborted);
                results.push(Block::ToolResult {
                    tool_use_id: item.id,
                    content: outcome.output.model_text,
                    is_error: outcome.error.is_some(),
                });
            }
            was_cancelled |= cancel.load(Ordering::Relaxed);
            let result_message = ChatMessage {
                role: Role::User,
                blocks: results,
            };
            // ToolUse 与所有 ToolResult 必须原子落库,否则崩溃恢复后历史会非法。
            if !self.commit(
                vec![
                    assistant_payload,
                    SessionEntryPayload::message(result_message, None),
                ],
                emit,
            ) {
                self.finish_run(inbox, queues, was_cancelled, emit);
                return;
            }
            if was_cancelled {
                self.finish_run(inbox, queues, true, emit);
                return;
            }
            if let Some(reason) = stop_after_commit {
                emit(AgentEvent::Notice(reason));
                self.finish_run(inbox, queues, false, emit);
                return;
            }
            // ---- 5. 完整工具批已提交:steering 的唯一注入点 ----
            // 不在单个工具之间注入:避免"模型要求写文件,用户中途改口,
            // 文件到底写没写"的隐式状态。紧急停止走取消。
            self.drain_inbox(inbox, &mut queues, emit, cancel);
            if !cancel.load(Ordering::Relaxed) {
                if let Some(text) = queues.steering.pop_front() {
                    if !self.inject_queued_input(text, emit) {
                        self.finish_run(inbox, queues, false, emit);
                        return;
                    }
                }
            }
            // 回到循环顶部,把 Observation(以及可能的 steering)喂给模型
        }

        emit(AgentEvent::Notice(format!(
            "连续调用模型达到上限({} 次),强制结束本轮;可直接输入\"继续\"接着跑",
            self.config.max_turns
        )));
        self.finish_run(inbox, queues, false, emit);
    }

    /// 排干命令通道,把活动运行期间到达的命令显式分类。
    /// 直接输入 → steering(附提示);Steer/FollowUp → 对应队列;
    /// Shutdown → 请求取消当前轮并延迟退出;其余命令延迟到本轮结束执行。
    fn drain_inbox(
        &mut self,
        inbox: Option<&Receiver<AgentCommand>>,
        queues: &mut RunQueues,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) {
        let Some(rx) = inbox else { return };
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                AgentCommand::UserInput(text) => {
                    emit(AgentEvent::Notice(
                        "当前轮进行中:该输入已按 steering 排队,将在本批工具完成后注入".into(),
                    ));
                    queues.steering.push_back(text);
                }
                AgentCommand::Steer(text) => queues.steering.push_back(text),
                AgentCommand::FollowUp(text) => queues.follow_up.push_back(text),
                AgentCommand::Shutdown => {
                    emit(AgentEvent::Notice("收到退出请求,正在结束当前轮…".into()));
                    cancel.store(true, Ordering::Relaxed);
                    self.deferred.push_back(AgentCommand::Shutdown);
                }
                other => self.deferred.push_back(other),
            }
        }
    }

    /// 注入一条排队输入(steering / follow-up):成为 Message 事实并回显。
    /// 有意不重跑 user-prompt hooks:排队输入属于当前运行的一部分,
    /// hooks 的"提交新 prompt"语义只对开启新运行的输入生效。
    fn inject_queued_input(&mut self, text: String, emit: &mut dyn FnMut(AgentEvent)) -> bool {
        emit(AgentEvent::UserMessage(text.clone()));
        self.commit(
            vec![SessionEntryPayload::message(
                ChatMessage::user_text(text),
                None,
            )],
            emit,
        )
    }

    /// 结束一次运行:取消时把通道里尚未取走的输入一并排干丢弃
    /// (取消清理队列;正常结束时留在通道里的输入会自然成为下一轮命令)。
    fn finish_run(
        &mut self,
        inbox: Option<&Receiver<AgentCommand>>,
        mut queues: RunQueues,
        cancelled: bool,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        if cancelled {
            if let Some(rx) = inbox {
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        AgentCommand::UserInput(text) | AgentCommand::Steer(text) => {
                            queues.steering.push_back(text)
                        }
                        AgentCommand::FollowUp(text) => queues.follow_up.push_back(text),
                        AgentCommand::Shutdown => self.deferred.push_back(AgentCommand::Shutdown),
                        other => self.deferred.push_back(other),
                    }
                }
            }
        }
        let dropped = queues.steering.len() + queues.follow_up.len();
        if dropped > 0 {
            emit(AgentEvent::Notice(format!(
                "本轮结束,已丢弃 {} 条未注入的排队输入",
                dropped
            )));
        }
        emit(AgentEvent::TurnFinished { cancelled });
    }

    /// 供宿主循环在一次命令处理后取走"延迟命令"逐条执行。
    pub fn take_deferred(&mut self) -> Option<AgentCommand> {
        self.deferred.pop_front()
    }

    /// /compact:调模型生成摘要,追加 Compaction 事实。
    /// 事实条数只增不减;此后模型视图从摘要开始,旧事实仍在日志与 UI 中。
    ///
    /// 压缩请求**不复用结构化历史**,而是把模型视图渲染成纯文本对话记录、
    /// 单条 user 消息发出:摘要调用声明零工具,但历史里带着
    /// ToolUse/ToolResult 块与厂商 reasoning 回传项——这种"有 tool 块却无
    /// tools 声明"的请求形状在 Anthropic 上是显式 400,在 OpenAI 兼容后端/
    /// 网关上也常被拒(表现为 502)。对话在这里是被摘要的**数据**,不是要
    /// 续写的上下文,纯文本才是对两种 API 都合法的形状。
    fn compact(&mut self, emit: &mut dyn FnMut(AgentEvent), cancel: &AtomicBool) {
        let projection = project_model_messages(&self.entries);
        let transcript = render_transcript_for_compaction(&projection.messages);
        if transcript.is_empty() {
            emit(AgentEvent::Notice("当前会话没有可压缩的历史".into()));
            return;
        }
        emit(AgentEvent::TurnStarted);
        let tokens_before = estimate_tokens(0, 0, &projection);
        let mut request_text = format!(
            "以下是一段完整的对话记录:\n\n{}\n\n请压缩以上对话。直接输出摘要正文。",
            transcript
        );
        // 压缩请求自身也要守预算:超长时折叠中段,保住开头(目标)与结尾(现状)。
        if let Some(window) = self.budget.context_window {
            let available = window.saturating_sub(self.budget.reserve_output).max(1);
            let max_chars = (available as usize).saturating_mul(3); // ~4 字符/token,留余量
            request_text = util::truncate_middle(&request_text, max_chars);
        }
        let mut prompt = PromptContext::default();
        prompt
            .system_sections
            .push(COMPACTION_SYSTEM_PROMPT.to_string());
        prompt.messages = vec![ChatMessage::user_text(request_text)];
        // 压缩调用不提供工具、不把流式增量当作助手正文转发(它不是对话内容)。
        match self.call_model(&prompt, &[], false, emit, cancel) {
            CallResult::Done(output) => {
                let summary = output.message.text().trim().to_string();
                self.usage_total.add(output.usage);
                emit(AgentEvent::Usage {
                    input_tokens: self.usage_total.input_tokens,
                    output_tokens: self.usage_total.output_tokens,
                    cache: self.usage_total.cache,
                });
                if summary.is_empty() {
                    emit(AgentEvent::Error("压缩失败:模型返回了空摘要".into()));
                    emit(AgentEvent::TurnFinished { cancelled: false });
                    return;
                }
                let committed = self.commit(
                    vec![SessionEntryPayload::Compaction(CompactionRecord {
                        summary: summary.clone(),
                        tokens_before,
                    })],
                    emit,
                );
                if committed {
                    emit(AgentEvent::Notice(format!(
                        "历史已压缩:压缩前估算约 {} tokens,摘要 {} 字符。\
                         事实日志保留全部原始记录。",
                        tokens_before,
                        summary.chars().count()
                    )));
                }
                emit(AgentEvent::TurnFinished { cancelled: false });
            }
            CallResult::Cancelled(_) => {
                emit(AgentEvent::Notice("压缩已取消,历史未变化".into()));
                emit(AgentEvent::TurnFinished { cancelled: true });
            }
            CallResult::Failed(failed) => {
                emit(AgentEvent::Error(format!(
                    "压缩失败,历史未变化: {}",
                    failed.error
                )));
                emit(AgentEvent::TurnFinished { cancelled: false });
            }
        }
    }

    /// 单个调用的 preflight:兼容转换 → schema 校验 → hard deny → pre hook
    /// → 权限复核(Ask 在此阻塞等待审批)。按源顺序在 Runtime 线程执行,
    /// 任何失败都在这里定案为错误结果,绝不进入执行阶段。
    fn preflight_tool_call(
        &mut self,
        id: String,
        name: String,
        arguments: &serde_json::Value,
        truncated: bool,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> BatchItem {
        let settled = |outcome: ToolOutcome| BatchItem {
            id: id.clone(),
            name: name.clone(),
            state: BatchItemState::Settled(normalize_outcome(outcome.clone())),
            outcome: Some(normalize_outcome(outcome)),
            hook_stop: None,
        };

        // `length` 截断的参数可能"语法合法但语义不完整",整批一个都不执行。
        if truncated {
            return settled(ToolOutcome::failure(ToolError::new(
                ToolErrorCode::TruncatedInput,
                "模型输出达到 token 上限，工具参数可能不完整；本次未执行，请重新发起完整工具调用",
            )));
        }

        let mut prepared = match self.tools.prepare(&name, arguments) {
            Ok(prepared) => prepared,
            Err(error) => return settled(ToolOutcome::failure(error)),
        };

        // Hook 之前先执行一次完整策略检查，hard deny 在这里不可逆地终止调用。
        if let PermissionDecision::Deny { reason } =
            self.permissions.evaluate(&prepared, &self.workspace)
        {
            return settled(permission_denied(reason));
        }

        let pre = self.hooks.run_pre_tool(
            &prepared.spec,
            &prepared.arguments,
            self.sessions.current_id(),
        );
        emit_hook_warnings(pre.warnings, emit);
        if let Some(reason) = pre.block {
            return settled(ToolOutcome::failure(ToolError::new(
                ToolErrorCode::HookRejected,
                reason,
            )));
        }

        if pre.arguments != prepared.arguments {
            prepared = match self.tools.prepare(&name, &pre.arguments) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return settled(ToolOutcome::failure(ToolError {
                        code: ToolErrorCode::HookRejected,
                        message: format!("Hook 改写后的参数未通过 preflight: {}", error.message),
                        retryable: false,
                        details: error.details,
                    }))
                }
            };
        }

        match self.permissions.evaluate(&prepared, &self.workspace) {
            PermissionDecision::Allow => {}
            PermissionDecision::Deny { reason } => {
                return settled(permission_denied(reason));
            }
            PermissionDecision::Ask { reason, scopes } => {
                if let Err(error) = self.request_approval(&prepared, reason, scopes, emit, cancel) {
                    return settled(ToolOutcome::failure(error));
                }
            }
        }

        BatchItem {
            id,
            name,
            state: BatchItemState::Ready(prepared),
            outcome: None,
            hook_stop: None,
        }
    }

    /// 执行一批已 preflight 的调用。
    ///
    /// 调度规则(与 Pi 一致的保守策略):
    /// - 全部 Ready 调用都是 ParallelSafe 且多于一个 → 受上限并发;
    ///   任一 Sequential 工具使整批退回串行(cap=1,按源顺序启动)。
    /// - `ToolCallUpdated`/`ToolCallFinished` 按**完成顺序**发出(UI 及时);
    ///   历史 ToolResult 由调用方按**源顺序**组装(相同输入产生相同 prompt)。
    /// - 全局取消传播到每个调用的组合标志;尚未启动的调用直接定案为取消,
    ///   每个 ToolUse 无论如何都有配对结果。
    /// - 配置了工具超时的调用逾期后置组合标志;工具因此中止的结果被改写为
    ///   Timeout。工具无视标志坚持完成的,保留其真实结果(副作用已发生)。
    /// - settle 之后到达的迟到进度被忽略;post hook 在协调线程按完成顺序运行。
    fn execute_tool_batch(
        &mut self,
        items: &mut [BatchItem],
        emit: &mut dyn FnMut(AgentEvent),
        global_cancel: &AtomicBool,
    ) {
        const PARALLEL_TOOL_LIMIT: usize = 4;
        let ready: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item.state, BatchItemState::Ready(_)))
            .map(|(index, _)| index)
            .collect();
        if ready.is_empty() {
            return;
        }
        let all_parallel_safe = ready.iter().all(|&index| {
            let BatchItemState::Ready(prepared) = &items[index].state else {
                return false;
            };
            prepared.spec.capabilities.execution_mode
                == crate::tools::ToolExecutionMode::ParallelSafe
        });
        let cap = if all_parallel_safe && ready.len() > 1 {
            PARALLEL_TOOL_LIMIT.min(ready.len())
        } else {
            1
        };
        let timeout = self.config.tool_timeout;
        let session_id = self.sessions.current_id().to_string();
        // 字段级拆分借用:worker 只读 tools/workspace,协调侧独占 hooks。
        let tools = &self.tools;
        let workspace = &self.workspace;
        let hooks = &mut self.hooks;

        enum WorkerMsg {
            Progress { slot: usize, output: ToolOutput },
            Done { slot: usize, outcome: ToolOutcome },
        }

        let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
        let flags: Vec<Arc<AtomicBool>> = ready
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let mut deadlines: Vec<Option<std::time::Instant>> = vec![None; ready.len()];
        let mut settled: Vec<bool> = vec![false; ready.len()];

        std::thread::scope(|scope| {
            let mut next = 0usize;
            let mut running = 0usize;
            let mut completed = 0usize;
            let mut cancel_propagated = false;
            while completed < ready.len() {
                // 全局取消 → 一次性传播到所有已启动调用的组合标志。
                if !cancel_propagated && global_cancel.load(Ordering::Relaxed) {
                    for flag in &flags {
                        flag.store(true, Ordering::Relaxed);
                    }
                    cancel_propagated = true;
                }
                // 超时检查:逾期调用置组合标志(工具在下一个检查点中止)。
                let now = std::time::Instant::now();
                for slot in 0..ready.len() {
                    if settled[slot] {
                        continue;
                    }
                    if deadlines[slot].is_some_and(|deadline| now >= deadline) {
                        flags[slot].store(true, Ordering::Relaxed);
                    }
                }
                // 启动新 worker;取消后不再启动,直接定案为取消结果。
                while running < cap && next < ready.len() {
                    let slot = next;
                    next += 1;
                    let item_index = ready[slot];
                    if global_cancel.load(Ordering::Relaxed) {
                        let outcome = ToolOutcome::failure(ToolError::new(
                            ToolErrorCode::Aborted,
                            "[用户取消,本工具未执行]",
                        ));
                        emit(AgentEvent::ToolCallFinished {
                            id: items[item_index].id.clone(),
                            name: items[item_index].name.clone(),
                            output: outcome.output.clone(),
                            error: outcome.error.clone(),
                        });
                        items[item_index].outcome = Some(outcome);
                        settled[slot] = true;
                        completed += 1;
                        continue;
                    }
                    let BatchItemState::Ready(prepared) = &items[item_index].state else {
                        unreachable!("ready 列表只含 Ready 项");
                    };
                    let prepared = prepared.clone();
                    let flag = Arc::clone(&flags[slot]);
                    let progress_tx = tx.clone();
                    let done_tx = tx.clone();
                    let sid = session_id.clone();
                    deadlines[slot] = timeout.map(|limit| std::time::Instant::now() + limit);
                    scope.spawn(move || {
                        let mut progress = move |update: ToolOutput| {
                            let _ = progress_tx.send(WorkerMsg::Progress {
                                slot,
                                output: update,
                            });
                        };
                        let mut ctx = ToolContext {
                            workspace,
                            cancel: &flag,
                            session_id: &sid,
                            progress: &mut progress,
                        };
                        let outcome = tools.execute_prepared(&prepared, &mut ctx);
                        let _ = done_tx.send(WorkerMsg::Done { slot, outcome });
                    });
                    running += 1;
                }
                if running == 0 {
                    // 剩余项都在取消分支同步定案了。
                    continue;
                }
                match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(WorkerMsg::Progress { slot, output }) => {
                        // settle 之后的迟到进度被忽略。
                        if !settled[slot] {
                            let item = &items[ready[slot]];
                            emit(AgentEvent::ToolCallUpdated {
                                id: item.id.clone(),
                                name: item.name.clone(),
                                output,
                            });
                        }
                    }
                    Ok(WorkerMsg::Done { slot, mut outcome }) => {
                        running -= 1;
                        completed += 1;
                        settled[slot] = true;
                        // 因超时标志而中止的结果改写为 Timeout,与用户取消区分。
                        let timed_out = deadlines[slot]
                            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                            && !global_cancel.load(Ordering::Relaxed);
                        if timed_out {
                            if let Some(error) = outcome.error.as_mut() {
                                if error.code == ToolErrorCode::Aborted {
                                    error.code = ToolErrorCode::Timeout;
                                    error.message = format!(
                                        "工具执行超过 {:.0}s 上限被中止: {}",
                                        timeout.unwrap_or_default().as_secs_f64(),
                                        error.message
                                    );
                                    outcome.output.model_text = error.message.clone();
                                }
                            }
                        }
                        let item_index = ready[slot];
                        let (post_outcome, hook_stop) = {
                            let BatchItemState::Ready(prepared) = &items[item_index].state else {
                                unreachable!("ready 列表只含 Ready 项");
                            };
                            let post = hooks.run_post_tool(
                                &prepared.spec,
                                &prepared.arguments,
                                outcome,
                                &session_id,
                            );
                            emit_hook_warnings(post.warnings, emit);
                            (normalize_outcome(post.outcome), post.stop_after_commit)
                        };
                        emit(AgentEvent::ToolCallFinished {
                            id: items[item_index].id.clone(),
                            name: items[item_index].name.clone(),
                            output: post_outcome.output.clone(),
                            error: post_outcome.error.clone(),
                        });
                        items[item_index].outcome = Some(post_outcome);
                        items[item_index].hook_stop = hook_stop;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
    }

    fn request_approval(
        &mut self,
        prepared: &PreparedToolCall,
        reason: String,
        scopes: Vec<ApprovalScope>,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> Result<(), ToolError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = ApprovalRequest {
            request_id: request_id.clone(),
            tool: prepared.spec.name.clone(),
            summary: util::args_summary(&prepared.arguments),
            reason,
            scopes,
        };
        emit(AgentEvent::PermissionRequested { request });

        let Some(receiver) = self.approval_rx.as_ref() else {
            emit(AgentEvent::PermissionResolved {
                request_id,
                allowed: false,
            });
            return Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                "当前前端不支持交互审批；请在 [permissions] 中显式 allow 或改用 TUI",
            ));
        };

        loop {
            if cancel.load(Ordering::Relaxed) {
                emit(AgentEvent::PermissionResolved {
                    request_id,
                    allowed: false,
                });
                return Err(ToolError::new(
                    ToolErrorCode::Aborted,
                    "等待审批时被用户取消",
                ));
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(response) if response.request_id != request_id => {
                    emit(AgentEvent::Notice(format!(
                        "忽略过期审批响应 {}",
                        response.request_id
                    )));
                }
                Ok(response) => match response.decision {
                    ApprovalDecision::Allow(scope) => {
                        if scope == ApprovalScope::Session {
                            self.permissions.remember_session_grant(prepared);
                        }
                        emit(AgentEvent::PermissionResolved {
                            request_id,
                            allowed: true,
                        });
                        return Ok(());
                    }
                    ApprovalDecision::Deny => {
                        emit(AgentEvent::PermissionResolved {
                            request_id,
                            allowed: false,
                        });
                        return Err(ToolError::new(
                            ToolErrorCode::PermissionDenied,
                            "用户拒绝了本次工具调用",
                        ));
                    }
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    emit(AgentEvent::PermissionResolved {
                        request_id,
                        allowed: false,
                    });
                    return Err(ToolError::new(
                        ToolErrorCode::PermissionDenied,
                        "审批通道已关闭，本次工具未执行",
                    ));
                }
            }
        }
    }

    /// 单次模型调用 + 重试策略。`forward_stream` 为 false 时不把流式增量
    /// 转发成对话事件(用于压缩这类"非对话"调用)。
    fn call_model(
        &self,
        prompt: &PromptContext,
        specs: &[ToolSpec],
        forward_stream: bool,
        emit: &mut dyn FnMut(AgentEvent),
        cancel: &AtomicBool,
    ) -> CallResult {
        let mut attempt = 1u32;
        loop {
            let mut emitted_any = false;
            let mut forward = |pe: ProviderEvent| {
                emitted_any = true;
                if !forward_stream {
                    return;
                }
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
                StreamTerminal::Done(out) => return CallResult::Done(out),
                StreamTerminal::Aborted(failed) => return CallResult::Cancelled(failed),
                StreamTerminal::Error(failed) => {
                    // 重试幂等:只有一个流事件都没产生的失败才可重播。
                    let delay = if failed.error.retryable && !emitted_any {
                        self.retry_policy
                            .delay_for(attempt, failed.error.retry_after)
                    } else {
                        None
                    };
                    let Some(wait) = delay else {
                        return CallResult::Failed(failed);
                    };
                    emit(AgentEvent::Notice(format!(
                        "{},{:.1}s 后重试({}/{})",
                        failed.error,
                        wait.as_secs_f64(),
                        attempt,
                        self.retry_policy.max_attempts - 1
                    )));
                    // 分片睡眠,期间可被取消
                    let mut slept = Duration::ZERO;
                    while slept < wait {
                        if cancel.load(Ordering::Relaxed) {
                            return CallResult::Cancelled(FailedTurn::aborted());
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
    Cancelled(FailedTurn),
    Failed(FailedTurn),
}

/// 一次运行的两个输入队列。语义不同,不能合并:
/// steering 改变正在进行的任务(完整工具批后注入),
/// follow-up 等当前任务将停止时才注入(one-at-a-time,每检查点取最老一条)。
#[derive(Default)]
struct RunQueues {
    steering: std::collections::VecDeque<String>,
    follow_up: std::collections::VecDeque<String>,
}

/// 工具批中的一个调用。preflight 后要么 Ready(可执行),
/// 要么 Settled(已定案为错误/拒绝,绝不执行);执行完成后 outcome 必定非空。
struct BatchItem {
    id: String,
    name: String,
    state: BatchItemState,
    outcome: Option<ToolOutcome>,
    hook_stop: Option<String>,
}

enum BatchItemState {
    Ready(PreparedToolCall),
    Settled(ToolOutcome),
}

fn budget_from_settings(settings: &crate::config::ProviderSettings) -> ContextBudget {
    ContextBudget {
        context_window: settings.context_window,
        // 输出预留:显式 max_tokens,否则一个保守默认。
        reserve_output: settings.max_tokens.unwrap_or(8192),
    }
}

/// 把模型视图渲染成纯文本对话记录(压缩请求专用)。
/// 工具调用/结果转成文字描述,思考过程与厂商 reasoning 原始数据一律丢弃——
/// 它们对摘要没有价值,回传反而会造成跨请求的协议问题。
fn render_transcript_for_compaction(messages: &[ChatMessage]) -> String {
    /// 单个工具结果进入摘要输入的字符上限(保头保尾)。
    const TOOL_RESULT_CHARS: usize = 1_500;
    let mut out = String::new();
    for message in messages {
        let label = match message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
        };
        let mut body = String::new();
        for block in &message.blocks {
            match block {
                Block::Text(text) if !text.trim().is_empty() => {
                    body.push_str(text.trim_end());
                    body.push('\n');
                }
                Block::Text(_) | Block::Thinking { .. } => {}
                Block::ToolUse { name, input, .. } => {
                    body.push_str(&format!(
                        "[调用工具 {}({})]\n",
                        name,
                        util::args_summary(input)
                    ));
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    body.push_str(if *is_error {
                        "[工具失败] "
                    } else {
                        "[工具结果] "
                    });
                    body.push_str(&util::truncate_middle(content, TOOL_RESULT_CHARS));
                    body.push('\n');
                }
            }
        }
        let body = body.trim_end();
        if body.is_empty() {
            continue;
        }
        out.push_str(label);
        out.push_str(":\n");
        out.push_str(body);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// 工具声明进入请求体的近似字符成本(name + description + schema JSON)。
fn tool_spec_chars(specs: &[ToolSpec]) -> u64 {
    specs
        .iter()
        .map(|spec| {
            (spec.name.chars().count()
                + spec.description.chars().count()
                + spec.schema.to_string().chars().count()
                + 32) as u64
        })
        .sum()
}

fn permission_denied(reason: String) -> ToolOutcome {
    ToolOutcome::failure(ToolError::new(ToolErrorCode::PermissionDenied, reason))
}

fn emit_hook_warnings(warnings: Vec<String>, emit: &mut dyn FnMut(AgentEvent)) {
    for warning in warnings {
        emit(AgentEvent::Notice(warning));
    }
}

/// 前端持有的 Runtime 句柄。
pub struct RuntimeHandle {
    pub commands: Sender<AgentCommand>,
    pub approvals: Sender<ApprovalResponse>,
    pub events: Receiver<AgentEvent>,
    /// 置 true 请求取消当前轮;Runtime 会在收尾后自行复位。
    pub cancel: Arc<AtomicBool>,
    pub provider_label: String,
    pub provider_names: Vec<String>,
    /// 配置文件中出现过的模型，供 TUI 提供真实而有限的选择目录。
    pub model_names: Vec<String>,
    pub session_id: String,
}

/// 把 Agent 装进工作线程,返回通道句柄。TUI 前端用这个;
/// headless 前端不需要线程,直接调 `Agent::handle_command`。
pub fn spawn(agent: Agent) -> RuntimeHandle {
    let provider_label = agent.provider_label();
    let provider_names = agent.config.provider_names();
    let model_names = agent.config.model_names();
    let session_id = agent.session_id().to_string();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<AgentCommand>();
    let (approval_tx, approval_rx) = std::sync::mpsc::channel::<ApprovalResponse>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();

    std::thread::Builder::new()
        .name("agent-runtime".into())
        .spawn(move || {
            let mut agent = agent;
            agent.approval_rx = Some(approval_rx);
            let mut emit = |e: AgentEvent| {
                // 前端先退出时 send 会失败,忽略即可(线程随后收到 Shutdown 或通道关闭)
                let _ = evt_tx.send(e);
            };
            loop {
                // 活动运行中延迟的命令(/clear、/provider、Shutdown…)优先于新命令。
                let cmd = match agent.take_deferred() {
                    Some(cmd) => cmd,
                    None => match cmd_rx.recv() {
                        Ok(cmd) => cmd,
                        Err(_) => break,
                    },
                };
                // 新命令开始前复位取消标志(上一轮的取消不该波及这一轮)
                cancel_worker.store(false, Ordering::Relaxed);
                if !agent.handle_command_with_inbox(cmd, &mut emit, &cancel_worker, Some(&cmd_rx)) {
                    break;
                }
            }
        })
        .expect("无法创建 runtime 线程");

    RuntimeHandle {
        commands: cmd_tx,
        approvals: approval_tx,
        events: evt_rx,
        cancel,
        provider_label,
        provider_names,
        model_names,
        session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::hooks::{
        Hook, PreToolUseContext, PreToolUseHookResult, StopContext, StopHookResult,
    };
    use crate::permission::{PermissionRule, PermissionRules};
    use crate::provider::{ProviderError, StreamTerminal};
    use crate::tools::{Tool, ToolCapabilities, ToolOutput, ToolPermissionSpec};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    enum ScriptStep {
        Output(crate::provider::TurnOutput),
        Error(ProviderError),
        Cancel,
    }

    struct ScriptedProvider {
        steps: Mutex<VecDeque<ScriptStep>>,
        /// 每次 stream_turn 收到的消息视图,供测试断言"模型看到了什么"。
        prompts: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        model: String,
    }

    struct RuntimeProgressTool;

    struct CountedTool {
        executions: Arc<AtomicUsize>,
        capabilities: ToolCapabilities,
        permission: ToolPermissionSpec,
    }

    impl Tool for CountedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "counted".into(),
                description: "counted test tool".into(),
                schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
                capabilities: self.capabilities,
                permission: self.permission.clone(),
            }
        }

        fn execute(
            &self,
            _args: &serde_json::Value,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            self.executions.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput::text("executed"))
        }
    }

    struct ReplacePathHook {
        calls: Arc<AtomicUsize>,
        replacement: String,
    }

    struct PreventStopHook {
        calls: Arc<AtomicUsize>,
    }

    impl Hook for PreventStopHook {
        fn name(&self) -> &str {
            "verify_once"
        }

        fn stop(&mut self, _ctx: &StopContext<'_>) -> anyhow::Result<StopHookResult> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(StopHookResult::PreventStop("run verification".into()))
        }
    }

    impl Hook for ReplacePathHook {
        fn name(&self) -> &str {
            "replace_path"
        }

        fn pre_tool_use(
            &mut self,
            _ctx: &PreToolUseContext<'_>,
        ) -> anyhow::Result<PreToolUseHookResult> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(PreToolUseHookResult::ReplaceArguments(
                serde_json::json!({ "path": self.replacement }),
            ))
        }
    }

    impl Tool for RuntimeProgressTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "runtime_progress".into(),
                description: "runtime progress test tool".into(),
                schema: serde_json::json!({ "type": "object" }),
                capabilities: ToolCapabilities::READ_ONLY,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &serde_json::Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            ctx.report_progress(ToolOutput {
                model_text: "halfway".into(),
                ui_summary: Some("1/2".into()),
                details: Some(serde_json::json!({ "completed": 1, "total": 2 })),
            });
            Ok(ToolOutput::text("done"))
        }
    }

    impl ScriptedProvider {
        fn new(steps: Vec<ScriptStep>) -> Self {
            ScriptedProvider {
                steps: Mutex::new(steps.into()),
                prompts: Arc::new(Mutex::new(Vec::new())),
                model: "scripted".into(),
            }
        }

        fn with_prompt_log(
            steps: Vec<ScriptStep>,
            prompts: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        ) -> Self {
            ScriptedProvider {
                steps: Mutex::new(steps.into()),
                prompts,
                model: "scripted".into(),
            }
        }
    }

    impl Provider for ScriptedProvider {
        fn label(&self) -> String {
            format!("scripted / {}", self.model)
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn set_model(&mut self, model: String) {
            self.model = model;
        }

        fn stream_turn(
            &self,
            prompt: &PromptContext,
            _tools: &[ToolSpec],
            _on_event: &mut dyn FnMut(ProviderEvent),
            _cancel: &AtomicBool,
        ) -> StreamTerminal {
            self.prompts.lock().unwrap().push(prompt.messages.clone());
            match self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted")
            {
                ScriptStep::Output(output) => StreamTerminal::Done(output),
                ScriptStep::Error(error) => StreamTerminal::Error(FailedTurn::from_error(error)),
                ScriptStep::Cancel => StreamTerminal::Aborted(FailedTurn::aborted()),
            }
        }
    }

    fn config(root: &std::path::Path) -> Config {
        config_with_max_turns(root, 2)
    }

    fn config_with_max_turns(root: &std::path::Path, max_turns: u32) -> Config {
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[agent]
provider = "mock"
max_turns = {}

[providers.mock]
api = "responses"
base_url = "http://127.0.0.1:1"
model = "scripted"
api_key = ""
"#,
                max_turns
            ),
        )
        .unwrap();
        Config::load(&path).unwrap()
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "onemore-runtime-{}-{}-{}",
            name,
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn output(message: ChatMessage, stop: StopReason) -> crate::provider::TurnOutput {
        crate::provider::TurnOutput {
            message,
            usage: Usage::default(),
            stop,
            prompt_fingerprint: None,
        }
    }

    fn tool_turn(calls: Vec<(&str, serde_json::Value)>) -> crate::provider::TurnOutput {
        output(
            ChatMessage {
                role: Role::Assistant,
                blocks: calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, input))| Block::ToolUse {
                        id: format!("call-{}", index + 1),
                        name: name.into(),
                        input,
                    })
                    .collect(),
            },
            StopReason::ToolUse,
        )
    }

    fn install_counted_tool(
        agent: &mut Agent,
        capabilities: ToolCapabilities,
        permission: ToolPermissionSpec,
    ) -> Arc<AtomicUsize> {
        let executions = Arc::new(AtomicUsize::new(0));
        agent.tools = ToolRegistry::new(vec![Box::new(CountedTool {
            executions: Arc::clone(&executions),
            capabilities,
            permission,
        })]);
        executions
    }

    #[test]
    fn terminal_protocol_keeps_final_message_on_error() {
        let provider = ScriptedProvider::new(vec![ScriptStep::Error(ProviderError::fatal("boom"))]);
        let terminal = provider.stream_turn(
            &PromptContext::default(),
            &[],
            &mut |_| {},
            &AtomicBool::new(false),
        );
        match terminal {
            StreamTerminal::Error(failed) => {
                assert!(failed.message.role == Role::Assistant);
                assert_eq!(failed.error.message, "boom");
            }
            other => panic!("应得到 Error 终止，实际为 {:?}", other),
        }
    }

    #[test]
    fn runtime_emits_closed_error_and_abort_turns() {
        let root = temp_root("terminal");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Error(
            ProviderError::fatal("boom"),
        )]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("失败".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(text) if text == "boom")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { cancelled: false })));

        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data-abort"),
        )
        .unwrap();
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Cancel]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("取消".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { cancelled: true })));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn max_tokens_tool_calls_are_returned_as_errors_without_execution() {
        let root = temp_root("length");
        let target = root.join("should-not-exist.txt");
        let first = output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "call-1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "should-not-exist.txt",
                        "content": "unsafe partial output"
                    }),
                }],
            },
            StopReason::MaxTokens,
        );
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("写文件".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );
        assert!(!target.exists(), "length 截断的工具调用不应产生写入副作用");
        assert!(events
            .iter()
            .any(|event| { matches!(event, AgentEvent::ToolCallFinished { error: Some(_), .. }) }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tool_progress_is_mapped_to_runtime_event_before_finish() {
        let root = temp_root("progress");
        let first = output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "progress-1".into(),
                    name: "runtime_progress".into(),
                    input: serde_json::json!({}),
                }],
            },
            StopReason::ToolUse,
        );
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.tools = ToolRegistry::new(vec![Box::new(RuntimeProgressTool)]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("运行".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        let updated = events.iter().position(|event| {
            matches!(
                event,
                AgentEvent::ToolCallUpdated { id, name, output }
                    if id == "progress-1"
                        && name == "runtime_progress"
                        && output.ui_text() == "1/2"
            )
        });
        let finished = events.iter().position(|event| {
            matches!(
                event,
                AgentEvent::ToolCallFinished { id, .. } if id == "progress-1"
            )
        });
        assert!(updated.is_some(), "应收到结构化工具进度事件: {events:?}");
        assert!(
            updated.unwrap() < finished.expect("应收到工具完成事件"),
            "进度必须先于完成事件"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permission_deny_never_executes_and_still_finishes_tool_pair() {
        let root = temp_root("permission-deny");
        let first = tool_turn(vec![("counted", serde_json::json!({ "path": "inside" }))]);
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.permissions = PermissionManager::new(PermissionRules {
            workspace_write: PermissionRule::Deny,
            ..PermissionRules::default()
        });
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::MUTATION,
            ToolPermissionSpec::paths(&["path"]),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("deny".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        assert_eq!(executions.load(Ordering::Relaxed), 0);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallFinished {
                error: Some(ToolError {
                    code: ToolErrorCode::PermissionDenied,
                    ..
                }),
                ..
            }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_rejection_is_a_tool_result_not_a_runtime_failure() {
        let root = temp_root("approval-deny");
        let first = tool_turn(vec![("counted", serde_json::json!({ "path": "opaque" }))]);
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::COMMAND,
            ToolPermissionSpec::opaque_side_effect(&[]),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let (approval_tx, approval_rx) = std::sync::mpsc::channel();
        agent.approval_rx = Some(approval_rx);
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("ask".into()),
            &mut |event| {
                if let AgentEvent::PermissionRequested { request } = &event {
                    approval_tx
                        .send(ApprovalResponse {
                            request_id: request.request_id.clone(),
                            decision: ApprovalDecision::Deny,
                        })
                        .unwrap();
                }
                events.push(event);
            },
            &AtomicBool::new(false),
        );

        assert_eq!(executions.load(Ordering::Relaxed), 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionResolved { allowed: false, .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallFinished {
                error: Some(ToolError {
                    code: ToolErrorCode::PermissionDenied,
                    ..
                }),
                ..
            }
        )));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_approval_only_skips_identical_following_call() {
        let root = temp_root("approval-session");
        let first = tool_turn(vec![
            ("counted", serde_json::json!({ "path": "same" })),
            ("counted", serde_json::json!({ "path": "same" })),
        ]);
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::COMMAND,
            ToolPermissionSpec::opaque_side_effect(&[]),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let (approval_tx, approval_rx) = std::sync::mpsc::channel();
        agent.approval_rx = Some(approval_rx);
        let mut requests = 0;
        agent.handle_command(
            AgentCommand::UserInput("session grant".into()),
            &mut |event| {
                if let AgentEvent::PermissionRequested { request } = event {
                    requests += 1;
                    approval_tx
                        .send(ApprovalResponse {
                            request_id: request.request_id,
                            decision: ApprovalDecision::Allow(ApprovalScope::Session),
                        })
                        .unwrap();
                }
            },
            &AtomicBool::new(false),
        );

        assert_eq!(requests, 1);
        assert_eq!(executions.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelling_an_approval_wait_aborts_without_executing() {
        let root = temp_root("approval-cancel");
        let first = tool_turn(vec![("counted", serde_json::json!({ "path": "opaque" }))]);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::COMMAND,
            ToolPermissionSpec::opaque_side_effect(&[]),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(first)]));
        let (_approval_tx, approval_rx) = std::sync::mpsc::channel();
        agent.approval_rx = Some(approval_rx);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("cancel approval".into()),
            &mut |event| {
                if matches!(event, AgentEvent::PermissionRequested { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
                events.push(event);
            },
            &cancel,
        );

        assert_eq!(executions.load(Ordering::Relaxed), 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: true })));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallFinished {
                error: Some(ToolError {
                    code: ToolErrorCode::Aborted,
                    ..
                }),
                ..
            }
        )));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hook_replacement_is_revalidated_and_rechecked_by_permission() {
        let root = temp_root("hook-recheck");
        let outside = temp_root("hook-outside").join("target.txt");
        let first = tool_turn(vec![("counted", serde_json::json!({ "path": "inside" }))]);
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.permissions = PermissionManager::new(PermissionRules {
            outside_workspace: PermissionRule::Deny,
            ..PermissionRules::default()
        });
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::MUTATION,
            ToolPermissionSpec::paths(&["path"]),
        );
        let hook_calls = Arc::new(AtomicUsize::new(0));
        agent.hooks = HookRegistry::new(vec![Box::new(ReplacePathHook {
            calls: Arc::clone(&hook_calls),
            replacement: outside.to_string_lossy().into_owned(),
        })]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        agent.handle_command(
            AgentCommand::UserInput("hook".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );

        assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
        assert_eq!(executions.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn hard_deny_runs_before_pre_tool_hook() {
        let root = temp_root("hard-deny-hook");
        let first = tool_turn(vec![("counted", serde_json::json!({ "path": "NUL.txt" }))]);
        let second = output(ChatMessage::empty_assistant(), StopReason::EndTurn);
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let executions = install_counted_tool(
            &mut agent,
            ToolCapabilities::MUTATION,
            ToolPermissionSpec::paths(&["path"]),
        );
        let hook_calls = Arc::new(AtomicUsize::new(0));
        agent.hooks = HookRegistry::new(vec![Box::new(ReplacePathHook {
            calls: Arc::clone(&hook_calls),
            replacement: "safe.txt".into(),
        })]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        agent.handle_command(
            AgentCommand::UserInput("hard deny".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );

        assert_eq!(hook_calls.load(Ordering::Relaxed), 0);
        assert_eq!(executions.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stop_hook_can_prevent_stop_only_once_per_run() {
        let root = temp_root("stop-hook");
        let first = output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text("first".into())],
            },
            StopReason::EndTurn,
        );
        let second = output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text("verified".into())],
            },
            StopReason::EndTurn,
        );
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        agent.hooks = HookRegistry::new(vec![Box::new(PreventStopHook {
            calls: Arc::clone(&calls),
        })]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(first),
            ScriptStep::Output(second),
        ]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("verify".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::AssistantMessage(text) if text == "verified")
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
        let _ = std::fs::remove_dir_all(root);
    }

    // ---- 阶段 4:事实日志、模型视图与预算 ----

    #[test]
    fn assistant_usage_is_recorded_as_fact_and_seeds_baseline() {
        let root = temp_root("usage-fact");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(
            crate::provider::TurnOutput {
                message: ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::Text("答复".into())],
                },
                usage: Usage {
                    input_tokens: 1200,
                    output_tokens: 34,
                    cache: None,
                },
                stop: StopReason::EndTurn,
                prompt_fingerprint: Some("sha256:test".into()),
            },
        )]));
        agent.handle_command(
            AgentCommand::UserInput("问题".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );

        let assistant = agent
            .entries
            .iter()
            .find_map(|entry| match &entry.payload {
                SessionEntryPayload::Message(record) if record.message.role == Role::Assistant => {
                    Some(record.clone())
                }
                _ => None,
            })
            .expect("assistant 应成为 Message 事实");
        assert_eq!(
            assistant.usage,
            Some(Usage {
                input_tokens: 1200,
                output_tokens: 34,
                cache: None,
            }),
            "事实必须携带该次调用的真实 usage"
        );
        assert_eq!(assistant.prompt_fingerprint.as_deref(), Some("sha256:test"));
        let projection = project_model_messages(&agent.entries);
        assert_eq!(projection.known_token_baseline, Some(1234));
        assert_eq!(projection.tail_chars_after_baseline, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn over_budget_refuses_to_call_provider() {
        let root = temp_root("budget-refuse");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        // 空脚本:任何 provider 调用都会 panic("script exhausted"),
        // 由此证明拒绝发生在请求发出之前。
        agent.provider = Box::new(ScriptedProvider::new(Vec::new()));
        agent.budget = ContextBudget {
            context_window: Some(100),
            reserve_output: 50,
        };
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("长输入".repeat(2000)),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error(text) if text.contains("/compact"))));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
        // 用户消息仍然是事实(拒绝的是本次请求,不是用户输入)。
        assert_eq!(agent.entries.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compact_appends_fact_and_shrinks_model_view() {
        let root = temp_root("compact");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        // 先跑一轮正常对话形成历史。
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text("旧回答".into())],
            },
            StopReason::EndTurn,
        ))]));
        agent.handle_command(
            AgentCommand::UserInput("旧问题".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );
        let facts_before = agent.entries.len();

        // /compact:模型返回摘要。
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(output(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text("摘要:讨论了旧问题".into())],
            },
            StopReason::EndTurn,
        ))]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::Compact,
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        // 压缩后事实条数不减少(只增加 Compaction)。
        assert_eq!(agent.entries.len(), facts_before + 1);
        assert!(matches!(
            agent.entries.last().unwrap().payload,
            SessionEntryPayload::Compaction(_)
        ));
        // 模型视图缩小为"摘要"一条。
        let projection = project_model_messages(&agent.entries);
        assert_eq!(projection.messages.len(), 1);
        assert!(projection.messages[0].text().contains("摘要:讨论了旧问题"));
        // 压缩期间不把摘要当对话正文流出。
        assert!(!events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantDelta(_))));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice(text) if text.contains("历史已压缩"))));
        let _ = std::fs::remove_dir_all(root);
    }

    /// /compact 的请求形状回归:历史里有工具往返时,压缩请求必须是
    /// 纯文本单条 user 消息——零工具请求携带 ToolUse/ToolResult 块在
    /// Anthropic 上是 400,在 OpenAI 兼容网关上常表现为 502。
    #[test]
    fn compact_request_is_plain_text_without_tool_blocks() {
        let root = temp_root("compact-plain");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        install_counted_tool(
            &mut agent,
            ToolCapabilities::READ_ONLY,
            ToolPermissionSpec::default(),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(tool_turn(vec![(
                "counted",
                serde_json::json!({ "path": "a" }),
            )])),
            ScriptStep::Output(output(
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::Text("做完了".into())],
                },
                StopReason::EndTurn,
            )),
        ]));
        agent.handle_command(
            AgentCommand::UserInput("做点事".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );

        let prompts = Arc::new(Mutex::new(Vec::new()));
        agent.provider = Box::new(ScriptedProvider::with_prompt_log(
            vec![ScriptStep::Output(output(
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::Text("摘要".into())],
                },
                StopReason::EndTurn,
            ))],
            Arc::clone(&prompts),
        ));
        agent.handle_command(AgentCommand::Compact, &mut |_| {}, &AtomicBool::new(false));

        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        let request = &prompts[0];
        assert_eq!(request.len(), 1, "压缩请求应是单条消息: {request:?}");
        assert_eq!(request[0].role, Role::User);
        assert!(
            request[0]
                .blocks
                .iter()
                .all(|block| matches!(block, Block::Text(_))),
            "压缩请求不得携带结构化工具/思考块: {request:?}"
        );
        let text = request[0].text();
        assert!(text.contains("counted"), "工具调用应以文字保留: {text}");
        assert!(text.contains("executed"), "工具结果应以文字保留: {text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_restore_returns_full_facts_including_ui_only() {
        let root = temp_root("restore-facts");
        let data_dir = root.join("data");
        let session_id;
        {
            let mut agent = Agent::new_with_data_dir(
                config(&root),
                Workspace::new(root.clone()),
                data_dir.clone(),
            )
            .unwrap();
            session_id = agent.session_id().to_string();
            // MaxTokens 且无工具调用 → assistant 事实 + UI-only Notice 事实同批落库。
            agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(output(
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::Text("被截断的回答".into())],
                },
                StopReason::MaxTokens,
            ))]));
            agent.handle_command(
                AgentCommand::UserInput("问题".into()),
                &mut |_| {},
                &AtomicBool::new(false),
            );
        }

        let mut second =
            Agent::new_with_data_dir(config(&root), Workspace::new(root.clone()), data_dir)
                .unwrap();
        let mut events = Vec::new();
        second.handle_command(
            AgentCommand::LoadSession(session_id.clone()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );
        let entries = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::SessionLoaded { id, entries, .. } if *id == session_id => {
                    Some(entries.clone())
                }
                _ => None,
            })
            .expect("应恢复目标会话");
        assert_eq!(entries.len(), 3, "user + assistant + notice 三条事实");
        assert!(entries
            .iter()
            .any(|entry| matches!(entry.payload, SessionEntryPayload::Notice(_))));
        // UI-only 事实不进模型视图。
        let projection = project_model_messages(&entries);
        assert_eq!(projection.messages.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn commit_failure_stops_memory_advance_and_reports() {
        let root = temp_root("commit-fail");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        // 半批工具事实(ToolUse 无配对结果)会被提交边界拒绝;
        // Runtime 的 commit 必须返回 false、发 Error,并且不推进内存镜像。
        let orphan = SessionEntryPayload::message(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "orphan".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            },
            None,
        );
        let mut events = Vec::new();
        let committed = agent.commit(vec![orphan], &mut |event| events.push(event));

        assert!(!committed);
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::Error(text) if text.contains("保存会话失败"))
        ));
        // 内存镜像与磁盘一致:双方都是空。
        assert!(agent.entries.is_empty());
        let id = agent.session_id().to_string();
        let (disk_entries, _) = agent.sessions.load(&id).unwrap();
        assert!(disk_entries.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    // ---- 阶段 5:RetryPolicy 与 steering/follow-up ----

    /// 轮询组合取消标志直到被置位,再以 Aborted 收尾。
    /// 配合"emit 侧看到 ToolCallStarted 就置全局取消"模拟用户在工具执行中按 Esc:
    /// 协调线程会把全局取消传播到每个调用的组合标志。
    struct WaitForCancelTool;

    impl Tool for WaitForCancelTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "wait_for_cancel".into(),
                description: "waits until cancelled".into(),
                schema: serde_json::json!({ "type": "object" }),
                capabilities: ToolCapabilities::READ_ONLY,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &serde_json::Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !ctx.cancel.load(Ordering::Relaxed) {
                if std::time::Instant::now() > deadline {
                    return Err(ToolError::execution("测试超时:未收到取消传播"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(ToolError::new(ToolErrorCode::Aborted, "已取消"))
        }
    }

    fn inbox_with(commands: Vec<AgentCommand>) -> Receiver<AgentCommand> {
        let (tx, rx) = std::sync::mpsc::channel();
        for command in commands {
            tx.send(command).unwrap();
        }
        rx
    }

    fn user_texts(entries: &[SessionEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                SessionEntryPayload::Message(record) if record.message.role == Role::User => {
                    let text = record.message.text();
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn retry_policy_is_deterministic_and_bounded() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(60),
            jitter_seed: 42,
        };
        // 尝试耗尽 → 不再重试。
        assert_eq!(policy.delay_for(3, None), None);
        // 服务器等待请求超上限 → 拒绝重试。
        assert_eq!(policy.delay_for(1, Some(Duration::from_secs(120))), None);
        // 合理的 Retry-After 原样生效(不加 jitter)。
        assert_eq!(
            policy.delay_for(1, Some(Duration::from_millis(2500))),
            Some(Duration::from_millis(2500))
        );
        // 指数退避 + jitter ∈ [0,25%),且同参数结果恒定。
        let first = policy.delay_for(1, None).unwrap();
        assert!(first >= Duration::from_secs(2) && first < Duration::from_millis(2500));
        assert_eq!(policy.delay_for(1, None), Some(first));
        let second = policy.delay_for(2, None).unwrap();
        assert!(second >= Duration::from_secs(4) && second < Duration::from_secs(5));
        // 退避不越过 max_delay。
        let capped = RetryPolicy {
            base_delay: Duration::from_secs(20),
            max_delay: Duration::from_secs(30),
            ..policy
        };
        assert!(capped.delay_for(2, None).unwrap() <= Duration::from_secs(30));
    }

    #[test]
    fn oversized_retry_after_fails_without_waiting() {
        let root = temp_root("retry-after-cap");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        // 只有一个步骤:若发生重试,脚本耗尽会 panic。
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Error(
            ProviderError {
                message: "overloaded".into(),
                retryable: true,
                retry_after: Some(Duration::from_secs(300)),
            },
        )]));
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("hi".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "不应等待超长 Retry-After"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error(text) if text.contains("overloaded"))));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn steering_is_injected_only_after_full_tool_batch() {
        let root = temp_root("steering");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        install_counted_tool(
            &mut agent,
            ToolCapabilities::READ_ONLY,
            ToolPermissionSpec::default(),
        );
        let prompts = Arc::new(Mutex::new(Vec::new()));
        agent.provider = Box::new(ScriptedProvider::with_prompt_log(
            vec![
                ScriptStep::Output(tool_turn(vec![
                    ("counted", serde_json::json!({ "path": "a" })),
                    ("counted", serde_json::json!({ "path": "b" })),
                ])),
                ScriptStep::Output(output(
                    ChatMessage {
                        role: Role::Assistant,
                        blocks: vec![Block::Text("changed course".into())],
                    },
                    StopReason::EndTurn,
                )),
            ],
            Arc::clone(&prompts),
        ));
        let inbox = inbox_with(vec![AgentCommand::Steer("换个方向".into())]);
        let mut events = Vec::new();
        agent.handle_command_with_inbox(
            AgentCommand::UserInput("开始".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
            Some(&inbox),
        );

        // steering 出现在完整工具批(assistant + results)之后。
        let kinds: Vec<String> = agent
            .entries
            .iter()
            .map(|entry| match &entry.payload {
                SessionEntryPayload::Message(record) => {
                    let has_results = record
                        .message
                        .blocks
                        .iter()
                        .any(|block| matches!(block, Block::ToolResult { .. }));
                    if record.message.role == Role::Assistant {
                        "assistant".into()
                    } else if has_results {
                        "results".into()
                    } else {
                        format!("user:{}", record.message.text())
                    }
                }
                other => other.kind().to_string(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user:开始".to_string(),
                "assistant".into(),
                "results".into(),
                "user:换个方向".into(),
                "assistant".into(),
            ]
        );
        // 第二次模型调用看到了 steering。
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1].last().unwrap().text(), "换个方向");
        // 整个运行只有一对 TurnStarted/TurnFinished(没有重入)。
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnStarted))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnFinished { .. }))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn user_input_during_active_run_is_classified_as_steering() {
        let root = temp_root("classify");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        install_counted_tool(
            &mut agent,
            ToolCapabilities::READ_ONLY,
            ToolPermissionSpec::default(),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(tool_turn(vec![(
                "counted",
                serde_json::json!({ "path": "a" }),
            )])),
            ScriptStep::Output(output(ChatMessage::empty_assistant(), StopReason::EndTurn)),
        ]));
        let inbox = inbox_with(vec![AgentCommand::UserInput("第二条".into())]);
        let mut events = Vec::new();
        agent.handle_command_with_inbox(
            AgentCommand::UserInput("第一条".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
            Some(&inbox),
        );

        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice(text) if text.contains("steering"))));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnStarted))
                .count(),
            1,
            "不允许重入开第二个运行"
        );
        assert!(user_texts(&agent.entries).contains(&"第二条".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn follow_up_runs_only_after_current_task_would_stop() {
        let root = temp_root("follow-up");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        agent.provider = Box::new(ScriptedProvider::with_prompt_log(
            vec![
                ScriptStep::Output(output(
                    ChatMessage {
                        role: Role::Assistant,
                        blocks: vec![Block::Text("第一件事完成".into())],
                    },
                    StopReason::EndTurn,
                )),
                ScriptStep::Output(output(
                    ChatMessage {
                        role: Role::Assistant,
                        blocks: vec![Block::Text("第二件事完成".into())],
                    },
                    StopReason::EndTurn,
                )),
            ],
            Arc::clone(&prompts),
        ));
        let inbox = inbox_with(vec![AgentCommand::FollowUp("下一件事".into())]);
        let mut events = Vec::new();
        agent.handle_command_with_inbox(
            AgentCommand::UserInput("第一件事".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
            Some(&inbox),
        );

        // follow-up 在第一件事的 assistant 之后注入。
        assert_eq!(
            user_texts(&agent.entries),
            vec!["第一件事".to_string(), "下一件事".into()]
        );
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1].last().unwrap().text(), "下一件事");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnFinished { .. }))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn queued_inputs_are_injected_one_at_a_time() {
        let root = temp_root("one-at-a-time");
        let mut agent = Agent::new_with_data_dir(
            config_with_max_turns(&root, 5),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        install_counted_tool(
            &mut agent,
            ToolCapabilities::READ_ONLY,
            ToolPermissionSpec::default(),
        );
        let prompts = Arc::new(Mutex::new(Vec::new()));
        agent.provider = Box::new(ScriptedProvider::with_prompt_log(
            vec![
                ScriptStep::Output(tool_turn(vec![(
                    "counted",
                    serde_json::json!({ "path": "a" }),
                )])),
                ScriptStep::Output(tool_turn(vec![(
                    "counted",
                    serde_json::json!({ "path": "b" }),
                )])),
                ScriptStep::Output(output(ChatMessage::empty_assistant(), StopReason::EndTurn)),
            ],
            Arc::clone(&prompts),
        ));
        let inbox = inbox_with(vec![
            AgentCommand::Steer("s1".into()),
            AgentCommand::Steer("s2".into()),
        ]);
        agent.handle_command_with_inbox(
            AgentCommand::UserInput("go".into()),
            &mut |_| {},
            &AtomicBool::new(false),
            Some(&inbox),
        );

        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 3);
        // 第二次调用只注入了最老的一条;s2 等到下一个检查点。
        let second: Vec<String> = prompts[1].iter().map(|m| m.text()).collect();
        assert!(second.contains(&"s1".to_string()));
        assert!(!second.contains(&"s2".to_string()));
        let third: Vec<String> = prompts[2].iter().map(|m| m.text()).collect();
        assert!(third.contains(&"s2".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancel_drops_queued_inputs_with_notice() {
        let root = temp_root("cancel-queues");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.tools = ToolRegistry::new(vec![Box::new(WaitForCancelTool)]);
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(tool_turn(
            vec![("wait_for_cancel", serde_json::json!({}))],
        ))]));
        let inbox = inbox_with(vec![AgentCommand::Steer("不该注入".into())]);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        agent.handle_command_with_inbox(
            AgentCommand::UserInput("go".into()),
            &mut |event| {
                // 模拟用户在工具刚开始执行时按 Esc。
                if matches!(event, AgentEvent::ToolCallStarted { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
                events.push(event);
            },
            &cancel,
            Some(&inbox),
        );

        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: true })));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice(text) if text.contains("丢弃"))));
        assert!(!user_texts(&agent.entries).contains(&"不该注入".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shutdown_during_run_cancels_and_defers_exit() {
        let root = temp_root("shutdown-mid-run");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        install_counted_tool(
            &mut agent,
            ToolCapabilities::READ_ONLY,
            ToolPermissionSpec::default(),
        );
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(tool_turn(
            vec![("counted", serde_json::json!({ "path": "a" }))],
        ))]));
        let inbox = inbox_with(vec![AgentCommand::Shutdown]);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let keep_running = agent.handle_command_with_inbox(
            AgentCommand::UserInput("go".into()),
            &mut |event| events.push(event),
            &cancel,
            Some(&inbox),
        );

        // 当前命令本身处理完毕(true),Shutdown 延迟到下一轮宿主循环。
        assert!(keep_running);
        assert!(cancel.load(Ordering::Relaxed), "Shutdown 应请求取消当前轮");
        assert!(matches!(
            agent.take_deferred(),
            Some(AgentCommand::Shutdown)
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: true })));
        let _ = std::fs::remove_dir_all(root);
    }

    // ---- 阶段 6:受控并发、超时与资源锁 ----

    /// 可配置执行模式的睡眠工具:分片睡眠,轮询组合取消标志。
    struct SleepTool {
        name: &'static str,
        millis: u64,
        mode: crate::tools::ToolExecutionMode,
    }

    impl Tool for SleepTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: "sleeps".into(),
                schema: serde_json::json!({ "type": "object" }),
                capabilities: ToolCapabilities {
                    read_only: true,
                    destructive: false,
                    execution_mode: self.mode,
                    supports_background: false,
                },
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &serde_json::Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            let deadline = std::time::Instant::now() + Duration::from_millis(self.millis);
            while std::time::Instant::now() < deadline {
                if ctx.cancel.load(Ordering::Relaxed) {
                    return Err(ToolError::new(ToolErrorCode::Aborted, "已中止"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(ToolOutput::text(format!("{} 完成", self.name)))
        }
    }

    /// 先报一次进度,然后等待取消(用于验证执行中取消的传播与配对)。
    struct ProgressThenWaitCancelTool;

    impl Tool for ProgressThenWaitCancelTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "progress_then_wait".into(),
                description: "emits progress then waits for cancel".into(),
                schema: serde_json::json!({ "type": "object" }),
                capabilities: ToolCapabilities::READ_ONLY,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &serde_json::Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            ctx.report_progress(ToolOutput::text("已启动"));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !ctx.cancel.load(Ordering::Relaxed) {
                if std::time::Instant::now() > deadline {
                    return Err(ToolError::execution("测试超时:未收到取消传播"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(ToolError::new(ToolErrorCode::Aborted, "已取消"))
        }
    }

    fn finished_order(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolCallFinished { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn result_block_order(entries: &[SessionEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                SessionEntryPayload::Message(record) if record.message.role == Role::User => {
                    let ids: Vec<String> = record
                        .message
                        .blocks
                        .iter()
                        .filter_map(|block| match block {
                            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                            _ => None,
                        })
                        .collect();
                    (!ids.is_empty()).then_some(ids)
                }
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn parallel_batch_finishes_by_completion_but_history_keeps_source_order() {
        let root = temp_root("parallel-order");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.tools = ToolRegistry::new(vec![
            Box::new(SleepTool {
                name: "slow",
                millis: 300,
                mode: crate::tools::ToolExecutionMode::ParallelSafe,
            }),
            Box::new(SleepTool {
                name: "fast",
                millis: 10,
                mode: crate::tools::ToolExecutionMode::ParallelSafe,
            }),
        ]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            // call-1 = slow, call-2 = fast(按源顺序)。
            ScriptStep::Output(tool_turn(vec![
                ("slow", serde_json::json!({})),
                ("fast", serde_json::json!({})),
            ])),
            ScriptStep::Output(output(ChatMessage::empty_assistant(), StopReason::EndTurn)),
        ]));
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("go".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        // UI 完成事件:快的先到。
        assert_eq!(
            finished_order(&events),
            vec!["call-2".to_string(), "call-1".into()],
            "完成顺序应是 fast 先"
        );
        // 历史 ToolResult:仍按 ToolUse 源顺序。
        assert_eq!(
            result_block_order(&agent.entries),
            vec!["call-1".to_string(), "call-2".into()]
        );
        // 并发生效:总时长应接近 slow(300ms)而不是显著串行。
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "批执行不应退化成显著串行"
        );
        // 所有 Started 在任何 Finished 之前(preflight 按源顺序先行)。
        let first_finished = events
            .iter()
            .position(|event| matches!(event, AgentEvent::ToolCallFinished { .. }))
            .unwrap();
        let started_count_before = events[..first_finished]
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(started_count_before, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn any_sequential_tool_forces_whole_batch_sequential() {
        let root = temp_root("sequential-forced");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.tools = ToolRegistry::new(vec![
            Box::new(SleepTool {
                name: "slow_seq",
                millis: 150,
                mode: crate::tools::ToolExecutionMode::Sequential,
            }),
            Box::new(SleepTool {
                name: "fast_par",
                millis: 5,
                mode: crate::tools::ToolExecutionMode::ParallelSafe,
            }),
        ]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(tool_turn(vec![
                ("slow_seq", serde_json::json!({})),
                ("fast_par", serde_json::json!({})),
            ])),
            ScriptStep::Output(output(ChatMessage::empty_assistant(), StopReason::EndTurn)),
        ]));
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("go".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        // 整批退回串行:尽管 fast 更快,完成顺序仍是源顺序。
        assert_eq!(
            finished_order(&events),
            vec!["call-1".to_string(), "call-2".into()]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancel_during_parallel_execution_still_pairs_every_call() {
        let root = temp_root("parallel-cancel");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.tools = ToolRegistry::new(vec![
            Box::new(ProgressThenWaitCancelTool),
            Box::new(SleepTool {
                name: "long_sleep",
                millis: 5_000,
                mode: crate::tools::ToolExecutionMode::ParallelSafe,
            }),
        ]);
        agent.provider = Box::new(ScriptedProvider::new(vec![ScriptStep::Output(tool_turn(
            vec![
                ("progress_then_wait", serde_json::json!({})),
                ("long_sleep", serde_json::json!({})),
            ],
        ))]));
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("go".into()),
            &mut |event| {
                // 第一个进度事件说明工具确实已在执行中,此刻模拟 Esc。
                if matches!(event, AgentEvent::ToolCallUpdated { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
                events.push(event);
            },
            &cancel,
        );

        assert!(
            started.elapsed() < Duration::from_secs(4),
            "取消应中断执行,而不是等工具睡满"
        );
        // 每个 ToolUse 都有配对结果,历史仍按源顺序。
        assert_eq!(
            result_block_order(&agent.entries),
            vec!["call-1".to_string(), "call-2".into()]
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: true })));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tool_timeout_rewrites_obedient_abort_to_timeout() {
        let root = temp_root("tool-timeout");
        let mut agent = Agent::new_with_data_dir(
            config(&root),
            Workspace::new(root.clone()),
            root.join("data"),
        )
        .unwrap();
        agent.config.tool_timeout = Some(Duration::from_millis(120));
        agent.tools = ToolRegistry::new(vec![Box::new(SleepTool {
            name: "sleepy",
            millis: 10_000,
            mode: crate::tools::ToolExecutionMode::ParallelSafe,
        })]);
        agent.provider = Box::new(ScriptedProvider::new(vec![
            ScriptStep::Output(tool_turn(vec![("sleepy", serde_json::json!({}))])),
            ScriptStep::Output(output(ChatMessage::empty_assistant(), StopReason::EndTurn)),
        ]));
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        agent.handle_command(
            AgentCommand::UserInput("go".into()),
            &mut |event| events.push(event),
            &AtomicBool::new(false),
        );

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时应中止工具,而不是等它睡满 10s"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::ToolCallFinished {
                    error: Some(ToolError {
                        code: ToolErrorCode::Timeout,
                        ..
                    }),
                    ..
                }
            )),
            "超时中止应报 Timeout 而不是 Aborted: {events:?}"
        );
        // 超时不是用户取消,本轮正常结束(模型会看到错误结果并自行调整)。
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
        let _ = std::fs::remove_dir_all(root);
    }
}

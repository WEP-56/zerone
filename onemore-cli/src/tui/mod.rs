//! TUI 前端:消费 AgentEvent 渲染画面,把用户操作变成 AgentCommand。
//!
//! 结构是最朴素的"单线程事件循环":
//! ```text
//! loop {
//!     把 Runtime 事件通道里攒的事件全部应用到界面状态;
//!     poll 终端按键(33ms 超时,顺带当渲染节拍);
//!     有变化才重绘;
//! }
//! ```
//! Runtime 在另一个线程里跑,阻塞的网络/工具调用不会卡住画面。
//!
//! Windows 特有的坑,这里都处理了:
//! - crossterm 在 Windows 会同时上报按键的 Press/Release,必须只认 Press,
//!   否则每个字都打两遍;
//! - 在 conpty 终端(Windows Terminal / VS Code)里,一次按键的
//!   Press+Release 是**同一瞬间**被合成出来的,队列里"有积压"不代表
//!   在粘贴。识别粘贴必须把积压事件取出来看内容(见 `enter_means_newline`),
//!   否则 Enter 永远发不出消息;
//! - 传统控制台没有括号粘贴(bracketed paste),多行粘贴会变成一串
//!   带 Enter 的按键;同样靠上面的内容检查兜底;
//! - 和 Codex 一样使用 inline viewport,已完成的消息写入终端原生 scrollback。
//!   因此不需要鼠标捕获:滚轮滚动终端历史,普通拖动直接选择文本。

mod command;
mod input;
mod picker;
mod transcript;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{Frame, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use crate::event::{AgentCommand, AgentEvent};
use crate::message::{Block as MessageBlock, ChatMessage, Role};
use crate::permission::{ApprovalDecision, ApprovalRequest, ApprovalResponse, ApprovalScope};
use crate::runtime::RuntimeHandle;
use crate::session::{SessionEntry, SessionEntryPayload};
use crate::util;
use input::InputBox;
use picker::{Picker, PickerItem};
use transcript::Transcript;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 输入区内容最多显示的行数(超出滚动)。
const INPUT_MAX_ROWS: usize = 6;
/// Inline viewport 只承载 live 内容和 composer；已完成消息在原生 scrollback 中。
const INLINE_VIEWPORT_ROWS: u16 = 8;
const HELP_TEXT: &str = "斜杠命令\n\
  /model             选择或输入模型\n\
  /provider          选择 provider(对话历史保留)\n\
  /session [ID]      列出或恢复历史会话\n\
  /compact           压缩历史(摘要替代模型视图,事实保留)\n\
  /queue <内容>      排队后续任务(当前任务结束后执行)\n\
  /clear             清空会话\n\
  /quit              退出\n\
\n\
运行中输入并回车 = steering:在当前一批工具完成后注入,修正方向;Esc 取消当前轮\n\
编辑: Ctrl+A/E 行首/行尾 · Ctrl+W 删前一词 · Ctrl+K 删到行尾 · Alt+←/→ 按词移动\n\
操作: ↑/↓ 浏览候选或历史 · Tab 补全 · Esc 关闭/取消 · 滚轮浏览终端历史 · 鼠标拖动选择文本";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Provider,
    Model,
    Session,
}

#[derive(Debug)]
enum Overlay {
    Picker { kind: PickerKind, picker: Picker },
    ModelInput(InputBox),
    Loading { kind: PickerKind, title: String },
    Approval(ApprovalRequest),
}

pub fn run(handle: RuntimeHandle) -> anyhow::Result<()> {
    let (_, terminal_rows) = ratatui::crossterm::terminal::size()?;
    let viewport_height = INLINE_VIEWPORT_ROWS.min(terminal_rows.max(1));
    let mut terminal = ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(viewport_height),
    })?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);

    let mut app = App::new(handle);
    app.transcript.push_notice(format!(
        "Onemore 已就绪({}) · 会话 {},输入内容开始对话,/help 查看命令",
        app.provider_label,
        short_id(&app.session_id)
    ));

    let result = app.event_loop(&mut terminal);

    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

struct App {
    handle: RuntimeHandle,
    transcript: Transcript,
    input: InputBox,
    overlay: Option<Overlay>,
    slash_selected: usize,
    slash_dismissed: Option<String>,

    /// 已从终端读出、还没处理的事件(Enter 的粘贴检测会预读一批进来)。
    pending_events: VecDeque<Event>,

    // 输入历史(↑/↓ 翻阅)
    history: Vec<String>,
    history_idx: Option<usize>,
    history_draft: String,

    busy: bool,
    status_note: String,
    provider_label: String,
    session_id: String,
    usage: (u64, u64),
    scroll_up: usize,
    last_transcript_height: u16,

    spinner_frame: usize,
    last_spin: Instant,
    quit_armed_at: Option<Instant>,
    should_quit: bool,
    force_clear: bool,
}

impl App {
    fn new(handle: RuntimeHandle) -> App {
        let provider_label = handle.provider_label.clone();
        let session_id = handle.session_id.clone();
        App {
            handle,
            transcript: Transcript::default(),
            input: InputBox::default(),
            overlay: None,
            slash_selected: 0,
            slash_dismissed: None,
            pending_events: VecDeque::new(),
            history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            busy: false,
            status_note: String::new(),
            provider_label,
            session_id,
            usage: (0, 0),
            scroll_up: 0,
            last_transcript_height: 20,
            spinner_frame: 0,
            last_spin: Instant::now(),
            quit_armed_at: None,
            should_quit: false,
            force_clear: false,
        }
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
        let mut dirty = true;
        loop {
            // 1. 应用 Runtime 事件
            while let Ok(ev) = self.handle.events.try_recv() {
                self.on_agent_event(ev);
                dirty = true;
            }
            // 2. 终端输入:一帧内把积压处理干净(粘贴洪峰时避免一字符一帧),
            //    上限防止超大粘贴饿死渲染
            let mut budget = 512;
            while budget > 0 {
                let ev = if let Some(e) = self.pending_events.pop_front() {
                    e
                } else if event::poll(Duration::ZERO)? {
                    event::read()?
                } else {
                    break;
                };
                dirty |= self.on_terminal_event(ev);
                budget -= 1;
            }
            // 空闲时阻塞等待(33ms 超时 ≈ 渲染节拍)
            if !dirty && event::poll(Duration::from_millis(33))? {
                let ev = event::read()?;
                dirty |= self.on_terminal_event(ev);
            }
            // 3. 忙碌时转 spinner
            if self.busy && self.last_spin.elapsed() > Duration::from_millis(100) {
                self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
                self.last_spin = Instant::now();
                dirty = true;
            }
            // 双击退出的提示过期后恢复状态栏
            if let Some(t) = self.quit_armed_at {
                if t.elapsed() > Duration::from_secs(2) {
                    self.quit_armed_at = None;
                    dirty = true;
                }
            }
            dirty |= self.commit_history(terminal)?;
            // 4. 渲染
            if self.force_clear {
                terminal.clear()?;
                self.force_clear = false;
            }
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }
            if self.should_quit {
                return Ok(());
            }
        }
    }

    fn commit_history(&mut self, terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<bool> {
        let width = terminal.size()?.width.max(1);
        let lines = self.transcript.drain_finalized_lines(width);
        if lines.is_empty() {
            return Ok(false);
        }
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        terminal.insert_before(height, move |buffer| {
            Paragraph::new(lines).render(buffer.area, buffer);
            clear_wide_continuation_cells(buffer);
        })?;
        self.scroll_up = 0;
        Ok(true)
    }

    /// 返回事件是否改变了界面。
    fn on_terminal_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                self.on_key(k.code, k.modifiers);
                true
            }
            Event::Paste(s) => {
                match &mut self.overlay {
                    Some(Overlay::ModelInput(input)) => input.insert_str(&s),
                    Some(Overlay::Picker { picker, .. }) => {
                        for c in s.chars() {
                            picker.push_filter(c);
                        }
                    }
                    Some(Overlay::Loading { .. }) | Some(Overlay::Approval(_)) => {}
                    None => {
                        self.input.insert_str(&s);
                        self.on_input_changed();
                    }
                }
                true
            }
            Event::Resize(_, _) => true,
            _ => false,
        }
    }

    // ---- Runtime 事件 → 界面状态 ----

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::UserMessage(t) => self.transcript.push_user(t),
            AgentEvent::TurnStarted => {
                self.busy = true;
                self.status_note = "思考中".into();
            }
            AgentEvent::AssistantDelta(t) => self.transcript.append_assistant(&t),
            AgentEvent::ThinkingDelta(t) => self.transcript.append_thinking(&t),
            AgentEvent::AssistantMessage(full) => self.transcript.finalize_assistant(full),
            AgentEvent::ToolCallPending { name } => {
                self.status_note = format!("正在生成 {} 的参数", name);
            }
            AgentEvent::ToolCallStarted { id, name, summary } => {
                self.status_note = format!("执行 {}", name);
                self.transcript.push_tool(id, name, summary);
            }
            AgentEvent::ToolCallUpdated { output, .. } => {
                self.status_note = format!("工具进度: {}", output.ui_text());
            }
            AgentEvent::ToolCallFinished {
                id, output, error, ..
            } => {
                self.status_note = "思考中".into();
                self.transcript
                    .finish_tool(&id, output.ui_text().to_string(), error.is_some());
            }
            AgentEvent::PermissionRequested { request } => {
                self.status_note = format!("等待审批: {}", request.tool);
                self.overlay = Some(Overlay::Approval(request));
            }
            AgentEvent::PermissionResolved {
                request_id,
                allowed,
            } => {
                if matches!(
                    &self.overlay,
                    Some(Overlay::Approval(request)) if request.request_id == request_id
                ) {
                    self.overlay = None;
                }
                self.status_note = if allowed {
                    "审批通过，正在执行".into()
                } else {
                    "审批未通过".into()
                };
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => self.usage = (input_tokens, output_tokens),
            AgentEvent::Notice(t) => self.transcript.push_notice(t),
            AgentEvent::Error(t) => {
                if matches!(self.overlay, Some(Overlay::Loading { .. })) {
                    self.overlay = None;
                }
                self.transcript.push_error(t);
            }
            AgentEvent::ConversationCleared => {
                self.transcript.clear();
                self.usage = (0, 0);
                self.transcript.push_notice("会话已清空".into());
            }
            AgentEvent::ProviderChanged { label } => {
                self.provider_label = label;
            }
            AgentEvent::SessionsListed {
                current_id,
                sessions,
            } => {
                if matches!(
                    self.overlay,
                    Some(Overlay::Loading {
                        kind: PickerKind::Session,
                        ..
                    })
                ) {
                    let items = sessions
                        .into_iter()
                        .map(|session| {
                            let is_current = session.id == current_id;
                            let label = if session.title.is_empty() {
                                format!("会话 {}", short_id(&session.id))
                            } else {
                                session.title
                            };
                            PickerItem {
                                label,
                                description: format!(
                                    "{} 条消息 · {}{}",
                                    session.message_count,
                                    short_id(&session.id),
                                    if is_current { " · 当前" } else { "" }
                                ),
                                value: Some(session.id),
                                current: is_current,
                            }
                        })
                        .collect();
                    self.overlay = Some(Overlay::Picker {
                        kind: PickerKind::Session,
                        picker: Picker::new("恢复会话", items),
                    });
                }
            }
            AgentEvent::SessionLoaded {
                id,
                entries,
                input_tokens,
                output_tokens,
            } => {
                self.session_id = id;
                self.usage = (input_tokens, output_tokens);
                let message_count = entries
                    .iter()
                    .filter(|entry| matches!(entry.payload, SessionEntryPayload::Message(_)))
                    .count();
                self.restore_transcript(&entries);
                self.transcript.push_notice(format!(
                    "已恢复会话 {}({} 条历史消息,{} 条事实)",
                    short_id(&self.session_id),
                    message_count,
                    entries.len()
                ));
                self.scroll_up = 0;
            }
            AgentEvent::TurnFinished { cancelled } => {
                self.busy = false;
                self.status_note.clear();
                self.transcript.close_open_cells();
                if cancelled {
                    self.transcript.push_notice("已取消".into());
                }
            }
        }
    }

    /// 按事实日志重建画面:Message 还原对话与工具单元,
    /// Notice/Compaction/ModelChange 等 UI-only 事实以提示行呈现。
    fn restore_transcript(&mut self, entries: &[SessionEntry]) {
        let results: HashMap<&str, (&str, bool)> = entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                SessionEntryPayload::Message(record) => Some(&record.message),
                _ => None,
            })
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                MessageBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
                _ => None,
            })
            .collect();

        self.transcript.clear();
        for entry in entries {
            match &entry.payload {
                SessionEntryPayload::Message(record) => {
                    self.restore_message(&record.message, &results)
                }
                SessionEntryPayload::Notice(notice) => {
                    self.transcript.push_notice(notice.text.clone());
                }
                SessionEntryPayload::Compaction(compaction) => {
                    self.transcript.push_notice(format!(
                        "—— 历史已压缩(压缩前约 {} tokens);此后模型视图从摘要开始 ——",
                        compaction.tokens_before
                    ));
                }
                SessionEntryPayload::ModelChange(change) => {
                    self.transcript
                        .push_notice(format!("模型切换: {}", change.provider));
                }
                SessionEntryPayload::Artifact(_) => {}
            }
        }
    }

    fn restore_message(&mut self, message: &ChatMessage, results: &HashMap<&str, (&str, bool)>) {
        match message.role {
            Role::User => {
                for block in &message.blocks {
                    if let MessageBlock::Text(text) = block {
                        self.transcript.push_user(text.clone());
                    }
                }
            }
            Role::Assistant => {
                for block in &message.blocks {
                    match block {
                        MessageBlock::Text(text) => {
                            self.transcript.append_assistant(text);
                        }
                        MessageBlock::Thinking { text, .. } if !text.is_empty() => {
                            self.transcript.append_thinking(text);
                        }
                        MessageBlock::ToolUse { id, name, input } => {
                            self.transcript.push_tool(
                                id.clone(),
                                name.clone(),
                                util::args_summary(input),
                            );
                            if let Some((output, is_error)) = results.get(id.as_str()) {
                                self.transcript.finish_tool(
                                    id,
                                    util::truncate_middle(output, 4000),
                                    *is_error,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                self.transcript.close_open_cells();
            }
        }
    }

    // ---- 按键 ----

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.overlay.is_some() {
            self.on_overlay_key(code, mods);
            return;
        }

        let slash_open = !self.slash_matches().is_empty();
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.on_ctrl_c(),
            KeyCode::Char('l') if mods.contains(KeyModifiers::CONTROL) => {
                self.force_clear = true;
            }
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => self.input.move_start(),
            KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => self.input.move_end_all(),
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                self.input.delete_word_left();
                self.on_input_changed();
            }
            KeyCode::Char('k') if mods.contains(KeyModifiers::CONTROL) => {
                self.input.delete_to_line_end();
                self.on_input_changed();
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.on_input_changed();
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.input.insert_char(c);
                self.on_input_changed();
            }
            KeyCode::Tab if slash_open => self.complete_slash_command(),
            KeyCode::BackTab if slash_open => self.move_slash_selection(false),
            KeyCode::Tab => {
                self.input.insert_str("    ");
                self.on_input_changed();
            }
            KeyCode::Enter => {
                if slash_open {
                    self.run_selected_slash_command();
                    return;
                }
                // 依次检查修饰键、可靠的行尾反斜杠语法、粘贴洪峰。短路求值避免
                // 普通 Shift+Enter 也去预读终端事件。
                let newline = mods
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
                    || self.input.pop_trailing_backslash()
                    || self.enter_means_newline();
                if newline {
                    self.input.insert_char('\n');
                    self.on_input_changed();
                } else {
                    self.submit();
                }
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.on_input_changed();
            }
            KeyCode::Delete => {
                self.input.delete();
                self.on_input_changed();
            }
            KeyCode::Left if mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                self.input.move_word_left()
            }
            KeyCode::Right if mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                self.input.move_word_right()
            }
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Up if slash_open => self.move_slash_selection(false),
            KeyCode::Down if slash_open => self.move_slash_selection(true),
            KeyCode::Up => {
                if self.input.is_multiline() {
                    self.input.move_vertical(true);
                } else {
                    self.history_prev();
                }
            }
            KeyCode::Down => {
                if self.input.is_multiline() {
                    self.input.move_vertical(false);
                } else {
                    self.history_next();
                }
            }
            KeyCode::PageUp => {
                self.scroll_up = self
                    .scroll_up
                    .saturating_add(self.last_transcript_height.max(1) as usize / 2);
            }
            KeyCode::PageDown => {
                self.scroll_up = self
                    .scroll_up
                    .saturating_sub(self.last_transcript_height.max(1) as usize / 2);
            }
            KeyCode::Esc => {
                if slash_open {
                    self.slash_dismissed = Some(self.input.text().to_string());
                } else if self.busy {
                    // 请求取消当前轮;Runtime 在下一个流事件/工具间隙生效
                    self.handle
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.status_note = "取消中…".into();
                } else if self.scroll_up > 0 {
                    self.scroll_up = 0;
                } else {
                    self.input.clear();
                    self.on_input_changed();
                }
            }
            _ => {}
        }
    }

    fn on_input_changed(&mut self) {
        self.slash_selected = 0;
        self.slash_dismissed = None;
        self.history_idx = None;
    }

    fn slash_query(&self) -> Option<&str> {
        let text = self.input.text();
        if self.slash_dismissed.as_deref() == Some(text) || !text.starts_with('/') {
            return None;
        }
        let line_end = text.find('\n').unwrap_or(text.len());
        if self.input.cursor() > line_end {
            return None;
        }
        let first_line = &text[..line_end];
        let command_end = first_line
            .find(char::is_whitespace)
            .unwrap_or(first_line.len());
        if self.input.cursor() > command_end || first_line[1..].chars().any(char::is_whitespace) {
            return None;
        }
        Some(&first_line[1..command_end])
    }

    fn slash_matches(&self) -> Vec<&'static command::CommandSpec> {
        self.slash_query().map(command::matches).unwrap_or_default()
    }

    fn move_slash_selection(&mut self, down: bool) {
        let len = self.slash_matches().len();
        if len == 0 {
            return;
        }
        self.slash_selected = if down {
            (self.slash_selected + 1) % len
        } else if self.slash_selected == 0 {
            len - 1
        } else {
            self.slash_selected - 1
        };
    }

    fn selected_slash_command(&self) -> Option<&'static command::CommandSpec> {
        let matches = self.slash_matches();
        matches
            .get(self.slash_selected.min(matches.len().saturating_sub(1)))
            .copied()
    }

    fn complete_slash_command(&mut self) {
        if let Some(spec) = self.selected_slash_command() {
            let suffix = if spec.accepts_args { " " } else { "" };
            self.input.set(format!("/{}{}", spec.name, suffix));
            self.slash_selected = 0;
            self.slash_dismissed = spec.accepts_args.then(|| self.input.text().to_string());
        }
    }

    fn run_selected_slash_command(&mut self) {
        if let Some(spec) = self.selected_slash_command() {
            self.input.clear();
            self.slash_dismissed = None;
            self.execute_slash(spec.command, "");
        }
    }

    fn on_overlay_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let mut accept_picker = false;
        let mut close = false;
        let mut approval = None;
        match self.overlay.as_mut().expect("overlay checked above") {
            Overlay::Picker { picker, .. } => match code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::BackTab => picker.move_up(),
                KeyCode::Down | KeyCode::Tab => picker.move_down(),
                KeyCode::Enter => accept_picker = true,
                KeyCode::Backspace => picker.pop_filter(),
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => close = true,
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => picker.push_filter(c),
                _ => {}
            },
            Overlay::ModelInput(input) => match code {
                KeyCode::Esc => close = true,
                KeyCode::Enter => {
                    let model = input.take().trim().to_string();
                    if !model.is_empty() {
                        let _ = self.handle.commands.send(AgentCommand::SetModel(model));
                        close = true;
                    }
                }
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Left if mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                    input.move_word_left()
                }
                KeyCode::Right if mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
                    input.move_word_right()
                }
                KeyCode::Left => input.move_left(),
                KeyCode::Right => input.move_right(),
                KeyCode::Home => input.move_home(),
                KeyCode::End => input.move_end(),
                KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => input.move_start(),
                KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => input.move_end_all(),
                KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                    input.delete_word_left()
                }
                KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => input.clear(),
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => input.insert_char(c),
                _ => {}
            },
            Overlay::Loading { .. } => {
                if matches!(code, KeyCode::Esc)
                    || matches!(code, KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL))
                {
                    close = true;
                }
            }
            Overlay::Approval(request) => match code {
                KeyCode::Enter | KeyCode::Char('y')
                    if request.scopes.contains(&ApprovalScope::Once) =>
                {
                    approval = Some((request.request_id.clone(), ApprovalScope::Once));
                }
                KeyCode::Char('a') if request.scopes.contains(&ApprovalScope::Session) => {
                    approval = Some((request.request_id.clone(), ApprovalScope::Session));
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    let _ = self.handle.approvals.send(ApprovalResponse {
                        request_id: request.request_id.clone(),
                        decision: ApprovalDecision::Deny,
                    });
                    close = true;
                }
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                    let _ = self.handle.approvals.send(ApprovalResponse {
                        request_id: request.request_id.clone(),
                        decision: ApprovalDecision::Deny,
                    });
                    close = true;
                }
                _ => {}
            },
        }
        if let Some((request_id, scope)) = approval {
            let _ = self.handle.approvals.send(ApprovalResponse {
                request_id,
                decision: ApprovalDecision::Allow(scope),
            });
            self.overlay = None;
        } else if accept_picker {
            self.accept_picker();
        } else if close {
            self.overlay = None;
        }
    }

    fn accept_picker(&mut self) {
        let selected = match &self.overlay {
            Some(Overlay::Picker { kind, picker }) => picker.selected().map(|item| (*kind, item)),
            _ => None,
        };
        let Some((kind, item)) = selected else { return };
        match (kind, item.value) {
            (PickerKind::Provider, Some(provider)) => {
                let _ = self
                    .handle
                    .commands
                    .send(AgentCommand::SwitchProvider(provider));
                self.overlay = None;
            }
            (PickerKind::Model, Some(model)) => {
                let _ = self.handle.commands.send(AgentCommand::SetModel(model));
                self.overlay = None;
            }
            (PickerKind::Model, None) => {
                self.overlay = Some(Overlay::ModelInput(InputBox::default()));
            }
            (PickerKind::Session, Some(session_id)) => {
                let _ = self
                    .handle
                    .commands
                    .send(AgentCommand::LoadSession(session_id));
                self.overlay = None;
            }
            (PickerKind::Provider | PickerKind::Session, None) => {}
        }
    }

    /// 判定这个 Enter 是"粘贴内容里的换行"还是"用户按下发送"。
    ///
    /// 背景:conpty 终端(Windows Terminal / VS Code)把一次按键合成为
    /// **同一瞬间**入队的 Press+Release 两条记录,所以"队列非空"完全
    /// 不能说明在粘贴——必须把积压事件读出来看内容:
    /// 粘贴时,这个 Enter 后面必然紧跟着更多**字符按下**事件;
    /// 手动按 Enter 时,积压里最多只有 Release 之类的噪音。
    /// 预读的事件存进 `pending_events`,主循环照常消费,一个不丢。
    ///
    /// 8ms 小超时是给 conpty 管道分块留的余量(超大粘贴可能在块边界
    /// 短暂断流);人手两次按键的间隔远大于它,不会把打字误判成粘贴。
    /// 代价是每次发送多约 8ms 延迟,无感。
    ///
    /// 已知取舍:粘贴内容若以换行结尾,最后那个换行会触发发送——
    /// 与把命令粘进 shell 的行为一致。
    fn enter_means_newline(&mut self) -> bool {
        while let Ok(true) = event::poll(Duration::from_millis(8)) {
            match event::read() {
                Ok(ev) => self.pending_events.push_back(ev),
                Err(_) => break,
            }
        }
        self.pending_events.iter().any(|ev| match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                matches!(k.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
            }
            Event::Paste(_) => true,
            _ => false,
        })
    }

    fn on_ctrl_c(&mut self) {
        // 规则:输入非空 → 清空输入;否则第一次按 → 预备退出(忙碌时顺带取消),
        // 2 秒内再按 → 真退出。任何状态下连按两次都能离开。
        if let Some(t) = self.quit_armed_at {
            if t.elapsed() <= Duration::from_secs(2) {
                self.quit();
                return;
            }
        }
        if !self.input.is_empty() {
            self.input.clear();
            return;
        }
        if self.busy {
            self.handle
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.quit_armed_at = Some(Instant::now());
    }

    fn quit(&mut self) {
        self.handle
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.handle.commands.send(AgentCommand::Shutdown);
        self.should_quit = true;
    }

    // ---- 提交与命令 ----

    fn submit(&mut self) {
        let raw = self.input.take();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(rest) = text.strip_prefix('/') {
            self.handle_slash(rest.trim());
            return;
        }
        if self.busy {
            // 运行中回车 = steering:Runtime 会在当前完整工具批之后注入。
            self.history.push(text.clone());
            self.history_idx = None;
            self.transcript
                .push_notice("已排队为 steering,将在当前一批工具完成后注入(Esc 取消本轮)".into());
            let _ = self.handle.commands.send(AgentCommand::Steer(text));
            return;
        }
        self.history.push(text.clone());
        self.history_idx = None;
        self.scroll_up = 0;
        let _ = self.handle.commands.send(AgentCommand::UserInput(text));
    }

    fn handle_slash(&mut self, cmd: &str) {
        let (head, rest) = match cmd.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        let Some(spec) = command::find(head) else {
            self.transcript
                .push_error(format!("未知命令 /{},输入 / 查看可用命令", head));
            return;
        };
        self.execute_slash(spec.command, rest);
    }

    fn execute_slash(&mut self, command: command::SlashCommand, rest: &str) {
        match command {
            command::SlashCommand::Help => self.transcript.push_notice(HELP_TEXT.into()),
            command::SlashCommand::Quit => self.quit(),
            command::SlashCommand::Clear => {
                // 命令走通道排队,真正清空以 ConversationCleared 事件为准
                let _ = self.handle.commands.send(AgentCommand::ClearConversation);
            }
            command::SlashCommand::Compact => {
                let _ = self.handle.commands.send(AgentCommand::Compact);
            }
            command::SlashCommand::Queue => {
                if rest.is_empty() {
                    self.transcript
                        .push_error("/queue 需要内容,例如 /queue 跑一遍测试".into());
                } else {
                    self.transcript
                        .push_notice("已排队为后续任务,当前任务结束后执行".into());
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::FollowUp(rest.to_string()));
                }
            }
            command::SlashCommand::Session => {
                if rest.is_empty() {
                    self.overlay = Some(Overlay::Loading {
                        kind: PickerKind::Session,
                        title: "正在读取会话…".into(),
                    });
                    let _ = self.handle.commands.send(AgentCommand::ListSessions);
                } else {
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::LoadSession(rest.to_string()));
                }
            }
            command::SlashCommand::Provider => {
                if rest.is_empty() {
                    self.open_provider_picker();
                } else {
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::SwitchProvider(rest.to_string()));
                }
            }
            command::SlashCommand::Model => {
                if rest.is_empty() {
                    self.open_model_picker();
                } else {
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::SetModel(rest.to_string()));
                }
            }
        }
    }

    fn open_provider_picker(&mut self) {
        let current = self.provider_label.split(" / ").next().unwrap_or("");
        let items = self
            .handle
            .provider_names
            .iter()
            .map(|name| PickerItem {
                label: name.clone(),
                description: if name == current {
                    "当前 provider".into()
                } else {
                    "config.toml 中的 profile".into()
                },
                value: Some(name.clone()),
                current: name == current,
            })
            .collect();
        self.overlay = Some(Overlay::Picker {
            kind: PickerKind::Provider,
            picker: Picker::new("选择 provider", items),
        });
    }

    fn open_model_picker(&mut self) {
        let current = self.provider_label.split(" / ").nth(1).unwrap_or("");
        let mut models = self.handle.model_names.clone();
        if !current.is_empty() && !models.iter().any(|model| model == current) {
            models.insert(0, current.to_string());
        }
        let mut items: Vec<PickerItem> = models
            .into_iter()
            .map(|model| PickerItem {
                description: if model == current {
                    "当前模型".into()
                } else {
                    "来自 config.toml".into()
                },
                current: model == current,
                value: Some(model.clone()),
                label: model,
            })
            .collect();
        items.push(PickerItem {
            label: "自定义模型…".into(),
            description: "输入服务端支持的模型名称".into(),
            value: None,
            current: false,
        });
        self.overlay = Some(Overlay::Picker {
            kind: PickerKind::Model,
            picker: Picker::new("选择模型", items),
        });
    }

    // ---- 输入历史 ----

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.history_draft = self.input.take();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.input.set(self.history[idx].clone());
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 < self.history.len() {
            self.history_idx = Some(idx + 1);
            self.input.set(self.history[idx + 1].clone());
        } else {
            self.history_idx = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.input.set(draft);
        }
    }

    // ---- 渲染 ----

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 10 || area.height < 6 {
            frame.render_widget(Paragraph::new("窗口太小"), area);
            return;
        }

        if self.overlay.is_some() {
            let bottom_h = match &self.overlay {
                Some(Overlay::Picker { picker, .. }) => picker.preferred_height(),
                Some(Overlay::ModelInput(_)) | Some(Overlay::Loading { .. }) => 5,
                Some(Overlay::Approval(_)) => 7,
                None => 0,
            }
            .min(area.height.saturating_sub(2));
            let [t_area, overlay_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_h)]).areas(area);
            self.draw_transcript(frame, t_area);
            match &mut self.overlay {
                Some(Overlay::Picker { picker, .. }) => picker.render(frame, overlay_area),
                Some(Overlay::ModelInput(input)) => {
                    draw_model_input(frame, overlay_area, input);
                }
                Some(Overlay::Loading { title, .. }) => draw_loading(frame, overlay_area, title),
                Some(Overlay::Approval(request)) => draw_approval(frame, overlay_area, request),
                None => {}
            }
            return;
        }

        let iv = self
            .input
            .view(area.width.saturating_sub(6), INPUT_MAX_ROWS);
        let input_h = (iv.total_rows.clamp(1, INPUT_MAX_ROWS) as u16) + 2;
        let slash_matches = self.slash_matches();
        let popup_h = slash_matches.len().min(7) as u16;
        let [t_area, p_area, i_area, s_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(popup_h),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_transcript(frame, t_area);

        if popup_h > 0 {
            self.draw_slash_popup(frame, p_area, &slash_matches);
        }

        self.draw_composer(frame, i_area, &iv);
        frame.render_widget(self.status_line(), s_area);
    }

    fn draw_transcript(&mut self, frame: &mut Frame, area: Rect) {
        self.last_transcript_height = area.height;
        let horizontal_margin = if area.width >= 100 { 3 } else { 1 };
        let t_inner = Rect {
            x: area.x + horizontal_margin,
            y: area.y,
            width: area.width.saturating_sub(horizontal_margin * 2),
            height: area.height,
        };
        let (lines, total) =
            self.transcript
                .visible_lines(t_inner.width, t_inner.height as usize, self.scroll_up);
        self.scroll_up = self
            .scroll_up
            .min(total.saturating_sub(t_inner.height as usize));
        frame.render_widget(Paragraph::new(lines), t_inner);
    }

    fn draw_slash_popup(&self, frame: &mut Frame, area: Rect, matches: &[&command::CommandSpec]) {
        frame.render_widget(Clear, area);
        let name_width = matches
            .iter()
            .map(|spec| spec.name.len() + 1)
            .max()
            .unwrap_or(0);
        let lines: Vec<Line> = matches
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(index, spec)| {
                let active = index == self.slash_selected.min(matches.len().saturating_sub(1));
                let style = if active {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(
                        format!(
                            "  {:<width$}",
                            format!("/{}", spec.name),
                            width = name_width
                        ),
                        style,
                    ),
                    Span::styled(spec.description, Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect, iv: &input::InputView) {
        let accent = if self.busy {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));
        let input_inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("›").style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            Rect {
                x: input_inner.x + 1,
                width: 1,
                ..input_inner
            },
        );
        let text_area = Rect {
            x: input_inner.x + 3,
            width: input_inner.width.saturating_sub(4),
            ..input_inner
        };
        if self.input.is_empty() {
            frame.render_widget(
                Paragraph::new("向 Onemore 提问，输入 / 查看命令")
                    .style(Style::default().fg(Color::DarkGray)),
                text_area,
            );
        } else {
            let rows: Vec<Line> = iv.rows.iter().map(|r| Line::raw(r.clone())).collect();
            frame.render_widget(Paragraph::new(rows), text_area);
        }
        frame.set_cursor_position((
            text_area.x + iv.cursor_col.min(text_area.width.saturating_sub(1)),
            text_area.y + (iv.cursor_row as u16).min(text_area.height.saturating_sub(1)),
        ));
    }

    fn status_line(&self) -> Paragraph<'static> {
        let dim = Style::default().fg(Color::DarkGray);
        let mut spans: Vec<Span> = Vec::new();
        if self.busy {
            spans.push(Span::styled(
                SPINNER[self.spinner_frame].to_string(),
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::styled(
                format!(" {} ", self.status_note),
                Style::default().fg(Color::Cyan),
            ));
        } else {
            spans.push(Span::styled("  ready ", Style::default().fg(Color::Green)));
        }
        spans.push(Span::styled(format!("  {} ", self.provider_label), dim));
        spans.push(Span::styled(
            format!(
                "  ↑{} ↓{} ",
                util::fmt_tokens(self.usage.0),
                util::fmt_tokens(self.usage.1)
            ),
            dim,
        ));
        let hint = if self.quit_armed_at.is_some() {
            "  再按一次 Ctrl+C 退出".to_string()
        } else if self.scroll_up > 0 {
            format!("  已上翻 {} 行,Esc 回到底部", self.scroll_up)
        } else if self.busy {
            "  Esc 取消".to_string()
        } else {
            "  Enter 发送 · Shift+Enter 换行 · / 命令".to_string()
        };
        spans.push(Span::styled(hint, dim.add_modifier(Modifier::DIM)));
        Paragraph::new(Line::from(spans))
    }
}

/// Ratatui 0.29 的 inline `insert_before` 会把临时 Buffer 的每一格都交给 backend，
/// 不像正常 diff 渲染那样跳过宽字符的 continuation cell。把这些占位格改为空
/// symbol，避免它们在中文字符的第二列重新打印一个空格。
fn clear_wide_continuation_cells(buffer: &mut ratatui::buffer::Buffer) {
    for y in buffer.area.top()..buffer.area.bottom() {
        let mut continuation = 0usize;
        for x in buffer.area.left()..buffer.area.right() {
            if continuation > 0 {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol("");
                }
                continuation -= 1;
                continue;
            }
            continuation = buffer
                .cell((x, y))
                .map(|cell| UnicodeWidthStr::width(cell.symbol()).saturating_sub(1))
                .unwrap_or(0);
        }
    }
}

fn draw_model_input(frame: &mut Frame, area: Rect, input: &InputBox) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " 自定义模型 ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [label_area, input_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new("  输入服务端支持的模型名称"), label_area);
    let view = input.view(input_area.width.saturating_sub(4), 1);
    let value = view.rows.first().cloned().unwrap_or_default();
    let line = if value.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::styled("例如 gpt-5", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(value),
        ])
    };
    frame.render_widget(Paragraph::new(line), input_area);
    frame.render_widget(
        Paragraph::new("  Enter 确认  Esc 返回").style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );
    frame.set_cursor_position((
        input_area.x + 2 + view.cursor_col.min(input_area.width.saturating_sub(3)),
        input_area.y,
    ));
}

fn draw_loading(frame: &mut Frame, area: Rect, title: &str) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ⠋ ", Style::default().fg(Color::Cyan)),
            Span::styled(title.to_string(), Style::default().fg(Color::DarkGray)),
        ])),
        inner,
    );
}

fn draw_approval(frame: &mut Frame, area: Rect, request: &ApprovalRequest) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " 工具审批 ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = vec![
        Line::from(vec![
            Span::styled("  工具  ", Style::default().fg(Color::DarkGray)),
            Span::styled(&request.tool, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("  参数  ", Style::default().fg(Color::DarkGray)),
            Span::raw(util::ellipsis(&request.summary, 180)),
        ]),
        Line::from(vec![
            Span::styled("  原因  ", Style::default().fg(Color::DarkGray)),
            Span::raw(request.reason.clone()),
        ]),
        Line::from(Span::styled(
            "  Enter/Y 本次允许   A 本会话相同调用   N/Esc 拒绝",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// 造一个没有真实 Runtime 的 App;返回事件发送端与命令接收端,
    /// 便于测试注入事件、断言提交行为。
    fn dummy_app() -> (
        App,
        std::sync::mpsc::Sender<AgentEvent>,
        std::sync::mpsc::Receiver<AgentCommand>,
        std::sync::mpsc::Receiver<ApprovalResponse>,
    ) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (approval_tx, approval_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let handle = RuntimeHandle {
            commands: cmd_tx,
            approvals: approval_tx,
            events: evt_rx,
            cancel: Arc::new(AtomicBool::new(false)),
            provider_label: "mock / test-model".into(),
            provider_names: vec!["mock".into()],
            model_names: vec!["test-model".into(), "other-model".into()],
            session_id: "12345678-1234-1234-1234-123456789abc".into(),
        };
        (App::new(handle), evt_tx, cmd_rx, approval_rx)
    }

    /// 完整过一遍事件流 + 输入操作 + 各种尺寸渲染,不允许 panic。
    #[test]
    fn renders_without_panic() {
        let (mut app, _evt, _cmd, _approvals) = dummy_app();
        // 模拟一轮对话的事件序列
        app.on_agent_event(AgentEvent::UserMessage(
            "读一下 main.rs,中文也要能换行显示".into(),
        ));
        app.on_agent_event(AgentEvent::TurnStarted);
        app.on_agent_event(AgentEvent::ThinkingDelta("让我想想……".into()));
        app.on_agent_event(AgentEvent::AssistantDelta("好的,".into()));
        app.on_agent_event(AgentEvent::AssistantDelta("我来读取。".into()));
        app.on_agent_event(AgentEvent::ToolCallPending {
            name: "read_file".into(),
        });
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "path=src/main.rs".into(),
        });
        app.on_agent_event(AgentEvent::ToolCallFinished {
            id: "t1".into(),
            name: "read_file".into(),
            output: crate::tools::ToolOutput::text(
                (1..=30)
                    .map(|i| format!("{} | line", i))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            error: None,
        });
        app.on_agent_event(AgentEvent::Usage {
            input_tokens: 1234,
            output_tokens: 567,
        });
        app.on_agent_event(AgentEvent::AssistantMessage(
            "好的,我来读取。完成了。".into(),
        ));
        app.on_agent_event(AgentEvent::Error("演示一个错误".into()));
        app.on_agent_event(AgentEvent::TurnFinished { cancelled: false });

        // 输入一些中英混排 + 多行
        for c in "写个 hello".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::SHIFT);
        for c in "第二行".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::PageUp, KeyModifiers::NONE);

        for (w, h) in [(80u16, 24u16), (120, 40), (20, 8), (10, 6), (9, 5)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
        }

        // 抽查:正常尺寸下画面里能看到状态栏的 provider 名
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let content = format!("{:?}", term.backend().buffer());
        assert!(content.contains("mock"), "状态栏应显示 provider 名");
    }

    /// 事件驱动的滚动边界:滚过头会被钳制,不会越界 panic。
    #[test]
    fn scroll_is_clamped() {
        let (mut app, _evt, _cmd, _approvals) = dummy_app();
        for i in 0..50 {
            app.on_agent_event(AgentEvent::Notice(format!("第 {} 条", i)));
        }
        app.scroll_up = 10_000;
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.scroll_up < 10_000, "渲染后 scroll 应被钳制");
    }

    /// Enter 必须发送——即使事件队列里躺着 Release 噪音(conpty 终端
    /// 会把按下/抬起同时入队,这曾导致 Enter 永远被当成换行)。
    #[test]
    fn enter_submits() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        for c in "你好".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        match cmd.try_recv() {
            Ok(AgentCommand::UserInput(t)) => assert_eq!(t, "你好"),
            other => panic!("应收到 UserInput,得到 {:?}", other),
        }
    }

    #[test]
    fn session_slash_commands_are_forwarded() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        app.handle_slash("session");
        assert!(matches!(cmd.recv().unwrap(), AgentCommand::ListSessions));

        app.handle_slash("session abc12345");
        match cmd.recv().unwrap() {
            AgentCommand::LoadSession(id) => assert_eq!(id, "abc12345"),
            other => panic!("应收到 LoadSession，得到 {:?}", other),
        }
    }

    #[test]
    fn session_list_becomes_an_interactive_picker() {
        use crate::storage::SessionSummary;

        let (mut app, _evt, cmd, _approvals) = dummy_app();
        app.handle_slash("session");
        assert!(matches!(cmd.recv().unwrap(), AgentCommand::ListSessions));
        assert!(matches!(
            app.overlay,
            Some(Overlay::Loading {
                kind: PickerKind::Session,
                ..
            })
        ));

        app.on_agent_event(AgentEvent::SessionsListed {
            current_id: "current-session".into(),
            sessions: vec![
                SessionSummary {
                    id: "current-session".into(),
                    title: "当前工作".into(),
                    message_count: 8,
                    updated_at: 2,
                },
                SessionSummary {
                    id: "older-session".into(),
                    title: "旧会话".into(),
                    message_count: 3,
                    updated_at: 1,
                },
            ],
        });
        assert!(matches!(
            app.overlay,
            Some(Overlay::Picker {
                kind: PickerKind::Session,
                ..
            })
        ));

        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            cmd.try_recv(),
            Ok(AgentCommand::LoadSession(id)) if id == "older-session"
        ));
    }

    /// 行尾反斜杠 + Enter = 续行,不发送;下一次 Enter 正常发送多行内容。
    #[test]
    fn backslash_enter_continues_line() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        for c in "第一行\\".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(cmd.try_recv().is_err(), "续行不应发送");
        for c in "第二行".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        match cmd.try_recv() {
            Ok(AgentCommand::UserInput(t)) => assert_eq!(t, "第一行\n第二行"),
            other => panic!("应收到 UserInput,得到 {:?}", other),
        }
    }

    #[test]
    fn slash_popup_navigates_completes_and_dispatches() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        app.on_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(app.selected_slash_command().unwrap().name, "provider");

        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "/provider ");
        for c in "mock".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        assert!(matches!(
            cmd.try_recv(),
            Ok(AgentCommand::SwitchProvider(name)) if name == "mock"
        ));
    }

    #[test]
    fn provider_picker_sends_selected_provider() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        app.handle_slash("provider");
        assert!(matches!(
            app.overlay,
            Some(Overlay::Picker {
                kind: PickerKind::Provider,
                ..
            })
        ));
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        match cmd.try_recv() {
            Ok(AgentCommand::SwitchProvider(name)) => assert_eq!(name, "mock"),
            other => panic!("应收到 SwitchProvider,得到 {:?}", other),
        }
    }

    #[test]
    fn model_picker_supports_custom_model_input() {
        let (mut app, _evt, cmd, _approvals) = dummy_app();
        app.handle_slash("model");
        // 从当前 test-model 越过另一个已配置模型，移动到“自定义模型”。
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::ModelInput(_))));
        for c in "new-model".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        match cmd.try_recv() {
            Ok(AgentCommand::SetModel(model)) => assert_eq!(model, "new-model"),
            other => panic!("应收到 SetModel,得到 {:?}", other),
        }
    }

    #[test]
    fn approval_overlay_uses_the_dedicated_response_channel() {
        let (mut app, _evt, _cmd, approvals) = dummy_app();
        app.on_agent_event(AgentEvent::PermissionRequested {
            request: ApprovalRequest {
                request_id: "approval-once".into(),
                tool: "dynamic_tool".into(),
                summary: "action=true".into(),
                reason: "external side effect".into(),
                scopes: vec![ApprovalScope::Once, ApprovalScope::Session],
            },
        });
        assert!(matches!(app.overlay, Some(Overlay::Approval(_))));
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            approvals.recv().unwrap(),
            ApprovalResponse {
                request_id: "approval-once".into(),
                decision: ApprovalDecision::Allow(ApprovalScope::Once),
            }
        );
        assert!(app.overlay.is_none());

        app.on_agent_event(AgentEvent::PermissionRequested {
            request: ApprovalRequest {
                request_id: "approval-deny".into(),
                tool: "dynamic_tool".into(),
                summary: String::new(),
                reason: "ask".into(),
                scopes: vec![ApprovalScope::Once],
            },
        });
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            approvals.recv().unwrap(),
            ApprovalResponse {
                request_id: "approval-deny".into(),
                decision: ApprovalDecision::Deny,
            }
        );
    }

    #[test]
    fn slash_and_picker_views_render_expected_content() {
        let (mut app, _evt, _cmd, _approvals) = dummy_app();
        app.input.set("/mo".into());
        let mut term = Terminal::new(TestBackend::new(72, 18)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let content = format!("{:?}", term.backend().buffer());
        assert!(content.contains("/model"));

        app.handle_slash("model");
        term.draw(|f| app.draw(f)).unwrap();
        let content = format!("{:?}", term.backend().buffer());
        assert!(content.contains("选择模型"));
        assert!(content.contains("自定义模型"));
    }

    #[test]
    fn scrollback_buffer_clears_wide_character_continuations() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        Paragraph::new("完成").render(area, &mut buffer);

        clear_wide_continuation_cells(&mut buffer);

        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "完");
        assert_eq!(buffer.cell((1, 0)).unwrap().symbol(), "");
        assert_eq!(buffer.cell((2, 0)).unwrap().symbol(), "成");
        assert_eq!(buffer.cell((3, 0)).unwrap().symbol(), "");
    }
}

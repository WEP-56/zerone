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
//!   带 Enter 的按键;同样靠上面的内容检查兜底。

mod input;
mod transcript;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::event::{AgentCommand, AgentEvent};
use crate::runtime::RuntimeHandle;
use crate::util;
use input::InputBox;
use transcript::Transcript;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 输入区内容最多显示的行数(超出滚动)。
const INPUT_MAX_ROWS: usize = 6;

const HELP_TEXT: &str = "可用命令:\n\
  /help              显示本帮助\n\
  /clear             清空会话(历史与画面)\n\
  /provider          列出可用 provider\n\
  /provider 名字     切换 provider(对话历史保留)\n\
  /model 模型名      修改当前 provider 的模型\n\
  /quit              退出\n\
按键:Enter 发送 · 换行用「行尾 \\ 再回车」(Shift+Enter 仅部分终端支持) · \
Esc 取消当前轮/回到底部 · PgUp/PgDn 滚动 · ↑/↓ 输入历史 · Ctrl+C×2 退出";

pub fn run(handle: RuntimeHandle) -> anyhow::Result<()> {
    let mut terminal = ratatui::init(); // 含 raw mode、备用屏、panic 钩子
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);

    let mut app = App::new(handle);
    app.transcript.push_notice(format!(
        "harness 已就绪({}),输入内容开始对话,/help 查看命令",
        app.provider_label
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

    /// 已从终端读出、还没处理的事件(Enter 的粘贴检测会预读一批进来)。
    pending_events: VecDeque<Event>,

    // 输入历史(↑/↓ 翻阅)
    history: Vec<String>,
    history_idx: Option<usize>,
    history_draft: String,

    busy: bool,
    status_note: String,
    provider_label: String,
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
        App {
            handle,
            transcript: Transcript::default(),
            input: InputBox::default(),
            pending_events: VecDeque::new(),
            history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            busy: false,
            status_note: String::new(),
            provider_label,
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
                self.on_terminal_event(ev);
                dirty = true;
                budget -= 1;
            }
            // 空闲时阻塞等待(33ms 超时 ≈ 渲染节拍)
            if !dirty && event::poll(Duration::from_millis(33))? {
                let ev = event::read()?;
                self.on_terminal_event(ev);
                dirty = true;
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

    fn on_terminal_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k.code, k.modifiers),
            Event::Paste(s) => self.input.insert_str(&s),
            Event::Resize(_, _) => {}
            _ => {}
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
            AgentEvent::ToolCallStarted { name, summary, .. } => {
                self.status_note = format!("执行 {}", name);
                self.transcript.push_tool(name, summary);
            }
            AgentEvent::ToolCallFinished {
                output, is_error, ..
            } => {
                self.status_note = "思考中".into();
                self.transcript.finish_tool(output, is_error);
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => self.usage = (input_tokens, output_tokens),
            AgentEvent::Notice(t) => self.transcript.push_notice(t),
            AgentEvent::Error(t) => self.transcript.push_error(t),
            AgentEvent::ConversationCleared => {
                self.transcript.clear();
                self.usage = (0, 0);
                self.transcript.push_notice("会话已清空".into());
            }
            AgentEvent::ProviderChanged { label } => {
                self.provider_label = label;
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

    // ---- 按键 ----

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.on_ctrl_c(),
            KeyCode::Char('l') if mods.contains(KeyModifiers::CONTROL) => {
                self.force_clear = true;
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => self.input.clear(),
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.input.insert_char(c);
            }
            KeyCode::Enter => {
                // ① 修饰键强制换行(传统 conhost 等能上报真实修饰键的终端)
                if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
                {
                    self.input.insert_char('\n');
                }
                // ② 行尾反斜杠 = 续行:在任何终端都可靠的手动换行方式
                else if self.input.pop_trailing_backslash() {
                    self.input.insert_char('\n');
                }
                // ③ 粘贴洪峰里的换行(判定逻辑见 enter_means_newline)
                else if self.enter_means_newline() {
                    self.input.insert_char('\n');
                } else {
                    self.submit();
                }
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
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
                if self.busy {
                    // 请求取消当前轮;Runtime 在下一个流事件/工具间隙生效
                    self.handle
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.status_note = "取消中…".into();
                } else if self.scroll_up > 0 {
                    self.scroll_up = 0;
                } else {
                    self.input.clear();
                }
            }
            _ => {}
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
            self.transcript
                .push_notice("上一轮仍在进行(Esc 可取消);稍候再发".into());
            self.input.set(raw);
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
        match head {
            "help" => self.transcript.push_notice(HELP_TEXT.into()),
            "quit" | "exit" => self.quit(),
            "clear" => {
                // 命令走通道排队,真正清空以 ConversationCleared 事件为准
                let _ = self.handle.commands.send(AgentCommand::ClearConversation);
            }
            "provider" => {
                if rest.is_empty() {
                    self.transcript.push_notice(format!(
                        "当前: {}\n可用: {}",
                        self.provider_label,
                        self.handle.provider_names.join(", ")
                    ));
                } else {
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::SwitchProvider(rest.to_string()));
                }
            }
            "model" => {
                if rest.is_empty() {
                    self.transcript
                        .push_notice(format!("当前: {}", self.provider_label));
                } else {
                    let _ = self
                        .handle
                        .commands
                        .send(AgentCommand::SetModel(rest.to_string()));
                }
            }
            other => self
                .transcript
                .push_error(format!("未知命令 /{},/help 查看可用命令", other)),
        }
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

        let inner_w = area.width.saturating_sub(2);
        let iv = self.input.view(inner_w, INPUT_MAX_ROWS);
        let input_h = (iv.total_rows.clamp(1, INPUT_MAX_ROWS) as u16) + 2;

        let [t_area, i_area, s_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.last_transcript_height = t_area.height;

        // 聊天区(左右各留 1 列呼吸感)
        let t_inner = Rect {
            x: t_area.x + 1,
            y: t_area.y,
            width: t_area.width.saturating_sub(2),
            height: t_area.height,
        };
        let (lines, total) =
            self.transcript
                .visible_lines(t_inner.width, t_inner.height as usize, self.scroll_up);
        self.scroll_up = self
            .scroll_up
            .min(total.saturating_sub(t_inner.height as usize));
        frame.render_widget(Paragraph::new(lines), t_inner);

        // 输入区
        let border_style = if self.busy {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let block = Block::bordered().border_style(border_style);
        let input_inner = block.inner(i_area);
        frame.render_widget(block, i_area);
        if self.input.is_empty() {
            frame.render_widget(
                Paragraph::new("输入消息…(/help 查看命令)")
                    .style(Style::default().fg(Color::DarkGray)),
                input_inner,
            );
        } else {
            let rows: Vec<Line> = iv.rows.iter().map(|r| Line::raw(r.clone())).collect();
            frame.render_widget(Paragraph::new(rows), input_inner);
        }
        frame.set_cursor_position((
            input_inner.x + iv.cursor_col.min(input_inner.width.saturating_sub(1)),
            input_inner.y + (iv.cursor_row as u16).min(input_inner.height.saturating_sub(1)),
        ));

        // 状态栏
        frame.render_widget(self.status_line(), s_area);
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
            spans.push(Span::styled("● 就绪 ", Style::default().fg(Color::Green)));
        }
        spans.push(Span::styled(format!("│ {} ", self.provider_label), dim));
        spans.push(Span::styled(
            format!(
                "│ ↑{} ↓{} ",
                util::fmt_tokens(self.usage.0),
                util::fmt_tokens(self.usage.1)
            ),
            dim,
        ));
        let hint = if self.quit_armed_at.is_some() {
            "│ 再按一次 Ctrl+C 退出".to_string()
        } else if self.scroll_up > 0 {
            format!("│ 已上翻 {} 行,Esc 回到底部", self.scroll_up)
        } else if self.busy {
            "│ Esc 取消".to_string()
        } else {
            "│ Enter 发送 · Shift+Enter 换行 · /help".to_string()
        };
        spans.push(Span::styled(hint, dim.add_modifier(Modifier::DIM)));
        Paragraph::new(Line::from(spans))
    }
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
    ) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let handle = RuntimeHandle {
            commands: cmd_tx,
            events: evt_rx,
            cancel: Arc::new(AtomicBool::new(false)),
            provider_label: "mock / test-model".into(),
            provider_names: vec!["mock".into()],
        };
        (App::new(handle), evt_tx, cmd_rx)
    }

    /// 完整过一遍事件流 + 输入操作 + 各种尺寸渲染,不允许 panic。
    #[test]
    fn renders_without_panic() {
        let (mut app, _evt, _cmd) = dummy_app();
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
            output: (1..=30)
                .map(|i| format!("{} | line", i))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
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
        let (mut app, _evt, _cmd) = dummy_app();
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
        let (mut app, _evt, cmd) = dummy_app();
        for c in "你好".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        match cmd.try_recv() {
            Ok(AgentCommand::UserInput(t)) => assert_eq!(t, "你好"),
            other => panic!("应收到 UserInput,得到 {:?}", other),
        }
    }

    /// 行尾反斜杠 + Enter = 续行,不发送;下一次 Enter 正常发送多行内容。
    #[test]
    fn backslash_enter_continues_line() {
        let (mut app, _evt, cmd) = dummy_app();
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
}

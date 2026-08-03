//! 聊天区:把事件流积累成"单元格"列表,并负责换行与渲染缓存。
//!
//! 一个 Cell 对应画面上一段内容(用户消息 / 助手消息 / 思考 / 工具调用 /
//! 提示 / 错误)。流式增量不断追加到"开放中"的 Cell 上。
//! 换行结果按 (宽度, 版本号) 缓存——只有变过的 Cell 才重新排版,
//! 长对话下每帧渲染依然轻量。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use textwrap::Options;

use crate::util;

/// 工具输出在聊天区最多显示的视觉行数(完整内容始终在消息历史里)。
const TOOL_PREVIEW_LINES: usize = 10;

pub enum Cell {
    User(String),
    Assistant {
        text: String,
        open: bool,
    },
    Thinking {
        text: String,
        open: bool,
    },
    Tool {
        name: String,
        summary: String,
        output: Option<String>,
        is_error: bool,
    },
    Notice(String),
    Error(String),
}

struct Entry {
    cell: Cell,
    version: u64,
    cache: Option<(u16, u64, Vec<Line<'static>>)>,
}

impl Entry {
    fn new(cell: Cell) -> Self {
        Entry {
            cell,
            version: 0,
            cache: None,
        }
    }

    fn touch(&mut self) {
        self.version += 1;
    }

    fn lines(&mut self, width: u16) -> &Vec<Line<'static>> {
        let valid = matches!(&self.cache, Some((w, v, _)) if *w == width && *v == self.version);
        if !valid {
            let lines = build_lines(&self.cell, width);
            self.cache = Some((width, self.version, lines));
        }
        &self.cache.as_ref().unwrap().2
    }
}

#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

impl Transcript {
    pub fn push_user(&mut self, text: String) {
        self.entries.push(Entry::new(Cell::User(text)));
    }

    pub fn push_notice(&mut self, text: String) {
        self.entries.push(Entry::new(Cell::Notice(text)));
    }

    pub fn push_error(&mut self, text: String) {
        self.entries.push(Entry::new(Cell::Error(text)));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 追加助手文本增量;没有开放中的助手 Cell 就新开一个。
    pub fn append_assistant(&mut self, delta: &str) {
        if let Some(e) = self.entries.last_mut() {
            if let Cell::Assistant { text, open: true } = &mut e.cell {
                text.push_str(delta);
                e.touch();
                return;
            }
        }
        self.entries.push(Entry::new(Cell::Assistant {
            text: delta.to_string(),
            open: true,
        }));
    }

    /// 思考增量(同上,但思考与正文可能交替出现,所以各自独立成 Cell)。
    pub fn append_thinking(&mut self, delta: &str) {
        if let Some(e) = self.entries.last_mut() {
            if let Cell::Thinking { text, open: true } = &mut e.cell {
                text.push_str(delta);
                e.touch();
                return;
            }
        }
        self.entries.push(Entry::new(Cell::Thinking {
            text: delta.to_string(),
            open: true,
        }));
    }

    /// 助手消息完成:用全文校正开放中的 Cell(流式增量偶有丢失时兜底)。
    pub fn finalize_assistant(&mut self, full: String) {
        for e in self.entries.iter_mut().rev() {
            if let Cell::Assistant { text, open } = &mut e.cell {
                if *open {
                    *text = full;
                    *open = false;
                    e.touch();
                    return;
                }
                break;
            }
        }
        if !full.is_empty() {
            self.entries.push(Entry::new(Cell::Assistant {
                text: full,
                open: false,
            }));
        }
    }

    /// 关闭所有开放中的 Cell(一轮结束时调用)。
    pub fn close_open_cells(&mut self) {
        for e in self.entries.iter_mut() {
            match &mut e.cell {
                Cell::Assistant { open, .. } | Cell::Thinking { open, .. } if *open => {
                    *open = false;
                    e.touch();
                }
                _ => {}
            }
        }
    }

    pub fn push_tool(&mut self, name: String, summary: String) {
        self.entries.push(Entry::new(Cell::Tool {
            name,
            summary,
            output: None,
            is_error: false,
        }));
    }

    /// 把结果填进最近一个"运行中"的工具 Cell。
    pub fn finish_tool(&mut self, output: String, is_error: bool) {
        for e in self.entries.iter_mut().rev() {
            if let Cell::Tool {
                output: slot @ None,
                is_error: err_flag,
                ..
            } = &mut e.cell
            {
                *slot = Some(output);
                *err_flag = is_error;
                e.touch();
                return;
            }
        }
    }

    /// 取可视窗口内的行。返回 (行, 总行数)。
    /// `scroll_up` = 从底部向上滚了多少行(0 = 贴底跟随)。
    pub fn visible_lines(
        &mut self,
        width: u16,
        height: usize,
        scroll_up: usize,
    ) -> (Vec<Line<'static>>, usize) {
        let total: usize = self.entries.iter_mut().map(|e| e.lines(width).len()).sum();
        let max_scroll = total.saturating_sub(height);
        let scroll_up = scroll_up.min(max_scroll);
        let start = total.saturating_sub(height + scroll_up);
        let end = (start + height).min(total);

        let mut out = Vec::with_capacity(height);
        let mut idx = 0usize;
        for e in self.entries.iter_mut() {
            let lines = e.lines(width);
            let n = lines.len();
            if idx + n > start && idx < end {
                for (i, l) in lines.iter().enumerate() {
                    let abs = idx + i;
                    if abs >= start && abs < end {
                        out.push(l.clone());
                    }
                }
            }
            idx += n;
            if idx >= end {
                break;
            }
        }
        (out, total)
    }
}

// ---- 排版 ----

fn style_user() -> Style {
    Style::default().fg(Color::Cyan)
}
fn style_thinking() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}
fn style_tool_head(is_error: bool) -> Style {
    if is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Magenta)
    }
}
fn style_dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// 把一段多行文本按宽度换行,首行/续行可带不同前缀,整体一个样式。
fn wrap_styled(
    text: &str,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
    style: Style,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, para) in text.split('\n').enumerate() {
        let head = if i == 0 { first_prefix } else { cont_prefix };
        if para.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        let opts = Options::new(width)
            .initial_indent(head)
            .subsequent_indent(cont_prefix);
        for piece in textwrap::wrap(para, opts) {
            out.push(Line::styled(piece.into_owned(), style));
        }
    }
    out
}

fn build_lines(cell: &Cell, width: u16) -> Vec<Line<'static>> {
    let w = (width as usize).max(8);
    let mut lines = match cell {
        Cell::User(t) => wrap_styled(t, w, "❯ ", "  ", style_user()),
        Cell::Assistant { text, .. } => {
            if text.is_empty() {
                Vec::new()
            } else {
                wrap_styled(text, w, "", "", Style::default())
            }
        }
        Cell::Thinking { text, .. } => {
            if text.is_empty() {
                Vec::new()
            } else {
                wrap_styled(text, w, "· ", "  ", style_thinking())
            }
        }
        Cell::Tool {
            name,
            summary,
            output,
            is_error,
        } => {
            let head = if summary.is_empty() {
                name.clone()
            } else {
                format!("{}({})", name, summary)
            };
            let mut v = wrap_styled(&head, w, "● ", "  ", style_tool_head(*is_error));
            match output {
                None => v.push(Line::styled("  运行中…", style_dim())),
                Some(out) => {
                    let body_style = if *is_error {
                        Style::default().fg(Color::Red).add_modifier(Modifier::DIM)
                    } else {
                        style_dim()
                    };
                    let mut shown = 0usize;
                    let logical: Vec<&str> = out.lines().collect();
                    for line in &logical {
                        if shown >= TOOL_PREVIEW_LINES {
                            break;
                        }
                        let wrapped = wrap_styled(
                            &util::ellipsis(line, w.saturating_sub(2).max(8) * 2),
                            w,
                            "  ",
                            "  ",
                            body_style,
                        );
                        shown += wrapped.len().max(1);
                        v.extend(wrapped);
                    }
                    let hidden = logical.len().saturating_sub(shown.min(logical.len()));
                    if hidden > 0 {
                        v.push(Line::styled(
                            format!("  … (+{} 行,完整内容已回传给模型)", hidden),
                            style_dim(),
                        ));
                    }
                }
            }
            v
        }
        Cell::Notice(t) => wrap_styled(t, w, "· ", "  ", style_dim()),
        Cell::Error(t) => wrap_styled(t, w, "✖ ", "  ", Style::default().fg(Color::Red)),
    };
    if lines.is_empty() {
        return lines;
    }
    lines.push(Line::raw("")); // 单元格之间空一行
    lines
}

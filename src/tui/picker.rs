//! Compact selection view shared by provider and model switching.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub label: String,
    pub description: String,
    pub value: Option<String>,
    pub current: bool,
}

#[derive(Debug)]
pub struct Picker {
    pub title: String,
    pub items: Vec<PickerItem>,
    filter: String,
    selected: usize,
    scroll: usize,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let selected = items.iter().position(|item| item.current).unwrap_or(0);
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            selected,
            scroll: 0,
        }
    }

    pub fn preferred_height(&self) -> u16 {
        (self.filtered_indices().len().min(8) as u16 + 4).max(6)
    }

    pub fn move_up(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            len - 1
        } else {
            self.selected - 1
        };
        self.keep_visible();
    }

    pub fn move_down(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected + 1) % len;
        self.keep_visible();
    }

    pub fn push_filter(&mut self, c: char) {
        if !c.is_control() {
            self.filter.push(c);
            self.reset_selection();
        }
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.reset_selection();
    }

    pub fn selected(&self) -> Option<PickerItem> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected)
            .map(|index| self.items[*index].clone())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let border = Style::default().fg(Color::DarkGray);
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border)
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let available = inner.height.saturating_sub(2) as usize;
        let indices = self.filtered_indices();
        let label_width = indices
            .iter()
            .map(|index| UnicodeWidthStr::width(self.items[*index].label.as_str()))
            .max()
            .unwrap_or(0)
            .min((inner.width as usize / 2).max(8));
        let mut lines = Vec::new();
        if self.filter.is_empty() {
            lines.push(Line::styled(
                "  输入可筛选",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  搜索 ", Style::default().fg(Color::DarkGray)),
                Span::raw(self.filter.clone()),
            ]));
        }

        for (visible, item_index) in indices.iter().enumerate().skip(self.scroll).take(available) {
            let item = &self.items[*item_index];
            let active = visible == self.selected;
            let marker = if active { "›" } else { " " };
            let current = if item.current { " ✓" } else { "" };
            let fitted = fit_to_width(&item.label, label_width);
            let padding = label_width.saturating_sub(UnicodeWidthStr::width(fitted.as_str()));
            let padded = format!("{fitted}{}", " ".repeat(padding));
            let row_style = if active {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} {padded}{current}"), row_style),
                Span::styled(
                    format!("  {}", item.description),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        if indices.is_empty() {
            lines.push(Line::styled(
                "  没有匹配项",
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::styled(
            "  ↑↓ 选择  Enter 确认  Esc 返回",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                needle.is_empty()
                    || item.label.to_lowercase().contains(&needle)
                    || item.description.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    fn keep_visible(&mut self) {
        const ROWS: usize = 8;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + ROWS {
            self.scroll = self.selected + 1 - ROWS;
        }
    }
}

fn fit_to_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + char_width > content_width {
            break;
        }
        out.push(c);
        width += char_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            "模型",
            vec![
                PickerItem {
                    label: "gpt-5".into(),
                    description: "current".into(),
                    value: Some("gpt-5".into()),
                    current: true,
                },
                PickerItem {
                    label: "claude-sonnet".into(),
                    description: String::new(),
                    value: Some("claude-sonnet".into()),
                    current: false,
                },
            ],
        )
    }

    #[test]
    fn selection_wraps_and_filters() {
        let mut p = picker();
        p.move_up();
        assert_eq!(
            p.selected().unwrap().value.as_deref(),
            Some("claude-sonnet")
        );
        p.push_filter('g');
        assert_eq!(p.selected().unwrap().value.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn labels_are_fitted_by_terminal_width() {
        assert_eq!(fit_to_width("自定义模型", 7), "自定义…");
        assert_eq!(
            UnicodeWidthStr::width(fit_to_width("abcdefgh", 5).as_str()),
            5
        );
    }
}

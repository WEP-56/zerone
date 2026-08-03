//! 输入框:一个支持多行、CJK 宽度感知的小型行编辑器。
//!
//! 数据结构刻意最简:一个 `String` + 光标字节偏移(始终落在字符边界)。
//! 渲染时把内容按可用宽度折成视觉行,并算出光标的 (行, 列);
//! 中文等宽字符占两列,由 unicode-width 提供宽度。

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct InputBox {
    text: String,
    /// 字节偏移,总是位于字符边界。
    cursor: usize,
}

/// 渲染结果:窗口内的视觉行 + 光标在窗口内的位置。
pub struct InputView {
    pub rows: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: u16,
    /// 总视觉行数(用于决定输入区高度)。
    pub total_rows: usize,
}

impl InputBox {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        match c {
            '\n' => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            '\t' => self.insert_str("    "),
            c if c.is_control() => {}
            c => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
        }
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            // 粘贴多行文本时保留换行(不会触发发送)
            if c == '\r' {
                continue;
            }
            self.insert_char(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    /// 到当前逻辑行行首。
    pub fn move_home(&mut self) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = line_start;
    }

    /// 到当前逻辑行行尾。
    pub fn move_end(&mut self) {
        let rel = self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
        self.cursor += rel;
    }

    /// 上/下移动一个逻辑行(尽量保持列号,按字符数)。
    pub fn move_vertical(&mut self, up: bool) {
        let lines: Vec<&str> = self.text.split('\n').collect();
        // 定位当前 (行号, 行内字符列)
        let mut offset = 0usize;
        let mut row = 0usize;
        let mut col_chars = 0usize;
        for (i, l) in lines.iter().enumerate() {
            let end = offset + l.len();
            if self.cursor <= end {
                row = i;
                col_chars = self.text[offset..self.cursor].chars().count();
                break;
            }
            offset = end + 1; // 跳过 \n
        }
        let target = if up {
            if row == 0 {
                return;
            }
            row - 1
        } else {
            if row + 1 >= lines.len() {
                return;
            }
            row + 1
        };
        // 目标行行首字节偏移
        let mut start = 0usize;
        for l in lines.iter().take(target) {
            start += l.len() + 1;
        }
        let target_line = lines[target];
        let take = col_chars.min(target_line.chars().count());
        let byte_in_line: usize = target_line.chars().take(take).map(|c| c.len_utf8()).sum();
        self.cursor = start + byte_in_line;
    }

    fn prev_boundary(&self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        if self.cursor >= self.text.len() {
            return None;
        }
        let c = self.text[self.cursor..].chars().next()?;
        Some(self.cursor + c.len_utf8())
    }

    /// 折行 + 光标定位 + 窗口裁剪。`inner_w` 为内容区宽度(列),
    /// `max_rows` 为最多显示的行数(超出时滚动到光标所在行)。
    pub fn view(&self, inner_w: u16, max_rows: usize) -> InputView {
        let w = inner_w.max(2) as usize;
        let mut rows: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut cur_width = 0usize;
        let mut cursor_pos: Option<(usize, u16)> = None;

        for (idx, c) in self.text.char_indices() {
            if idx == self.cursor {
                cursor_pos = Some((rows.len(), cur_width as u16));
            }
            if c == '\n' {
                rows.push(std::mem::take(&mut cur));
                cur_width = 0;
                continue;
            }
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if cur_width + cw > w {
                rows.push(std::mem::take(&mut cur));
                cur_width = 0;
            }
            cur.push(c);
            cur_width += cw;
        }
        if self.cursor >= self.text.len() {
            // 光标在末尾;若正好压在右边界,换到下一行行首
            if cur_width >= w {
                rows.push(std::mem::take(&mut cur));
                cur_width = 0;
            }
            cursor_pos = Some((rows.len(), cur_width as u16));
        }
        rows.push(cur);

        let (cursor_row, cursor_col) = cursor_pos.unwrap_or((0, 0));
        let total_rows = rows.len();
        // 窗口:保证光标行可见
        let start = (cursor_row + 1).saturating_sub(max_rows);
        let end = (start + max_rows).min(total_rows);
        InputView {
            rows: rows[start..end].to_vec(),
            cursor_row: cursor_row - start,
            cursor_col,
            total_rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_move_cjk() {
        let mut b = InputBox::default();
        b.insert_str("你好a");
        assert_eq!(b.text, "你好a");
        b.move_left();
        b.move_left();
        b.insert_char('X');
        assert_eq!(b.text, "你X好a");
        b.backspace();
        assert_eq!(b.text, "你好a");
    }

    #[test]
    fn view_wraps_by_display_width() {
        let mut b = InputBox::default();
        b.insert_str("你好你好"); // 8 列宽
        let v = b.view(4, 8); // 每行放 2 个汉字
        assert_eq!(v.rows, vec!["你好", "你好", ""]);
        // 光标在末尾:第 4 列压线,折到下一行行首
        assert_eq!((v.cursor_row, v.cursor_col), (2, 0));
    }

    #[test]
    fn vertical_move_keeps_column() {
        let mut b = InputBox::default();
        b.insert_str("abcd\nxy");
        // 光标在末尾(第 2 行第 2 列)
        b.move_vertical(true);
        b.insert_char('!');
        assert_eq!(b.text, "ab!cd\nxy");
    }
}

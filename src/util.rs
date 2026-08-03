//! 小而通用的文本处理函数:清洗控制字符、按行/字节截断、参数摘要。
//! 工具输出与流式文本都要过这里,保证喂给 TUI 和模型的内容"无害"。

/// 清洗终端控制序列与控制字符:
/// - 去掉 ANSI CSI(`ESC [ ... 字母`)与 OSC(`ESC ] ... BEL/ST`)序列,
///   否则子进程输出里的彩色码会直接把 TUI 画花;
/// - Tab 展开为 4 空格(ratatui 对 \t 宽度处理不可靠);
/// - 去掉 \r(统一 \n 换行);
/// - 其余 C0 控制字符丢弃。
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC [ 参数字节... 直到 0x40..=0x7E 的终止字节
                Some('[') => {
                    chars.next();
                    for t in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&t) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... 直到 BEL 或 ESC \
                Some(']') => {
                    chars.next();
                    while let Some(t) = chars.next() {
                        if t == '\u{07}' {
                            break;
                        }
                        if t == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // 其他 ESC 序列:再吞一个字符了事
                _ => {
                    chars.next();
                }
            },
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// 按字符数截断,超出部分保留"头 + 尾"(报错通常在尾部,如编译日志),
/// 中间插入标记。工具结果回给模型前都要过这一层,防止撑爆上下文。
pub fn truncate_middle(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let head_n = max_chars / 2;
    let tail_n = max_chars - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(total - tail_n).collect();
    format!(
        "{}\n\n……[输出过长,已省略中间 {} 个字符]……\n\n{}",
        head,
        total - max_chars,
        tail
    )
}

/// 单行截断(显示摘要用),超长以 … 结尾。
pub fn ellipsis(s: &str, max_chars: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 把工具入参压成一行人类可读摘要,用于 `● read_file(path=src/main.rs)` 这类显示。
pub fn args_summary(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let vs = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{}={}", k, ellipsis(&vs, 48))
                })
                .collect();
            ellipsis(&parts.join(", "), 120)
        }
        other => ellipsis(&other.to_string(), 120),
    }
}

/// 格式化 token 数:1234 -> "1.2k"。状态栏用。
pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi() {
        assert_eq!(sanitize("a\u{1b}[31mred\u{1b}[0mb"), "aredb");
        assert_eq!(sanitize("x\u{1b}]0;title\u{07}y"), "xy");
        assert_eq!(sanitize("a\tb\r\nc"), "a    b\nc");
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "0123456789".repeat(10); // 100 字符
        let t = truncate_middle(&s, 20);
        assert!(t.starts_with("0123456789"));
        assert!(t.ends_with("0123456789"));
        assert!(t.contains("已省略中间 80 个字符"));
        // 不超限时原样返回
        assert_eq!(truncate_middle("abc", 10), "abc");
    }

    #[test]
    fn ellipsis_flattens_newlines() {
        assert_eq!(ellipsis("ab\ncd", 10), "ab cd");
        assert_eq!(ellipsis("abcdef", 4), "abc…");
    }
}

//! SSE(Server-Sent Events)解析器。
//!
//! 三种 API 的流式响应都是 SSE,格式差异只在事件内容,所以解析器只有一个:
//!
//! ```text
//! event: content_block_delta          ← 可选的事件名(Anthropic/Responses 用)
//! data: {"type":"..."}                ← 数据行,可多行(拼接时以 \n 相连)
//!                                     ← 空行 = 一个事件结束
//! : ping                              ← 冒号开头是注释,忽略
//! ```
//!
//! 用同步阻塞读实现:`next_event()` 阻塞到读出一个完整事件为止。
//! 上层在每个事件之间检查取消标志——这意味着取消最迟在下一个事件
//! 到达时生效(模型吐 token 很频繁,体感几乎即时)。

use std::io::{BufRead, BufReader, Read};

#[derive(Debug, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub struct SseReader<R: Read> {
    reader: BufReader<R>,
}

impl<R: Read> SseReader<R> {
    pub fn new(inner: R) -> Self {
        SseReader {
            reader: BufReader::new(inner),
        }
    }

    /// 读下一个完整事件;流结束返回 Ok(None)。
    pub fn next_event(&mut self) -> std::io::Result<Option<SseEvent>> {
        let mut event: Option<String> = None;
        let mut data_lines: Vec<String> = Vec::new();

        loop {
            // 用 read_until 而不是 read_line:后者遇到非 UTF-8 直接报错,
            // 这里宽容处理(lossy),坏字节不至于断掉整个流
            let mut raw = Vec::new();
            let n = self.reader.read_until(b'\n', &mut raw)?;
            if n == 0 {
                // EOF:如果手里攒着半个事件就交出去
                if !data_lines.is_empty() || event.is_some() {
                    return Ok(Some(SseEvent {
                        event,
                        data: data_lines.join("\n"),
                    }));
                }
                return Ok(None);
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);

            if line.is_empty() {
                // 空行:事件边界
                if !data_lines.is_empty() || event.is_some() {
                    return Ok(Some(SseEvent {
                        event,
                        data: data_lines.join("\n"),
                    }));
                }
                continue; // 连续空行,继续等
            }
            if let Some(rest) = line.strip_prefix(':') {
                let _ = rest; // 注释行(keep-alive ping),忽略
                continue;
            }
            if let Some(v) = strip_field(line, "data") {
                data_lines.push(v.to_string());
            } else if let Some(v) = strip_field(line, "event") {
                event = Some(v.to_string());
            }
            // 其余字段(id:、retry:)本项目用不到,忽略
        }
    }
}

/// 解析 `field: value` 行;SSE 规范允许冒号后有一个可选空格。
fn strip_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(field)?;
    let rest = rest.strip_prefix(':')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &str) -> Vec<SseEvent> {
        let mut r = SseReader::new(input.as_bytes());
        let mut out = vec![];
        while let Some(e) = r.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn parses_named_events() {
        let events = read_all("event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: {}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn parses_dataonly_and_done() {
        let events = read_all("data: {\"x\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, None);
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn handles_crlf_comments_multiline() {
        let events = read_all(": keep-alive\r\ndata: line1\r\ndata: line2\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn emits_pending_event_on_eof() {
        let events = read_all("data: tail-no-blank-line");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail-no-blank-line");
    }
}

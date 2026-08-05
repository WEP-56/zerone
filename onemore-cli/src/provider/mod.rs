//! # 模型层:Provider trait 与三个 API 适配器
//!
//! [`Provider`] 是 Runtime 与"某家 LLM API"之间的唯一接口:
//! 输入统一的 [`PromptContext`] + 工具声明,输出统一的流事件与
//! [`TurnOutput`]。**新增 Provider 不需要改 Agent Loop**——
//! 实现 trait、在 [`build_provider`] 的 match 里加一行即可
//! (见 docs/07-how-to-add-provider.md)。
//!
//! 三个适配器各自负责两件事(方向相反的两次翻译):
//! 1. 把统一消息模型(`message::ChatMessage/Block`)编码成该 API 的请求体;
//! 2. 把该 API 的 SSE 流解码回统一模型 + 统一流事件。
//!
//! | 概念       | Messages(Anthropic)   | Chat Completions      | Responses(OpenAI)        |
//! |-----------|------------------------|-----------------------|---------------------------|
//! | 端点       | /v1/messages           | /v1/chat/completions  | /v1/responses             |
//! | 系统提示   | 顶层 system 字段        | messages[0] role=system | 顶层 instructions 字段  |
//! | 工具声明   | {name,input_schema}    | {type:function,function:{...}} | {type:function,...} 平铺 |
//! | 工具调用   | tool_use 内容块         | tool_calls 数组        | function_call 输出项       |
//! | 工具结果   | user 消息内 tool_result | role:"tool" 消息       | function_call_output 输入项|
//! | 流结束     | message_stop 事件       | data: [DONE]           | response.completed 事件    |

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;
pub mod sse;

use std::io::Read;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use crate::config::{ApiKind, ProviderSettings};
use crate::context::PromptContext;
use crate::message::{ChatMessage, StopReason, Usage};
use crate::tools::ToolSpec;

/// 流式过程中发给上层的增量事件(Runtime 会转成 AgentEvent)。
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// 模型开始生成一次工具调用(参数还在流式拼接中,先让 UI 有反馈)。
    ToolCallBegun {
        name: String,
    },
}

/// 一次模型调用的完整产出。
#[derive(Debug)]
pub struct TurnOutput {
    pub message: ChatMessage,
    pub usage: Usage,
    pub stop: StopReason,
}

/// Provider 层错误。`retryable` 标记网络/限流/服务端故障这类
/// "重试可能就好了"的错误;Runtime 只在尚未收到任何流事件时才重试。
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub fn fatal(message: impl Into<String>) -> Self {
        ProviderError {
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub trait Provider: Send {
    /// 状态栏显示用,如 `anthropic / claude-sonnet-5`。
    fn label(&self) -> String;
    fn model(&self) -> &str;
    fn set_model(&mut self, model: String);

    /// Provider 的唯一公共调用边界：每次调用必然终止于 Done、Error 或 Aborted。
    fn stream_turn(
        &self,
        prompt: &PromptContext,
        tools: &[ToolSpec],
        on_event: &mut dyn FnMut(ProviderEvent),
        cancel: &AtomicBool,
    ) -> StreamTerminal;
}

/// 一次 Provider 调用的唯一终止结果。
#[derive(Debug)]
pub enum StreamTerminal {
    Done(TurnOutput),
    Error(FailedTurn),
    Aborted(FailedTurn),
}

/// Provider 失败时仍可供上层消费的最终结果。
#[derive(Debug)]
pub struct FailedTurn {
    pub message: ChatMessage,
    pub error: ProviderError,
}

impl FailedTurn {
    pub fn from_error(error: ProviderError) -> Self {
        FailedTurn {
            message: ChatMessage::empty_assistant(),
            error,
        }
    }

    pub fn aborted() -> Self {
        FailedTurn::from_error(ProviderError::fatal("模型调用已取消"))
    }
}

/// 工厂:按配置实例化 Provider。新增一家在这里加一个 match 分支。
pub fn build_provider(settings: ProviderSettings) -> Box<dyn Provider> {
    match settings.api {
        ApiKind::Messages => Box::new(anthropic::AnthropicProvider::new(settings)),
        ApiKind::Chat => Box::new(openai_chat::ChatProvider::new(settings)),
        ApiKind::Responses => Box::new(openai_responses::ResponsesProvider::new(settings)),
    }
}

// ---------------------------------------------------------------------------
// 三个适配器共用的 HTTP 与小工具
// ---------------------------------------------------------------------------

/// 构造带超时与代理的 HTTP agent。
/// 代理从环境变量读取(HTTPS_PROXY / HTTP_PROXY / ALL_PROXY),
/// 国内网络访问境外 API 通常需要它。
pub(crate) fn http_agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        // SSE 是长连接,read 超时要放宽;推理模型思考期间可能长时间无数据
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(60));
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
    ] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                if let Ok(proxy) = ureq::Proxy::new(v.trim()) {
                    builder = builder.proxy(proxy);
                }
                break;
            }
        }
    }
    builder.build()
}

/// POST 一个 JSON 请求,返回 SSE 字节流(调用方套上 SseReader)。
/// 非 2xx 会读出响应体、提取各家错误信息,并按状态码标记是否可重试。
pub(crate) fn post_sse(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, String)],
    body: &Value,
) -> Result<Box<dyn Read + Send + Sync + 'static>, ProviderError> {
    let mut req = agent.post(url).set("content-type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => Ok(resp.into_reader()),
        Err(ureq::Error::Status(code, resp)) => {
            let retry_after = retry_after_hint(
                code,
                resp.header("retry-after-ms"),
                resp.header("retry-after"),
            );
            let text = resp.into_string().unwrap_or_default();
            Err(ProviderError {
                message: format!("HTTP {}: {}", code, extract_api_error(&text)),
                retryable: matches!(code, 408 | 429) || code >= 500,
                retry_after,
            })
        }
        Err(ureq::Error::Transport(t)) => Err(ProviderError {
            message: format!("网络错误: {}", t),
            retryable: true,
            retry_after: None,
        }),
    }
}

/// 解析服务器建议的等待时长。只在 Retry-After 语义成立的状态码
/// (429 限流、503 过载、408 超时)上采信;网关 502 之类的错误页也常
/// 携带 `Retry-After: 60`,那是 CDN 模板噪声,采信它会让重试白等一分钟,
/// 应改走自家指数退避。毫秒头(Anthropic 等)优先于秒级标准头。
fn retry_after_hint(
    status: u16,
    ms_header: Option<&str>,
    secs_header: Option<&str>,
) -> Option<Duration> {
    if !matches!(status, 408 | 429 | 503) {
        return None;
    }
    ms_header
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| {
            secs_header
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
        })
}

/// 从错误响应体里挖出人话。兼容两种主流形状:
/// OpenAI `{"error":{"message":...}}`、Anthropic `{"error":{"type":..,"message":...}}`。
/// 网关/CDN 的 HTML 错误页只保留 `<title>`,绝不把整页标签倒进通知栏。
fn extract_api_error(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for path in [&["error", "message"][..], &["message"][..]] {
            let mut cur = &v;
            let mut ok = true;
            for key in path {
                match cur.get(key) {
                    Some(next) => cur = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some(s) = cur.as_str() {
                    return s.to_string();
                }
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(空响应体)".to_string();
    }
    if looks_like_html(trimmed) {
        return match html_title(trimmed) {
            Some(title) => format!("{}(HTML 错误页,正文已省略)", title),
            None => format!("(HTML 错误页,{} 字符,已省略)", trimmed.chars().count()),
        };
    }
    crate::util::ellipsis(&collapse_whitespace(trimmed), 400)
}

fn looks_like_html(body: &str) -> bool {
    let head: String = body.chars().take(256).collect::<String>().to_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<html")
}

/// 取 `<title>` 内容(大小写不敏感,折叠空白,限长)。
fn html_title(body: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let open = lower.find("<title")?;
    let open_end = body[open..].find('>')? + open + 1;
    let close = lower[open_end..].find("</title>")? + open_end;
    let title = collapse_whitespace(body[open_end..close].trim());
    (!title.is_empty()).then(|| crate::util::ellipsis(&title, 120))
}

/// 把连续空白(含换行)折叠成单个空格,保证错误信息是干净的一行。
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 拼接 API URL:兼容 base_url 带不带 `/v1` 两种写法。
/// `versioned_path` 形如 `v1/messages`。
pub(crate) fn url_join(base: &str, versioned_path: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{}/{}", b, versioned_path.trim_start_matches("v1/"))
    } else {
        format!("{}/{}", b, versioned_path)
    }
}

/// ToolUse 的 input 序列化成参数字符串(Chat/Responses 的工具参数是字符串)。
/// input 若本来就是 String(上次解析失败留下的原文),原样送回。
pub(crate) fn args_to_string(input: &Value) -> String {
    match input {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 流式拼出来的参数文本 → JSON。空文本按 `{}` 处理(无参工具);
/// 解析失败时保留原文为 `Value::String`,让工具层报错、模型自愈。
pub(crate) fn parse_args(raw: &str) -> Value {
    let t = raw.trim();
    if t.is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(t).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// ToolUse 的 input 编码成对象(Anthropic 的 input 必须是 object)。
pub(crate) fn args_to_object(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        other => serde_json::json!({ "_raw": args_to_string(other) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_join_handles_v1() {
        assert_eq!(
            url_join("https://api.anthropic.com", "v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            url_join("https://api.openai.com/v1", "v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            url_join("http://localhost:11434/v1/", "v1/chat/completions"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn parse_args_variants() {
        assert_eq!(parse_args(""), serde_json::json!({}));
        assert_eq!(parse_args("{\"a\":1}"), serde_json::json!({"a":1}));
        assert!(matches!(parse_args("{broken"), Value::String(_)));
    }

    #[test]
    fn retry_after_is_only_trusted_on_rate_limit_statuses() {
        assert_eq!(
            retry_after_hint(429, Some("2500"), None),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(
            retry_after_hint(503, None, Some("7")),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            retry_after_hint(408, None, Some("3")),
            Some(Duration::from_secs(3))
        );
        // 网关错误页(502 等)上的 Retry-After 是 CDN 模板噪声,不采信,
        // 让上层走自家指数退避而不是白等一分钟。
        assert_eq!(retry_after_hint(502, Some("60000"), Some("60")), None);
        assert_eq!(retry_after_hint(500, None, Some("60")), None);
    }

    #[test]
    fn html_error_pages_reduce_to_title_not_markup() {
        let cloudflare = "<!DOCTYPE html>\n<html lang=\"en-US\"><head>\n  <title>weppp.cyou | 502: Bad gateway</title>\n</head><body>very long markup body</body></html>";
        assert_eq!(
            extract_api_error(cloudflare),
            "weppp.cyou | 502: Bad gateway(HTML 错误页,正文已省略)"
        );
        let untitled = "<html><body>oops</body></html>";
        let rendered = extract_api_error(untitled);
        assert!(rendered.contains("HTML 错误页"), "{rendered}");
        assert!(!rendered.contains("<body>"), "不得泄漏标签: {rendered}");
        // JSON 与纯文本路径不受影响;多行文本折叠成单行。
        assert_eq!(
            extract_api_error(r#"{"error":{"message":"quota exceeded"}}"#),
            "quota exceeded"
        );
        assert_eq!(
            extract_api_error("line one\n   line two"),
            "line one line two"
        );
    }
}

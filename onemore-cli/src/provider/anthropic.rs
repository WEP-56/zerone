//! Anthropic Messages API 适配器(https://docs.anthropic.com/en/api/messages)。
//!
//! 统一模型与它几乎一一对应(内部模型就是照它设计的),要点只有三个:
//! - `max_tokens` 是**必填**字段(另两家可省略),没配就用默认值;
//! - 消息必须 user/assistant 严格交替且 content 非空,
//!   所以编码时合并相邻同角色消息、跳过空消息;
//! - 流式的工具参数以 `input_json_delta.partial_json` 碎片下发,拼完才能解析。
//!
//! 流事件序列(与代码里的 match 一一对应):
//! ```text
//! message_start                          输入 token 用量在这
//!   content_block_start (index=0 text)
//!   content_block_delta (text_delta)     × N
//!   content_block_stop
//!   content_block_start (index=1 tool_use, 带 id/name)
//!   content_block_delta (input_json_delta) × N
//!   content_block_stop
//! message_delta                          stop_reason + 输出 token 用量
//! message_stop
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use super::{
    args_to_object, http_agent, parse_args, post_sse, sse::SseReader, FailedTurn, Provider,
    ProviderError, ProviderEvent, StreamTerminal, TurnOutput,
};
use crate::config::ProviderSettings;
use crate::context::PromptContext;
use crate::message::{Block, CacheUsage, ChatMessage, Role, StopReason, Usage};
use crate::tools::ToolSpec;

const DEFAULT_MAX_TOKENS: u64 = 8192;
const API_VERSION: &str = "2023-06-01";

fn cache_usage(usage: &Value) -> Option<CacheUsage> {
    let read = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    let write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    (read.is_some() || write.is_some()).then(|| CacheUsage {
        read_tokens: read.unwrap_or(0),
        write_tokens: write.unwrap_or(0),
    })
}

pub struct AnthropicProvider {
    settings: ProviderSettings,
    agent: ureq::Agent,
}

impl AnthropicProvider {
    pub fn new(settings: ProviderSettings) -> Self {
        AnthropicProvider {
            settings,
            agent: http_agent(),
        }
    }

    /// 统一模型 → Messages API 请求体。
    fn build_body(&self, prompt: &PromptContext, tools: &[ToolSpec]) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        for m in &prompt.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let mut content: Vec<Value> = Vec::new();
            for b in &m.blocks {
                match b {
                    Block::Text(t) => {
                        if !t.is_empty() {
                            content.push(json!({"type": "text", "text": t}));
                        }
                    }
                    // 未启用 extended thinking,历史里的思考块不回传
                    // (Anthropic 要求回传的 thinking 必须带原始签名,启用时再扩展)
                    Block::Thinking { .. } => {}
                    Block::ToolUse { id, name, input } => {
                        content.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": args_to_object(input),
                        }));
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content: c,
                        is_error,
                    } => {
                        let mut o = json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": c,
                        });
                        if *is_error {
                            o["is_error"] = json!(true);
                        }
                        content.push(o);
                    }
                }
            }
            if content.is_empty() {
                continue;
            }
            // 相邻同角色合并(API 要求严格交替)
            match messages.last_mut() {
                Some(last) if last["role"] == role => {
                    if let Some(arr) = last["content"].as_array_mut() {
                        arr.extend(content);
                    }
                }
                _ => messages.push(json!({"role": role, "content": content})),
            }
        }

        let mut body = json!({
            "model": self.settings.model,
            "max_tokens": self.settings.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "messages": messages,
            "stream": true,
        });
        let system = prompt.system_text();
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                super::sorted_tools(tools)
                    .into_iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.schema,
                        })
                    })
                    .collect(),
            );
        }
        body
    }
}

/// 流式过程中"半成品"内容块(按 index 归位)。
enum Partial {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        args: String,
    },
}

impl Provider for AnthropicProvider {
    fn label(&self) -> String {
        format!("{} / {}", self.settings.name, self.settings.model)
    }

    fn model(&self) -> &str {
        &self.settings.model
    }

    fn set_model(&mut self, model: String) {
        self.settings.model = model;
    }

    fn stream_turn(
        &self,
        prompt: &PromptContext,
        tools: &[ToolSpec],
        on_event: &mut dyn FnMut(ProviderEvent),
        cancel: &AtomicBool,
    ) -> StreamTerminal {
        match self.stream_turn_impl(prompt, tools, on_event, cancel) {
            Ok(Some(output)) => StreamTerminal::Done(output),
            Ok(None) => StreamTerminal::Aborted(FailedTurn::aborted()),
            Err(error) => StreamTerminal::Error(FailedTurn::from_error(error)),
        }
    }
}

impl AnthropicProvider {
    fn stream_turn_impl(
        &self,
        prompt: &PromptContext,
        tools: &[ToolSpec],
        on_event: &mut dyn FnMut(ProviderEvent),
        cancel: &AtomicBool,
    ) -> Result<Option<TurnOutput>, ProviderError> {
        let url = super::url_join(&self.settings.base_url, "v1/messages");
        let mut headers = vec![("x-api-key", self.settings.api_key.clone())];
        if self
            .settings
            .profile
            .capabilities()
            .canonical_version_header
        {
            headers.push(("anthropic-version", API_VERSION.to_string()));
        }
        let body = self.build_body(prompt, tools);
        let prompt_fingerprint =
            super::prompt_fingerprint(self.settings.profile, &self.settings.model, prompt, tools);
        let reader = post_sse(&self.agent, &url, &headers, &body)?;
        let mut sse = SseReader::new(reader);

        let mut partials: BTreeMap<usize, Partial> = BTreeMap::new();
        let mut blocks: Vec<Block> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::EndTurn;
        let mut saw_terminal = false;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let Some(ev) = sse
                .next_event()
                .map_err(|e| ProviderError::fatal(format!("读取流失败: {}", e)))?
            else {
                break;
            };
            if ev.data == "[DONE]" {
                saw_terminal = true;
                break;
            }
            let data: Value = serde_json::from_str(&ev.data)
                .map_err(|e| ProviderError::fatal(format!("流事件 JSON 无效: {}", e)))?;
            // 事件类型以 data.type 为准(event: 行与它一致)
            match data["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &data["message"]["usage"];
                    usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                    usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                    usage.cache = cache_usage(u);
                    // Anthropic reports uncached, cache-read, and cache-write input
                    // buckets separately. Normalize input_tokens to the full prompt.
                    if let Some(cache) = usage.cache {
                        usage.input_tokens = usage
                            .input_tokens
                            .saturating_add(cache.read_tokens)
                            .saturating_add(cache.write_tokens);
                    }
                }
                "content_block_start" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let cb = &data["content_block"];
                    match cb["type"].as_str().unwrap_or("") {
                        "text" => {
                            partials.insert(index, Partial::Text(String::new()));
                        }
                        "thinking" => {
                            partials.insert(index, Partial::Thinking(String::new()));
                        }
                        "tool_use" => {
                            let name = cb["name"].as_str().unwrap_or("").to_string();
                            on_event(ProviderEvent::ToolCallBegun { name: name.clone() });
                            partials.insert(
                                index,
                                Partial::ToolUse {
                                    id: cb["id"].as_str().unwrap_or("").to_string(),
                                    name,
                                    args: String::new(),
                                },
                            );
                        }
                        _ => {} // 未来的新块类型:忽略即向前兼容
                    }
                }
                "content_block_delta" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &data["delta"];
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let piece = delta["text"].as_str().unwrap_or("");
                            if let Some(Partial::Text(buf)) = partials.get_mut(&index) {
                                buf.push_str(piece);
                            }
                            on_event(ProviderEvent::TextDelta(piece.to_string()));
                        }
                        "thinking_delta" => {
                            let piece = delta["thinking"].as_str().unwrap_or("");
                            if let Some(Partial::Thinking(buf)) = partials.get_mut(&index) {
                                buf.push_str(piece);
                            }
                            on_event(ProviderEvent::ThinkingDelta(piece.to_string()));
                        }
                        "input_json_delta" => {
                            if let Some(Partial::ToolUse { args, .. }) = partials.get_mut(&index) {
                                args.push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(p) = partials.remove(&index) {
                        blocks.push(match p {
                            Partial::Text(t) => Block::Text(t),
                            Partial::Thinking(t) => Block::Thinking {
                                text: t,
                                provider_kind: Some("anthropic".to_string()),
                                raw: None,
                            },
                            Partial::ToolUse { id, name, args } => Block::ToolUse {
                                id,
                                name,
                                input: parse_args(&args),
                            },
                        });
                    }
                }
                "message_delta" => {
                    if let Some(r) = data["delta"]["stop_reason"].as_str() {
                        stop = match r {
                            "end_turn" | "stop_sequence" => StopReason::EndTurn,
                            "tool_use" => StopReason::ToolUse,
                            "max_tokens" => StopReason::MaxTokens,
                            other => StopReason::Other(other.to_string()),
                        };
                    }
                    // 这里的 output_tokens 是累计值,直接覆盖
                    if let Some(n) = data["usage"]["output_tokens"].as_u64() {
                        usage.output_tokens = n;
                    }
                }
                "message_stop" => {
                    saw_terminal = true;
                    break;
                }
                "error" => {
                    return Err(ProviderError::fatal(format!(
                        "API 流错误: {}",
                        data["error"]["message"].as_str().unwrap_or("未知")
                    )));
                }
                _ => {} // ping 等
            }
        }

        if !saw_terminal {
            return Err(ProviderError::fatal("流在终止事件前结束"));
        }

        // 防御:极端情况下(流被掐断)可能有没 stop 的半成品,按序收编
        for (_, p) in partials {
            blocks.push(match p {
                Partial::Text(t) => Block::Text(t),
                Partial::Thinking(t) => Block::Thinking {
                    text: t,
                    provider_kind: Some("anthropic".to_string()),
                    raw: None,
                },
                Partial::ToolUse { id, name, args } => Block::ToolUse {
                    id,
                    name,
                    input: parse_args(&args),
                },
            });
        }

        Ok(Some(TurnOutput {
            message: ChatMessage {
                role: Role::Assistant,
                blocks,
            },
            usage,
            stop,
            prompt_fingerprint: Some(prompt_fingerprint),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKind, ProviderProfile};

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new(ProviderSettings {
            name: "anthropic".into(),
            api: ApiKind::Messages,
            profile: ProviderProfile::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            api_key: "test".into(),
            model: "claude-sonnet-5".into(),
            max_tokens: None,
            context_window: None,
        })
    }

    /// 请求体形状回归测试:改坏编码逻辑时在本地就能发现,不用打真 API。
    #[test]
    fn request_body_shape() {
        let mut prompt = PromptContext::default();
        prompt.system_sections.push("sys".into());
        prompt.messages.push(ChatMessage::user_text("你好"));
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text("我看看".into()),
                Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":"a.txt"}),
                },
            ],
        });
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "内容".into(),
                is_error: false,
            }],
        });

        let tools = vec![ToolSpec {
            name: "read_file".into(),
            description: "读".into(),
            schema: serde_json::json!({"type":"object","properties":{}}),
            capabilities: crate::tools::ToolCapabilities::READ_ONLY,
            permission: crate::tools::ToolPermissionSpec::default(),
        }];
        let body = provider().build_body(&prompt, &tools);

        assert_eq!(body["system"], "sys");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    /// 相邻同角色消息必须被合并(工具结果消息 + 用户新输入)。
    #[test]
    fn merges_consecutive_user_messages() {
        let mut prompt = PromptContext::default();
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: "r".into(),
                is_error: false,
            }],
        });
        prompt.messages.push(ChatMessage::user_text("继续"));
        let body = provider().build_body(&prompt, &[]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
    }
}

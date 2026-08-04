//! OpenAI Responses API 适配器(/v1/responses,OpenAI 当前主推的接口)。
//!
//! 与 Chat Completions 的核心差异:
//! - 不是"消息列表"而是"条目(item)列表":消息、函数调用、函数结果、
//!   推理(reasoning)都是并列的 item 类型;
//! - 我们以无状态方式使用它(`store: false`,每轮全量带上历史),
//!   这与另两个适配器的心智模型一致。代价是:**推理模型的 reasoning item
//!   必须原样回传**(否则下一轮带 function_call 的请求会被 400 拒绝),
//!   所以请求 `include: ["reasoning.encrypted_content"]`,并把整个 item
//!   存进 `Block::Thinking.raw`,编码时原样吐回;
//! - 流式事件类型非常多(几十种),本适配器只消费必要子集,
//!   未知事件一律忽略(向前兼容)。
//!
//! 消费的事件(带 → 的会产生统一流事件):
//! ```text
//! response.output_item.added                → ToolCallBegun(function_call 时)
//! response.output_text.delta                → TextDelta
//! response.reasoning_summary_text.delta     → ThinkingDelta
//! response.function_call_arguments.delta    (拼参数)
//! response.output_item.done                 (以完整 item 定稿一个 Block)
//! response.completed / incomplete           (用量、停止原因,收尾)
//! response.failed / error                   (报错)
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use super::{
    args_to_string, http_agent, parse_args, post_sse, sse::SseReader, FailedTurn, Provider,
    ProviderError, ProviderEvent, StreamTerminal, TurnOutput,
};
use crate::config::ProviderSettings;
use crate::context::PromptContext;
use crate::message::{Block, ChatMessage, Role, StopReason, Usage};
use crate::tools::ToolSpec;

/// 标记 Thinking.raw 属于本适配器(切换 provider 后不会误回传)。
const KIND: &str = "openai_responses";

pub struct ResponsesProvider {
    settings: ProviderSettings,
    agent: ureq::Agent,
}

impl ResponsesProvider {
    pub fn new(settings: ProviderSettings) -> Self {
        ResponsesProvider {
            settings,
            agent: http_agent(),
        }
    }

    fn build_body(&self, prompt: &PromptContext, tools: &[ToolSpec]) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for m in &prompt.messages {
            for b in &m.blocks {
                match (m.role, b) {
                    (Role::User, Block::Text(t)) if !t.is_empty() => {
                        input.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": t}],
                        }));
                    }
                    (
                        Role::User,
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        },
                    ) => {
                        let text = if *is_error {
                            format!("ERROR: {}", content)
                        } else {
                            content.clone()
                        };
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": text,
                        }));
                    }
                    (Role::Assistant, Block::Text(t)) if !t.is_empty() => {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": t}],
                        }));
                    }
                    (
                        Role::Assistant,
                        Block::ToolUse {
                            id,
                            name,
                            input: args,
                        },
                    ) => {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args_to_string(args),
                        }));
                    }
                    // 只回传本适配器自己存下的 reasoning item(原样)
                    (
                        Role::Assistant,
                        Block::Thinking {
                            provider_kind,
                            raw: Some(item),
                            ..
                        },
                    ) if provider_kind.as_deref() == Some(KIND) => {
                        input.push(item.clone());
                    }
                    _ => {}
                }
            }
        }

        let mut body = json!({
            "model": self.settings.model,
            "input": input,
            "stream": true,
            // 无状态使用:OpenAI 不保存本次响应,历史完全由我们管理
            "store": false,
            // 拿到加密的 reasoning 内容,才能在下一轮原样回传
            "include": ["reasoning.encrypted_content"],
        });
        let system = prompt.system_text();
        if !system.is_empty() {
            body["instructions"] = json!(system);
        }
        if !tools.is_empty() {
            // 注意:Responses 的工具声明是平铺的,没有 function 包一层
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.schema,
                        })
                    })
                    .collect(),
            );
        }
        if let Some(n) = self.settings.max_tokens {
            body["max_output_tokens"] = json!(n);
        }
        body
    }
}

/// 流式过程中按 output_index 归位的半成品(仅作流意外中断时的兜底;
/// 正常路径以 output_item.done 里的完整 item 为准)。
enum Partial {
    Msg(String),
    Fc {
        call_id: String,
        name: String,
        args: String,
    },
    Reasoning(String),
}

/// 完整 item → 统一 Block。
fn block_from_item(item: &Value) -> Option<Block> {
    match item["type"].as_str().unwrap_or("") {
        "message" => {
            let mut text = String::new();
            if let Some(parts) = item["content"].as_array() {
                for p in parts {
                    if p["type"] == "output_text" {
                        text.push_str(p["text"].as_str().unwrap_or(""));
                    }
                }
            }
            if text.is_empty() {
                None
            } else {
                Some(Block::Text(text))
            }
        }
        "function_call" => Some(Block::ToolUse {
            id: item["call_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or("")
                .to_string(),
            name: item["name"].as_str().unwrap_or("").to_string(),
            input: parse_args(item["arguments"].as_str().unwrap_or("")),
        }),
        "reasoning" => {
            let mut summary = String::new();
            if let Some(parts) = item["summary"].as_array() {
                for p in parts {
                    summary.push_str(p["text"].as_str().unwrap_or(""));
                }
            }
            Some(Block::Thinking {
                text: summary,
                provider_kind: Some(KIND.to_string()),
                raw: Some(item.clone()),
            })
        }
        _ => None,
    }
}

impl Provider for ResponsesProvider {
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

impl ResponsesProvider {
    fn stream_turn_impl(
        &self,
        prompt: &PromptContext,
        tools: &[ToolSpec],
        on_event: &mut dyn FnMut(ProviderEvent),
        cancel: &AtomicBool,
    ) -> Result<Option<TurnOutput>, ProviderError> {
        let url = super::url_join(&self.settings.base_url, "v1/responses");
        let mut headers: Vec<(&str, String)> = Vec::new();
        if !self.settings.api_key.is_empty() {
            headers.push(("authorization", format!("Bearer {}", self.settings.api_key)));
        }
        let body = self.build_body(prompt, tools);
        let reader = post_sse(&self.agent, &url, &headers, &body)?;
        let mut sse = SseReader::new(reader);

        let mut partials: BTreeMap<u64, Partial> = BTreeMap::new();
        let mut blocks: Vec<Block> = Vec::new();
        let mut usage = Usage::default();
        let mut stop: Option<StopReason> = None;
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
            let index = data["output_index"].as_u64().unwrap_or(0);

            match data["type"].as_str().unwrap_or("") {
                "response.output_item.added" => {
                    let item = &data["item"];
                    match item["type"].as_str().unwrap_or("") {
                        "message" => {
                            partials.insert(index, Partial::Msg(String::new()));
                        }
                        "reasoning" => {
                            partials.insert(index, Partial::Reasoning(String::new()));
                        }
                        "function_call" => {
                            let name = item["name"].as_str().unwrap_or("").to_string();
                            on_event(ProviderEvent::ToolCallBegun { name: name.clone() });
                            partials.insert(
                                index,
                                Partial::Fc {
                                    call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                                    name,
                                    args: String::new(),
                                },
                            );
                        }
                        _ => {}
                    }
                }
                "response.output_text.delta" => {
                    let piece = data["delta"].as_str().unwrap_or("");
                    if let Some(Partial::Msg(buf)) = partials.get_mut(&index) {
                        buf.push_str(piece);
                    }
                    on_event(ProviderEvent::TextDelta(piece.to_string()));
                }
                "response.reasoning_summary_text.delta" => {
                    let piece = data["delta"].as_str().unwrap_or("");
                    if let Some(Partial::Reasoning(buf)) = partials.get_mut(&index) {
                        buf.push_str(piece);
                    }
                    on_event(ProviderEvent::ThinkingDelta(piece.to_string()));
                }
                "response.function_call_arguments.delta" => {
                    if let Some(Partial::Fc { args, .. }) = partials.get_mut(&index) {
                        args.push_str(data["delta"].as_str().unwrap_or(""));
                    }
                }
                "response.output_item.done" => {
                    partials.remove(&index);
                    if let Some(b) = block_from_item(&data["item"]) {
                        blocks.push(b);
                    }
                }
                "response.completed" | "response.incomplete" => {
                    saw_terminal = true;
                    let resp = &data["response"];
                    if let Some(u) = resp.get("usage").filter(|u| !u.is_null()) {
                        usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                        usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                    }
                    if data["type"] == "response.incomplete" {
                        let reason = resp["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("incomplete");
                        stop = Some(if reason == "max_output_tokens" {
                            StopReason::MaxTokens
                        } else {
                            StopReason::Other(reason.to_string())
                        });
                    }
                    break;
                }
                "response.failed" => {
                    return Err(ProviderError::fatal(format!(
                        "API 错误: {}",
                        data["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("response.failed(无详情)")
                    )));
                }
                "error" => {
                    return Err(ProviderError::fatal(format!(
                        "API 流错误: {}",
                        data["message"].as_str().unwrap_or("未知")
                    )));
                }
                _ => {} // created / in_progress / content_part.* 等一律忽略
            }
        }

        if !saw_terminal {
            return Err(ProviderError::fatal("流在 response terminal 事件前结束"));
        }

        // 流被掐断时的兜底:把半成品按序收编
        for (_, p) in partials {
            match p {
                Partial::Msg(t) if !t.is_empty() => blocks.push(Block::Text(t)),
                Partial::Fc {
                    call_id,
                    name,
                    args,
                } => blocks.push(Block::ToolUse {
                    id: call_id,
                    name,
                    input: parse_args(&args),
                }),
                Partial::Reasoning(t) if !t.is_empty() => blocks.push(Block::Thinking {
                    text: t,
                    provider_kind: None, // 没拿到完整 item,不能回传
                    raw: None,
                }),
                _ => {}
            }
        }

        let has_calls = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let stop = stop.unwrap_or(if has_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });

        Ok(Some(TurnOutput {
            message: ChatMessage {
                role: Role::Assistant,
                blocks,
            },
            usage,
            stop,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKind;

    fn provider() -> ResponsesProvider {
        ResponsesProvider::new(ProviderSettings {
            name: "openai".into(),
            api: ApiKind::Responses,
            base_url: "https://api.openai.com/v1".into(),
            api_key: "test".into(),
            model: "gpt-5".into(),
            max_tokens: None,
            context_window: None,
        })
    }

    #[test]
    fn request_body_shape() {
        let mut prompt = PromptContext::default();
        prompt.system_sections.push("sys".into());
        prompt.messages.push(ChatMessage::user_text("hi"));
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Thinking {
                    text: "…".into(),
                    provider_kind: Some(KIND.to_string()),
                    raw: Some(json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"xxx"})),
                },
                Block::ToolUse {
                    id: "fc_call_1".into(),
                    name: "read_file".into(),
                    input: json!({"path":"a.txt"}),
                },
            ],
        });
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "fc_call_1".into(),
                content: "内容".into(),
                is_error: false,
            }],
        });

        let tools = vec![ToolSpec {
            name: "read_file".into(),
            description: "读".into(),
            schema: json!({"type":"object","properties":{}}),
            capabilities: crate::tools::ToolCapabilities::READ_ONLY,
            permission: crate::tools::ToolPermissionSpec::default(),
        }];
        let body = provider().build_body(&prompt, &tools);

        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        // reasoning item 原样回传,且必须排在它的 function_call 之前
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "xxx");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "fc_call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        // 工具声明是平铺的(没有 function 包一层)
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn foreign_thinking_is_dropped() {
        let mut prompt = PromptContext::default();
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::Thinking {
                text: "deepseek 的思考".into(),
                provider_kind: None,
                raw: None,
            }],
        });
        let body = provider().build_body(&prompt, &[]);
        assert!(body["input"].as_array().unwrap().is_empty());
    }
}

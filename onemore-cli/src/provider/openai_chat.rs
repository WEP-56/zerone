//! OpenAI Chat Completions API 适配器。
//!
//! 这是兼容面最广的接口:DeepSeek、Kimi、Qwen、ollama、vLLM……
//! 几乎所有模型服务都实现了它。也因此要多防几手"方言":
//! - 工具调用参数按 `tool_calls[].index` 分片流式下发,id/name 只在首片;
//!   个别服务不发 index(按"新 id = 新调用"兜底);
//! - 用量要显式请求(`stream_options.include_usage`),
//!   且用量块的 `choices` 是空数组——不能无脑取 `choices[0]`;
//! - DeepSeek R1 的思考走非标字段 `reasoning_content`,这里顺手支持;
//! - `max_tokens` 字段:OpenAI 新模型已改名 `max_completion_tokens`,
//!   多数兼容服务仍认旧名。本适配器在配置了 max_tokens 时发旧名,
//!   不配则不发(见 docs/04-provider.md 的讨论)。
//!
//! 与统一模型的映射:
//! - `Block::ToolResult` → 独立的 `role:"tool"` 消息(必须紧跟发起调用的
//!   assistant 消息,所以编码 user 消息时先输出工具结果,再输出用户文本);
//! - 工具结果没有 is_error 字段,错误以 `ERROR:` 前缀文本表达;
//! - `Block::Thinking` 不回传(DeepSeek 明确要求不要把 reasoning 传回去)。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use super::{
    args_to_string, http_agent, parse_args, post_sse, sse::SseReader, Provider, ProviderError,
    ProviderEvent, TurnOutput,
};
use crate::config::ProviderSettings;
use crate::context::PromptContext;
use crate::message::{Block, ChatMessage, Role, StopReason, Usage};
use crate::tools::ToolSpec;

pub struct ChatProvider {
    settings: ProviderSettings,
    agent: ureq::Agent,
}

impl ChatProvider {
    pub fn new(settings: ProviderSettings) -> Self {
        ChatProvider {
            settings,
            agent: http_agent(),
        }
    }

    fn build_body(&self, prompt: &PromptContext, tools: &[ToolSpec]) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        // 只为已编码的有效 ToolUse 发送结果。旧版本可能把稀疏 index 补成
        // name="" 的占位调用；在这里过滤，可让已被污染的会话自行恢复。
        let mut valid_tool_call_ids: HashSet<String> = HashSet::new();
        let system = prompt.system_text();
        if !system.is_empty() {
            messages.push(json!({"role": "system", "content": system}));
        }

        for m in &prompt.messages {
            match m.role {
                Role::User => {
                    // 先工具结果(role:"tool" 必须紧跟 assistant 的 tool_calls)
                    for b in &m.blocks {
                        if let Block::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = b
                        {
                            if !valid_tool_call_ids.remove(tool_use_id) {
                                continue;
                            }
                            let text = if *is_error {
                                format!("ERROR: {}", content)
                            } else {
                                content.clone()
                            };
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": text,
                            }));
                        }
                    }
                    // 再普通文本
                    let text = m.text();
                    if !text.is_empty() {
                        messages.push(json!({"role": "user", "content": text}));
                    }
                }
                Role::Assistant => {
                    let text = m.text();
                    let calls: Vec<Value> = m
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            Block::ToolUse { id, name, input }
                                if !id.trim().is_empty() && !name.trim().is_empty() =>
                            {
                                valid_tool_call_ids.insert(id.clone());
                                Some(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": args_to_string(input),
                                    },
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    if text.is_empty() && calls.is_empty() {
                        continue;
                    }
                    let mut msg = json!({"role": "assistant"});
                    // 纯工具调用时 content 置 null(标准写法)
                    msg["content"] = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    };
                    if !calls.is_empty() {
                        msg["tool_calls"] = Value::Array(calls);
                    }
                    messages.push(msg);
                }
            }
        }

        let mut body = json!({
            "model": self.settings.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.schema,
                            },
                        })
                    })
                    .collect(),
            );
        }
        if let Some(n) = self.settings.max_tokens {
            body["max_tokens"] = json!(n);
        }
        body
    }
}

/// 流式拼装中的一次工具调用。
#[derive(Default)]
struct PendingCall {
    /// 流中的逻辑位置。兼容服务不一定从 0 开始，也不一定连续。
    stream_index: Option<u64>,
    id: String,
    name: String,
    args: String,
    announced: bool,
}

/// 合并一个 `tool_calls[]` 增量，返回首次得到的工具名供 UI 提示。
///
/// 不能用 index 直接扩容 Vec：有些兼容服务从 1 开始编号，扩容会制造一个
/// 空的 index=0 占位调用，随后污染历史并让下一次请求永久 400。
fn merge_tool_call_delta(calls: &mut Vec<PendingCall>, tc: &Value) -> Option<String> {
    let index = tc["index"]
        .as_u64()
        .or_else(|| tc["index"].as_str().and_then(|s| s.parse().ok()));
    let incoming_id = tc["id"].as_str().filter(|id| !id.is_empty());

    let position = index
        .and_then(|wanted| {
            calls
                .iter()
                .position(|call| call.stream_index == Some(wanted))
        })
        .or_else(|| incoming_id.and_then(|wanted| calls.iter().position(|call| call.id == wanted)))
        .or_else(|| {
            if index.is_none() && incoming_id.is_none() {
                calls.len().checked_sub(1)
            } else {
                None
            }
        });

    let position = match position {
        Some(position) => position,
        None => {
            calls.push(PendingCall {
                stream_index: index,
                ..PendingCall::default()
            });
            calls.len() - 1
        }
    };
    let call = &mut calls[position];
    if call.stream_index.is_none() {
        call.stream_index = index;
    }
    if let Some(id) = incoming_id {
        call.id = id.to_string();
    }

    // 标准形状是 function.{name,arguments}；少数兼容服务会把两者平铺。
    let function = tc
        .get("function")
        .filter(|value| value.is_object())
        .unwrap_or(tc);
    if let Some(name) = function["name"].as_str().filter(|name| !name.is_empty()) {
        merge_name_fragment(&mut call.name, name);
    }
    if let Some(arguments) = function.get("arguments") {
        if let Some(fragment) = arguments.as_str() {
            call.args.push_str(fragment);
        } else if !arguments.is_null() {
            call.args.push_str(&arguments.to_string());
        }
    }
    if !call.announced && !call.name.is_empty() {
        call.announced = true;
        Some(call.name.clone())
    } else {
        None
    }
}

fn merge_name_fragment(current: &mut String, fragment: &str) {
    if current.is_empty() || fragment.starts_with(current.as_str()) {
        *current = fragment.to_string();
    } else if fragment != current && !current.ends_with(fragment) {
        current.push_str(fragment);
    }
}

fn finish_tool_calls(calls: Vec<PendingCall>) -> Result<Vec<Block>, ProviderError> {
    let mut blocks = Vec::with_capacity(calls.len());
    for (position, call) in calls.into_iter().enumerate() {
        if call.name.trim().is_empty() {
            let index = call
                .stream_index
                .map(|value| value.to_string())
                .unwrap_or_else(|| "未知".into());
            return Err(ProviderError::fatal(format!(
                "Chat Completions 返回了缺少 function.name 的工具调用(index={});\
                 已丢弃本次 assistant 消息，会话仍可继续",
                index
            )));
        }
        blocks.push(Block::ToolUse {
            // 个别服务不发 id:必须造一个,否则结果消息无法配对
            id: if call.id.is_empty() {
                format!("call_{}", position)
            } else {
                call.id
            },
            name: call.name,
            input: parse_args(&call.args),
        });
    }
    Ok(blocks)
}

impl Provider for ChatProvider {
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
    ) -> Result<Option<TurnOutput>, ProviderError> {
        let url = super::url_join(&self.settings.base_url, "v1/chat/completions");
        let mut headers: Vec<(&str, String)> = Vec::new();
        // 本地服务(ollama 等)不需要鉴权:key 留空则不发 Authorization 头
        if !self.settings.api_key.is_empty() {
            headers.push(("authorization", format!("Bearer {}", self.settings.api_key)));
        }
        let body = self.build_body(prompt, tools);
        let reader = post_sse(&self.agent, &url, &headers, &body)?;
        let mut sse = SseReader::new(reader);

        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: Vec<PendingCall> = Vec::new();
        let mut finish: Option<String> = None;
        let mut usage = Usage::default();

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
                break;
            }
            let data: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // 流中途的错误对象(部分兼容服务这样报错)
            if let Some(msg) = data["error"]["message"].as_str() {
                return Err(ProviderError::fatal(format!("API 错误: {}", msg)));
            }

            // 用量块:choices 为空数组,只带 usage
            if let Some(u) = data.get("usage").filter(|u| !u.is_null()) {
                usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(usage.input_tokens);
                usage.output_tokens = u["completion_tokens"]
                    .as_u64()
                    .unwrap_or(usage.output_tokens);
            }

            let Some(choice) = data["choices"].get(0) else {
                continue;
            };
            if let Some(r) = choice["finish_reason"].as_str() {
                finish = Some(r.to_string());
            }
            let delta = &choice["delta"];

            if let Some(piece) = delta["content"].as_str() {
                if !piece.is_empty() {
                    text.push_str(piece);
                    on_event(ProviderEvent::TextDelta(piece.to_string()));
                }
            }
            // DeepSeek R1 等的思考流(非标字段)
            if let Some(piece) = delta["reasoning_content"].as_str() {
                if !piece.is_empty() {
                    thinking.push_str(piece);
                    on_event(ProviderEvent::ThinkingDelta(piece.to_string()));
                }
            }

            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    if let Some(name) = merge_tool_call_delta(&mut calls, tc) {
                        on_event(ProviderEvent::ToolCallBegun { name });
                    }
                }
            }
        }

        // 组装统一消息
        let mut blocks: Vec<Block> = Vec::new();
        if !thinking.is_empty() {
            blocks.push(Block::Thinking {
                text: thinking,
                provider_kind: None, // 不回传
                raw: None,
            });
        }
        if !text.is_empty() {
            blocks.push(Block::Text(text));
        }
        let call_blocks = finish_tool_calls(calls)?;
        let has_calls = !call_blocks.is_empty();
        blocks.extend(call_blocks);

        let stop = if has_calls {
            StopReason::ToolUse
        } else {
            match finish.as_deref() {
                Some("stop") | None => StopReason::EndTurn,
                Some("length") => StopReason::MaxTokens,
                Some(other) => StopReason::Other(other.to_string()),
            }
        };

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

    fn provider() -> ChatProvider {
        ChatProvider::new(ProviderSettings {
            name: "openai-chat".into(),
            api: ApiKind::Chat,
            base_url: "https://api.openai.com/v1".into(),
            api_key: "test".into(),
            model: "gpt-5".into(),
            max_tokens: None,
        })
    }

    #[test]
    fn request_body_shape() {
        let mut prompt = PromptContext::default();
        prompt.system_sections.push("sys".into());
        prompt.messages.push(ChatMessage::user_text("hi"));
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: "call_1".into(),
                name: "list_dir".into(),
                input: json!({"path":"."}),
            }],
        });
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![
                Block::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "src/".into(),
                    is_error: false,
                },
                Block::Text("继续".into()),
            ],
        });

        let body = provider().build_body(&prompt, &[]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert!(msgs[2]["content"].is_null()); // 纯工具调用 content=null
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "list_dir");
        // 字符串化的 arguments
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\".\"}"
        );
        // tool 结果必须排在用户文本前
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "继续");
        // 显式请求用量
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn tool_error_gets_prefix() {
        let mut prompt = PromptContext::default();
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({"path": "missing.txt"}),
            }],
        });
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "找不到文件".into(),
                is_error: true,
            }],
        });
        let body = provider().build_body(&prompt, &[]);
        assert!(body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .starts_with("ERROR:"));
    }

    #[test]
    fn one_based_sparse_index_does_not_create_empty_call() {
        let mut calls = Vec::new();
        let announced = merge_tool_call_delta(
            &mut calls,
            &json!({
                "index": 1,
                "id": "call_1",
                "function": {"name": "read_file", "arguments": "{\"path\":"}
            }),
        );
        assert_eq!(announced.as_deref(), Some("read_file"));
        merge_tool_call_delta(
            &mut calls,
            &json!({"index": 1, "function": {"arguments": "\"README.md\"}"}}),
        );

        assert_eq!(calls.len(), 1, "稀疏 index 不应生成占位调用");
        let blocks = finish_tool_calls(calls).unwrap();
        match &blocks[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(input, &json!({"path": "README.md"}));
            }
            other => panic!("应得到 ToolUse，实际为 {:?}", other),
        }
    }

    #[test]
    fn missing_tool_name_is_rejected_before_reaching_runtime() {
        let mut calls = Vec::new();
        merge_tool_call_delta(
            &mut calls,
            &json!({"index": 0, "id": "bad", "function": {"arguments": "{}"}}),
        );
        let error = finish_tool_calls(calls).unwrap_err();
        assert!(error.message.contains("缺少 function.name"));
    }

    #[test]
    fn invalid_legacy_tool_pair_is_omitted_from_request_history() {
        let mut prompt = PromptContext::default();
        prompt.messages.push(ChatMessage::user_text("读取文件"));
        prompt.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: "call_bad".into(),
                name: "".into(),
                input: json!({"path": "README.md"}),
            }],
        });
        prompt.messages.push(ChatMessage {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: "call_bad".into(),
                content: "未知工具".into(),
                is_error: true,
            }],
        });
        prompt.messages.push(ChatMessage::user_text("继续"));

        let body = provider().build_body(&prompt, &[]);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "读取文件");
        assert_eq!(messages[1]["content"], "继续");
        assert!(messages.iter().all(|message| message["role"] != "tool"));
    }
}

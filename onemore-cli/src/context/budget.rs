//! # 上下文预算:估算 → 确定性修剪 → 明确拒绝
//!
//! 每次发请求前,Runtime 用它回答"这轮上下文放不放得下":
//!
//! 1. **估算**优先使用真实 usage 基线:最近一条带 usage 的 assistant 的
//!    input+output 覆盖了"那次请求看到的一切",其后的尾部消息按 ~4 字符/token
//!    估算(与 Pi `estimate.ts` 同思路)。没有基线时对全量做字符估算。
//! 2. 超出预算时先做**确定性修剪**:把旧 User 消息里的 ToolResult 正文折叠成
//!    短摘要——只改"本轮模型视图"里的字符串,事实日志与工具配对结构不动,
//!    因此不会出现半批 ToolUse/ToolResult。
//! 3. 修剪改变了视图,真实 usage 基线随之失效,改用纯字符估算复核;
//!    **仍然超预算就拒绝发请求**,由调用方提示用户 /compact 或 /clear。
//!    预算层从不静默删除消息。

use crate::message::{Block, ChatMessage, Role};
use crate::session::{message_chars, ModelProjection};

/// 折叠旧 ToolResult 时保留的正文头部字符数。
const SHORTENED_RESULT_HEAD_CHARS: usize = 160;
/// 视图末尾受保护的消息条数(最近一批工具结果与其 assistant 不被折叠)。
const PROTECTED_TAIL_MESSAGES: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    /// 模型上下文窗口(token)。None = 不启用预算强制。
    pub context_window: Option<u64>,
    /// 为输出预留的 token(通常取 max_tokens)。
    pub reserve_output: u64,
}

impl ContextBudget {
    pub fn disabled() -> Self {
        ContextBudget {
            context_window: None,
            reserve_output: 0,
        }
    }

    /// 可用于输入侧的 token 预算。
    fn available_input(&self) -> Option<u64> {
        self.context_window
            .map(|window| window.saturating_sub(self.reserve_output).max(1))
    }
}

#[derive(Debug)]
pub enum BudgetDecision {
    /// 可以发送。`messages` 可能是折叠过旧 ToolResult 的视图。
    Send {
        messages: Vec<ChatMessage>,
        estimated_tokens: u64,
        /// 非空表示做过修剪,内容适合作为 Notice 提示用户。
        notices: Vec<String>,
    },
    /// 修剪后仍超预算,拒绝发请求(等待 /compact 或 /clear)。
    Refuse {
        estimated_tokens: u64,
        available_tokens: u64,
    },
}

fn chars_to_tokens(chars: u64) -> u64 {
    chars / 4 + 1
}

/// 基线可用时:基线 + 尾部估算;否则全量字符估算(含 system 与工具声明)。
pub fn estimate_tokens(system_chars: u64, tools_chars: u64, projection: &ModelProjection) -> u64 {
    match projection.known_token_baseline {
        Some(baseline) => baseline + chars_to_tokens(projection.tail_chars_after_baseline),
        None => {
            let message_total: u64 = projection.messages.iter().map(message_chars).sum();
            chars_to_tokens(system_chars + tools_chars + message_total)
        }
    }
}

pub fn apply_budget(
    budget: &ContextBudget,
    system_chars: u64,
    tools_chars: u64,
    projection: ModelProjection,
) -> BudgetDecision {
    let estimated = estimate_tokens(system_chars, tools_chars, &projection);
    let Some(available) = budget.available_input() else {
        return BudgetDecision::Send {
            messages: projection.messages,
            estimated_tokens: estimated,
            notices: Vec::new(),
        };
    };
    if estimated <= available {
        return BudgetDecision::Send {
            messages: projection.messages,
            estimated_tokens: estimated,
            notices: Vec::new(),
        };
    }

    let (messages, shortened) = shorten_old_tool_results(projection.messages);
    if shortened == 0 {
        return BudgetDecision::Refuse {
            estimated_tokens: estimated,
            available_tokens: available,
        };
    }
    // 视图已被改写,真实 usage 基线不再对应它;退回纯字符估算复核。
    let message_total: u64 = messages.iter().map(message_chars).sum();
    let reestimated = chars_to_tokens(system_chars + tools_chars + message_total);
    if reestimated <= available {
        BudgetDecision::Send {
            messages,
            estimated_tokens: reestimated,
            notices: vec![format!(
                "上下文接近预算,已在本轮视图中折叠 {} 个旧工具结果(事实日志未修改)",
                shortened
            )],
        }
    } else {
        BudgetDecision::Refuse {
            estimated_tokens: reestimated,
            available_tokens: available,
        }
    }
}

/// 折叠受保护尾部之前所有 User 消息里的长 ToolResult 正文。
/// 只缩短字符串,不移除块:ToolUse/ToolResult 配对与消息结构保持原样。
fn shorten_old_tool_results(mut messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, usize) {
    let protected_from = messages.len().saturating_sub(PROTECTED_TAIL_MESSAGES);
    let mut shortened = 0usize;
    for message in messages.iter_mut().take(protected_from) {
        if message.role != Role::User {
            continue;
        }
        for block in &mut message.blocks {
            let Block::ToolResult { content, .. } = block else {
                continue;
            };
            let total = content.chars().count();
            if total <= SHORTENED_RESULT_HEAD_CHARS {
                continue;
            }
            let head: String = content.chars().take(SHORTENED_RESULT_HEAD_CHARS).collect();
            *content = format!(
                "{}\n[已按上下文预算折叠,原文 {} 字符;完整内容保留在会话事实日志中]",
                head, total
            );
            shortened += 1;
        }
    }
    (messages, shortened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ChatMessage, Role, Usage};
    use crate::session::{project_model_messages, SessionEntry, SessionEntryPayload};

    fn entry(payload: SessionEntryPayload) -> SessionEntry {
        SessionEntry {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            created_at: 0,
            payload,
        }
    }

    fn assistant_with_usage(text: &str, input: u64, output: u64) -> SessionEntryPayload {
        SessionEntryPayload::message(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::Text(text.into())],
            },
            Some(Usage {
                input_tokens: input,
                output_tokens: output,
            }),
        )
    }

    fn tool_roundtrip(id: &str, result_chars: usize) -> Vec<SessionEntryPayload> {
        vec![
            SessionEntryPayload::message(
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolUse {
                        id: id.into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "big.txt"}),
                    }],
                },
                None,
            ),
            SessionEntryPayload::message(
                ChatMessage {
                    role: Role::User,
                    blocks: vec![Block::ToolResult {
                        tool_use_id: id.into(),
                        content: "x".repeat(result_chars),
                        is_error: false,
                    }],
                },
                None,
            ),
        ]
    }

    #[test]
    fn estimate_uses_real_usage_baseline_plus_tail() {
        let entries = vec![
            entry(SessionEntryPayload::message(
                ChatMessage::user_text("很长的第一问".repeat(100)),
                None,
            )),
            entry(assistant_with_usage("答", 1000, 50)),
            entry(SessionEntryPayload::message(
                ChatMessage::user_text("x".repeat(400)),
                None,
            )),
        ];
        let projection = project_model_messages(&entries);
        assert_eq!(projection.known_token_baseline, Some(1050));
        // 尾部只有基线之后那条 400 字符消息(+块开销 16)。
        assert_eq!(projection.tail_chars_after_baseline, 416);
        let estimated = estimate_tokens(10_000, 10_000, &projection);
        // 基线路径不该把 system/tools 再算一遍(它们已含在真实 usage 中)。
        assert_eq!(estimated, 1050 + 416 / 4 + 1);
    }

    #[test]
    fn without_baseline_estimate_counts_everything() {
        let entries = vec![entry(SessionEntryPayload::message(
            ChatMessage::user_text("x".repeat(400)),
            None,
        ))];
        let projection = project_model_messages(&entries);
        assert_eq!(projection.known_token_baseline, None);
        let estimated = estimate_tokens(400, 200, &projection);
        assert_eq!(estimated, (400 + 200 + 416) / 4 + 1);
    }

    #[test]
    fn within_budget_sends_untouched_view() {
        let entries = vec![entry(SessionEntryPayload::message(
            ChatMessage::user_text("hi"),
            None,
        ))];
        let projection = project_model_messages(&entries);
        let budget = ContextBudget {
            context_window: Some(10_000),
            reserve_output: 1_000,
        };
        match apply_budget(&budget, 0, 0, projection) {
            BudgetDecision::Send {
                messages, notices, ..
            } => {
                assert_eq!(messages.len(), 1);
                assert!(notices.is_empty());
            }
            other => panic!("应直接发送: {:?}", other),
        }
    }

    #[test]
    fn over_budget_shortens_old_tool_results_but_keeps_pairing() {
        let mut payloads = vec![SessionEntryPayload::message(
            ChatMessage::user_text("查日志"),
            None,
        )];
        payloads.extend(tool_roundtrip("old-call", 40_000));
        payloads.push(assistant_with_usage("看完了", 30_000, 100));
        payloads.extend(tool_roundtrip("new-call", 500));
        let entries: Vec<SessionEntry> = payloads.into_iter().map(entry).collect();
        let projection = project_model_messages(&entries);

        // 基线 30100 超出 16k 预算;折叠旧结果后纯字符估算应低于预算。
        let budget = ContextBudget {
            context_window: Some(16_000),
            reserve_output: 1_000,
        };
        match apply_budget(&budget, 100, 100, projection) {
            BudgetDecision::Send {
                messages, notices, ..
            } => {
                assert_eq!(notices.len(), 1);
                // 旧 ToolResult 被折叠且仍与 ToolUse 配对。
                let old_result = messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .find_map(|b| match b {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } if tool_use_id == "old-call" => Some(content.clone()),
                        _ => None,
                    })
                    .expect("配对未被破坏");
                assert!(old_result.contains("已按上下文预算折叠"));
                // 最近一批工具结果在保护区内,不折叠。
                let new_result = messages
                    .iter()
                    .flat_map(|m| m.blocks.iter())
                    .find_map(|b| match b {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } if tool_use_id == "new-call" => Some(content.clone()),
                        _ => None,
                    })
                    .unwrap();
                assert!(!new_result.contains("折叠"));
            }
            other => panic!("折叠后应可发送: {:?}", other),
        }
    }

    #[test]
    fn refuses_to_send_when_shortening_is_not_enough() {
        // 超预算的正文在普通消息里,折叠 ToolResult 无济于事 → 明确拒绝。
        let entries = vec![entry(SessionEntryPayload::message(
            ChatMessage::user_text("x".repeat(100_000)),
            None,
        ))];
        let projection = project_model_messages(&entries);
        let budget = ContextBudget {
            context_window: Some(2_000),
            reserve_output: 1_000,
        };
        match apply_budget(&budget, 0, 0, projection) {
            BudgetDecision::Refuse {
                estimated_tokens,
                available_tokens,
            } => {
                assert!(estimated_tokens > available_tokens);
                assert_eq!(available_tokens, 1_000);
            }
            other => panic!("应拒绝发送: {:?}", other),
        }
    }
}

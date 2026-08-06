//! Session 领域模型：事实日志、应用事实和模型消息之间的单向投影。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::message::{Block, ChatMessage, Role, Usage};
use crate::plan::{reduce_plan, PlanSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub payload: SessionEntryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEntryPayload {
    Message(MessageRecord),
    Notice(NoticeRecord),
    Artifact(ArtifactRef),
    Compaction(CompactionRecord),
    ModelChange(ModelChangeRecord),
    PlanUpdated(PlanSnapshot),
    PlanReminder(PlanReminderRecord),
}

impl SessionEntryPayload {
    pub fn message(message: ChatMessage, usage: Option<Usage>) -> Self {
        SessionEntryPayload::Message(MessageRecord {
            message,
            usage,
            prompt_fingerprint: None,
        })
    }

    pub fn message_with_prompt(
        message: ChatMessage,
        usage: Usage,
        prompt_fingerprint: Option<String>,
    ) -> Self {
        SessionEntryPayload::Message(MessageRecord {
            message,
            usage: Some(usage),
            prompt_fingerprint,
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            SessionEntryPayload::Message(_) => "message",
            SessionEntryPayload::Notice(_) => "notice",
            SessionEntryPayload::Artifact(_) => "artifact",
            SessionEntryPayload::Compaction(_) => "compaction",
            SessionEntryPayload::ModelChange(_) => "model_change",
            SessionEntryPayload::PlanUpdated(_) => "plan_updated",
            SessionEntryPayload::PlanReminder(_) => "plan_reminder",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub message: ChatMessage,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub prompt_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoticeRecord {
    pub text: String,
    pub level: NoticeLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub media_type: String,
    pub byte_len: usize,
}

#[derive(Debug, Clone)]
pub struct ArtifactDraft {
    pub reference: ArtifactRef,
    pub bytes: Vec<u8>,
}

impl ArtifactDraft {
    pub fn text(text: String) -> Self {
        let bytes = text.into_bytes();
        ArtifactDraft {
            reference: ArtifactRef {
                id: uuid::Uuid::new_v4().to_string(),
                media_type: "text/plain; charset=utf-8".into(),
                byte_len: bytes.len(),
            },
            bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    pub tokens_before: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelChangeRecord {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_reasoning_effort")]
    pub effort: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReminderRecord {
    pub revision: u64,
    pub reason: PlanReminderReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReminderReason {
    Continue,
    Cancelled,
}

impl PlanReminderRecord {
    fn model_message(self) -> ChatMessage {
        let text = match self.reason {
            PlanReminderReason::Continue => format!(
                "[Plan reminder] Plan revision {} still has active items. Continue the work. If the work is actually finished, call update_plan with the complete current snapshot before giving the final response.",
                self.revision
            ),
            PlanReminderReason::Cancelled => format!(
                "[Plan state after cancellation] The in-progress item was returned to pending. The current plan revision is {}.",
                self.revision
            ),
        };
        ChatMessage::user_text(text)
    }
}

fn default_reasoning_effort() -> String {
    crate::config::DEFAULT_REASONING_EFFORT.to_string()
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub entries: Vec<SessionEntry>,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct ModelProjection {
    pub messages: Vec<ChatMessage>,
    pub diagnostics: Vec<String>,
    /// 最近一条带真实 usage 的 assistant 的 input+output 总量。
    /// 它覆盖"当时那次请求看到的一切"(system + tools + 之前的消息),
    /// 是 token 估算的可信前缀基线;只统计最近一次 compaction 之后的消息,
    /// 因为压缩会使更早的 usage 与当前视图脱节。
    pub known_token_baseline: Option<u64>,
    /// 基线 assistant 之后新增消息的字符量(无基线时为全部消息字符量)。
    /// 估算 = 基线 + 尾部字符/4;详见 `context::budget`。
    pub tail_chars_after_baseline: u64,
}

/// 一条消息进入请求体的近似字符成本(正文 + 工具参数/结果 + 少量结构开销)。
pub fn message_chars(message: &ChatMessage) -> u64 {
    let mut chars = 0u64;
    for block in &message.blocks {
        chars += 16; // 块结构本身的编码开销
        match block {
            Block::Text(text) => chars += text.chars().count() as u64,
            Block::Thinking { text, .. } => chars += text.chars().count() as u64,
            Block::ToolUse { name, input, .. } => {
                chars += name.chars().count() as u64;
                chars += input.to_string().chars().count() as u64;
            }
            Block::ToolResult { content, .. } => chars += content.chars().count() as u64,
        }
    }
    chars
}

pub fn application_messages(entries: &[SessionEntry]) -> Vec<ChatMessage> {
    entries
        .iter()
        .filter_map(|entry| match &entry.payload {
            SessionEntryPayload::Message(record) => Some(record.message.clone()),
            _ => None,
        })
        .collect()
}

pub fn project_model_messages(entries: &[SessionEntry]) -> ModelProjection {
    let plan = reduce_plan(entries);
    let last_compaction = entries
        .iter()
        .rposition(|entry| matches!(entry.payload, SessionEntryPayload::Compaction(_)));
    let mut messages = Vec::new();
    let mut known_token_baseline = None;
    let mut tail_chars_after_baseline = 0u64;
    let start = match last_compaction {
        Some(index) => {
            if let SessionEntryPayload::Compaction(compaction) = &entries[index].payload {
                let summary = ChatMessage::user_text(format!(
                    "[Earlier conversation summary]\n{}",
                    compaction.summary
                ));
                tail_chars_after_baseline += message_chars(&summary);
                messages.push(summary);
            }
            index + 1
        }
        None => 0,
    };
    for entry in &entries[start..] {
        match &entry.payload {
            SessionEntryPayload::Message(record) => {
                messages.push(record.message.clone());
                let usage_baseline = if record.message.role == Role::Assistant {
                    record.usage
                } else {
                    None
                };
                match usage_baseline {
                    Some(usage) => {
                        known_token_baseline =
                            Some(usage.input_tokens.saturating_add(usage.output_tokens));
                        tail_chars_after_baseline = 0;
                    }
                    None => tail_chars_after_baseline += message_chars(&record.message),
                }
            }
            SessionEntryPayload::PlanReminder(reminder) => {
                let message = reminder.model_message();
                tail_chars_after_baseline += message_chars(&message);
                messages.push(message);
            }
            _ => {}
        }
    }
    let (messages, mut diagnostics) = repair_tool_pairs(messages);
    diagnostics.extend(plan.diagnostics);
    ModelProjection {
        messages,
        diagnostics,
        known_token_baseline,
        tail_chars_after_baseline,
    }
}

/// 新写入必须在 Runtime/事务边界保持配对；这里仅用于旧库或损坏数据的防御性修复。
fn repair_tool_pairs(messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, Vec<String>) {
    let mut projected = Vec::new();
    let mut diagnostics = Vec::new();
    let mut pending: HashMap<String, String> = HashMap::new();

    for mut message in messages {
        if !pending.is_empty() {
            if message.role == Role::User {
                let mut seen = HashSet::new();
                message.blocks.retain(|block| match block {
                    Block::ToolResult { tool_use_id, .. } if pending.contains_key(tool_use_id) => {
                        if seen.insert(tool_use_id.clone()) {
                            true
                        } else {
                            diagnostics.push(format!("移除重复 ToolResult {}", tool_use_id));
                            false
                        }
                    }
                    Block::ToolResult { tool_use_id, .. } => {
                        diagnostics.push(format!("移除孤立 ToolResult {}", tool_use_id));
                        false
                    }
                    _ => true,
                });
                for (id, name) in &pending {
                    if !seen.contains(id) {
                        diagnostics.push(format!("为工具 {} 合成缺失的 ToolResult {}", name, id));
                        message.blocks.push(missing_tool_result(id));
                    }
                }
                pending.clear();
            } else {
                projected.push(synthetic_results(&pending, &mut diagnostics));
                pending.clear();
            }
        } else if message.role == Role::User {
            message.blocks.retain(|block| {
                if let Block::ToolResult { tool_use_id, .. } = block {
                    diagnostics.push(format!("移除孤立 ToolResult {}", tool_use_id));
                    false
                } else {
                    true
                }
            });
        }

        if message.role == Role::Assistant {
            for (id, name, _) in message.tool_uses() {
                if pending.insert(id.to_string(), name.to_string()).is_some() {
                    diagnostics.push(format!("assistant 内 ToolUse ID {} 重复", id));
                }
            }
        }
        projected.push(message);
    }
    if !pending.is_empty() {
        projected.push(synthetic_results(&pending, &mut diagnostics));
    }
    (projected, diagnostics)
}

fn synthetic_results(
    pending: &HashMap<String, String>,
    diagnostics: &mut Vec<String>,
) -> ChatMessage {
    let mut ids: Vec<_> = pending.keys().cloned().collect();
    ids.sort();
    for id in &ids {
        diagnostics.push(format!("为末尾 ToolUse 合成缺失的 ToolResult {}", id));
    }
    ChatMessage {
        role: Role::User,
        blocks: ids.iter().map(|id| missing_tool_result(id)).collect(),
    }
}

fn missing_tool_result(id: &str) -> Block {
    Block::ToolResult {
        tool_use_id: id.into(),
        content: "[会话恢复修复：工具结果缺失，原调用未重新执行]".into(),
        is_error: true,
    }
}

pub fn validate_new_message_batch(payloads: &[SessionEntryPayload]) -> Result<(), String> {
    let mut uses = HashMap::new();
    let mut results = HashMap::new();
    for payload in payloads {
        let SessionEntryPayload::Message(record) = payload else {
            continue;
        };
        for block in &record.message.blocks {
            match block {
                Block::ToolUse { id, .. } => {
                    *uses.entry(id.as_str()).or_insert(0usize) += 1;
                }
                Block::ToolResult { tool_use_id, .. } => {
                    *results.entry(tool_use_id.as_str()).or_insert(0usize) += 1;
                }
                _ => {}
            }
        }
    }
    for (id, count) in &uses {
        if *count != 1 || results.get(id).copied() != Some(1) {
            return Err(format!(
                "事务中的 ToolUse {} 必须恰好配对一个 ToolResult",
                id
            ));
        }
    }
    for id in results.keys() {
        if !uses.contains_key(id) {
            return Err(format!("事务含有孤立 ToolResult {}", id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanItem, PlanStatus};

    fn entry(payload: SessionEntryPayload) -> SessionEntry {
        SessionEntry {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            created_at: 0,
            payload,
        }
    }

    #[test]
    fn ui_only_entries_are_not_projected_to_model() {
        let entries = vec![
            entry(SessionEntryPayload::Notice(NoticeRecord {
                text: "visible in UI only".into(),
                level: NoticeLevel::Info,
            })),
            entry(SessionEntryPayload::message(
                ChatMessage::user_text("hello"),
                None,
            )),
        ];
        let projection = project_model_messages(&entries);
        assert_eq!(projection.messages.len(), 1);
        assert_eq!(projection.messages[0].text(), "hello");
    }

    #[test]
    fn plan_state_is_not_chat_but_bounded_reminders_are_projected() {
        let entries = vec![
            entry(SessionEntryPayload::PlanUpdated(PlanSnapshot {
                revision: 1,
                items: vec![PlanItem {
                    id: "work".into(),
                    text: "Do the work".into(),
                    status: PlanStatus::Pending,
                }],
                explanation: None,
            })),
            entry(SessionEntryPayload::PlanReminder(PlanReminderRecord {
                revision: 1,
                reason: PlanReminderReason::Continue,
            })),
        ];
        assert!(application_messages(&entries).is_empty());
        let projection = project_model_messages(&entries);
        assert_eq!(projection.messages.len(), 1);
        assert!(projection.messages[0].text().contains("Plan revision 1"));
    }

    #[test]
    fn compaction_changes_model_view_without_deleting_facts() {
        let entries = vec![
            entry(SessionEntryPayload::message(
                ChatMessage::user_text("old fact"),
                None,
            )),
            entry(SessionEntryPayload::Compaction(CompactionRecord {
                summary: "old task summary".into(),
                tokens_before: 100,
            })),
            entry(SessionEntryPayload::message(
                ChatMessage::user_text("new fact"),
                None,
            )),
        ];
        let projection = project_model_messages(&entries);
        assert_eq!(entries.len(), 3);
        assert_eq!(projection.messages.len(), 2);
        assert!(projection.messages[0].text().contains("old task summary"));
        assert_eq!(projection.messages[1].text(), "new fact");
    }

    #[test]
    fn projector_repairs_missing_tool_result_without_reexecution() {
        let entries = vec![entry(SessionEntryPayload::message(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "call-1".into(),
                    name: "dynamic".into(),
                    input: serde_json::json!({}),
                }],
            },
            None,
        ))];
        let projection = project_model_messages(&entries);
        assert_eq!(projection.messages.len(), 2);
        assert!(matches!(
            projection.messages[1].blocks[0],
            Block::ToolResult { is_error: true, .. }
        ));
        assert_eq!(projection.diagnostics.len(), 1);
    }

    #[test]
    fn new_batches_reject_half_tool_pairs() {
        let assistant = SessionEntryPayload::message(
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "call-1".into(),
                    name: "dynamic".into(),
                    input: serde_json::json!({}),
                }],
            },
            None,
        );
        assert!(validate_new_message_batch(&[assistant]).is_err());
    }
}

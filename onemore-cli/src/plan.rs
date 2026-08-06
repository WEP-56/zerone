//! Structured plan state and its append-only reducer.

use serde::{Deserialize, Serialize};

use crate::session::{SessionEntry, SessionEntryPayload};

pub const MAX_PLAN_ITEMS: usize = 32;
pub const MAX_PLAN_ID_CHARS: usize = 64;
pub const MAX_PLAN_TEXT_CHARS: usize = 512;
pub const MAX_PLAN_EXPLANATION_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    pub id: String,
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSnapshot {
    pub revision: u64,
    pub items: Vec<PlanItem>,
    pub explanation: Option<String>,
}

impl PlanSnapshot {
    pub fn has_active_items(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.status != PlanStatus::Completed)
    }

    pub fn counts(&self) -> PlanCounts {
        let mut counts = PlanCounts::default();
        for item in &self.items {
            match item.status {
                PlanStatus::Pending => counts.pending += 1,
                PlanStatus::InProgress => counts.in_progress += 1,
                PlanStatus::Completed => counts.completed += 1,
            }
        }
        counts
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReduction {
    pub snapshot: PlanSnapshot,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanErrorKind {
    Invalid,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub kind: PlanErrorKind,
    pub message: String,
}

impl PlanError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: PlanErrorKind::Invalid,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: PlanErrorKind::Conflict,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub fn update_plan(
    current: &PlanSnapshot,
    expected_revision: u64,
    items: Vec<PlanItem>,
    explanation: Option<String>,
) -> Result<PlanSnapshot, PlanError> {
    if expected_revision != current.revision {
        return Err(PlanError::conflict(format!(
            "plan revision conflict: expected {}, current {}",
            expected_revision, current.revision
        )));
    }
    let revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| PlanError::conflict("plan revision overflow"))?;
    let snapshot = normalize_snapshot(PlanSnapshot {
        revision,
        items,
        explanation,
    })?;
    validate_transition(current, &snapshot)?;
    Ok(snapshot)
}

pub fn validate_transition(current: &PlanSnapshot, next: &PlanSnapshot) -> Result<(), PlanError> {
    let expected = current
        .revision
        .checked_add(1)
        .ok_or_else(|| PlanError::conflict("plan revision overflow"))?;
    if next.revision != expected {
        return Err(PlanError::conflict(format!(
            "plan revision must advance from {} to {}, got {}",
            current.revision, expected, next.revision
        )));
    }
    validate_snapshot(next)
}

pub fn reduce_plan(entries: &[SessionEntry]) -> PlanReduction {
    reduce_plan_payloads(entries.iter().map(|entry| &entry.payload))
}

pub fn reduce_plan_payloads<'a>(
    payloads: impl IntoIterator<Item = &'a SessionEntryPayload>,
) -> PlanReduction {
    let mut snapshot = PlanSnapshot::default();
    let mut diagnostics = Vec::new();
    for payload in payloads {
        let SessionEntryPayload::PlanUpdated(next) = payload else {
            continue;
        };
        match validate_transition(&snapshot, next) {
            Ok(()) => snapshot = next.clone(),
            Err(error) => diagnostics.push(format!(
                "ignored invalid PlanUpdated revision {}: {}",
                next.revision, error
            )),
        }
    }
    PlanReduction {
        snapshot,
        diagnostics,
    }
}

pub fn validate_plan_append(
    existing: &[SessionEntry],
    appended: &[SessionEntryPayload],
) -> Result<(), PlanError> {
    let mut current = reduce_plan(existing).snapshot;
    for payload in appended {
        let SessionEntryPayload::PlanUpdated(next) = payload else {
            continue;
        };
        validate_transition(&current, next)?;
        current = next.clone();
    }
    Ok(())
}

pub fn return_in_progress_to_pending(current: &PlanSnapshot) -> Option<PlanSnapshot> {
    if !current
        .items
        .iter()
        .any(|item| item.status == PlanStatus::InProgress)
    {
        return None;
    }
    let items = current
        .items
        .iter()
        .cloned()
        .map(|mut item| {
            if item.status == PlanStatus::InProgress {
                item.status = PlanStatus::Pending;
            }
            item
        })
        .collect();
    update_plan(
        current,
        current.revision,
        items,
        Some("当前轮已取消；进行中条目已退回待处理。".into()),
    )
    .ok()
}

pub fn compaction_summary(snapshot: &PlanSnapshot) -> Option<String> {
    if snapshot.revision == 0 || !snapshot.has_active_items() {
        return None;
    }
    const ITEM_PREVIEW_CHARS: usize = 160;
    let counts = snapshot.counts();
    let mut lines = vec![format!(
        "[Active plan revision {}; {} completed]",
        snapshot.revision, counts.completed
    )];
    for item in snapshot
        .items
        .iter()
        .filter(|item| item.status != PlanStatus::Completed)
    {
        let status = item.status.as_str();
        lines.push(format!(
            "- [{}] {}: {}",
            status,
            item.id,
            crate::util::ellipsis(&item.text, ITEM_PREVIEW_CHARS)
        ));
    }
    Some(lines.join("\n"))
}

fn normalize_snapshot(mut snapshot: PlanSnapshot) -> Result<PlanSnapshot, PlanError> {
    for item in &mut snapshot.items {
        item.id = item.id.trim().to_string();
        item.text = item.text.trim().to_string();
    }
    snapshot.explanation = snapshot
        .explanation
        .map(|explanation| explanation.trim().to_string())
        .filter(|explanation| !explanation.is_empty());
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn validate_snapshot(snapshot: &PlanSnapshot) -> Result<(), PlanError> {
    if snapshot.items.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::invalid(format!(
            "plan has {} items; maximum is {}",
            snapshot.items.len(),
            MAX_PLAN_ITEMS
        )));
    }
    let mut ids = std::collections::HashSet::new();
    let mut in_progress = 0usize;
    for (index, item) in snapshot.items.iter().enumerate() {
        if item.id.is_empty() {
            return Err(PlanError::invalid(format!(
                "plan item {} has an empty id",
                index
            )));
        }
        if item.id.trim() != item.id {
            return Err(PlanError::invalid(format!(
                "plan item {:?} id is not trimmed",
                item.id
            )));
        }
        if item.id.chars().count() > MAX_PLAN_ID_CHARS {
            return Err(PlanError::invalid(format!(
                "plan item id {:?} exceeds {} characters",
                item.id, MAX_PLAN_ID_CHARS
            )));
        }
        if !ids.insert(item.id.as_str()) {
            return Err(PlanError::invalid(format!(
                "duplicate plan item id {:?}",
                item.id
            )));
        }
        if item.text.is_empty() {
            return Err(PlanError::invalid(format!(
                "plan item {:?} has empty text",
                item.id
            )));
        }
        if item.text.trim() != item.text {
            return Err(PlanError::invalid(format!(
                "plan item {:?} text is not trimmed",
                item.id
            )));
        }
        if item.text.chars().count() > MAX_PLAN_TEXT_CHARS {
            return Err(PlanError::invalid(format!(
                "plan item {:?} text exceeds {} characters",
                item.id, MAX_PLAN_TEXT_CHARS
            )));
        }
        if item.status == PlanStatus::InProgress {
            in_progress += 1;
        }
    }
    if in_progress > 1 {
        return Err(PlanError::invalid(
            "plan may contain at most one in_progress item",
        ));
    }
    if let Some(explanation) = &snapshot.explanation {
        if explanation.trim() != explanation {
            return Err(PlanError::invalid("plan explanation is not trimmed"));
        }
        if explanation.chars().count() > MAX_PLAN_EXPLANATION_CHARS {
            return Err(PlanError::invalid(format!(
                "plan explanation exceeds {} characters",
                MAX_PLAN_EXPLANATION_CHARS
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, status: PlanStatus) -> PlanItem {
        PlanItem {
            id: id.into(),
            text: format!("work on {id}"),
            status,
        }
    }

    fn entry(snapshot: PlanSnapshot) -> SessionEntry {
        SessionEntry {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            created_at: 0,
            payload: SessionEntryPayload::PlanUpdated(snapshot),
        }
    }

    #[test]
    fn update_normalizes_text_and_preserves_order() {
        let snapshot = update_plan(
            &PlanSnapshot::default(),
            0,
            vec![
                PlanItem {
                    id: " first ".into(),
                    text: " First step ".into(),
                    status: PlanStatus::InProgress,
                },
                item("second", PlanStatus::Pending),
            ],
            Some(" Start now ".into()),
        )
        .unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.items[0].id, "first");
        assert_eq!(snapshot.items[0].text, "First step");
        assert_eq!(snapshot.items[1].id, "second");
        assert_eq!(snapshot.explanation.as_deref(), Some("Start now"));
    }

    #[test]
    fn invalid_snapshots_are_rejected() {
        let current = PlanSnapshot::default();
        let cases = [
            vec![
                item("same", PlanStatus::Pending),
                item("same", PlanStatus::Completed),
            ],
            vec![PlanItem {
                id: "empty".into(),
                text: "   ".into(),
                status: PlanStatus::Pending,
            }],
            vec![
                item("one", PlanStatus::InProgress),
                item("two", PlanStatus::InProgress),
            ],
        ];
        for items in cases {
            assert!(update_plan(&current, 0, items, None).is_err());
        }
        assert!(update_plan(
            &current,
            0,
            (0..=MAX_PLAN_ITEMS)
                .map(|index| item(&index.to_string(), PlanStatus::Pending))
                .collect(),
            None,
        )
        .is_err());
        assert!(update_plan(
            &current,
            0,
            vec![PlanItem {
                id: "i".repeat(MAX_PLAN_ID_CHARS + 1),
                text: "work".into(),
                status: PlanStatus::Pending,
            }],
            None,
        )
        .is_err());
        assert!(update_plan(
            &current,
            0,
            vec![PlanItem {
                id: "long".into(),
                text: "t".repeat(MAX_PLAN_TEXT_CHARS + 1),
                status: PlanStatus::Pending,
            }],
            None,
        )
        .is_err());
        assert!(update_plan(
            &current,
            0,
            Vec::new(),
            Some("e".repeat(MAX_PLAN_EXPLANATION_CHARS + 1)),
        )
        .is_err());
    }

    #[test]
    fn stale_expected_revision_is_a_conflict() {
        let current = PlanSnapshot {
            revision: 4,
            ..PlanSnapshot::default()
        };
        let error = update_plan(&current, 3, Vec::new(), None).unwrap_err();
        assert_eq!(error.kind, PlanErrorKind::Conflict);
        assert!(error.message.contains("current 4"));
    }

    #[test]
    fn reducer_keeps_last_legal_snapshot_after_gap() {
        let first = PlanSnapshot {
            revision: 1,
            items: vec![item("one", PlanStatus::Pending)],
            explanation: None,
        };
        let gap = PlanSnapshot {
            revision: 3,
            items: vec![item("three", PlanStatus::Pending)],
            explanation: None,
        };
        let reduction = reduce_plan(&[entry(first.clone()), entry(gap)]);
        assert_eq!(reduction.snapshot, first);
        assert_eq!(reduction.diagnostics.len(), 1);
    }

    #[test]
    fn cancellation_only_returns_in_progress_to_pending() {
        let current = PlanSnapshot {
            revision: 7,
            items: vec![
                item("done", PlanStatus::Completed),
                item("active", PlanStatus::InProgress),
                item("later", PlanStatus::Pending),
            ],
            explanation: None,
        };
        let repaired = return_in_progress_to_pending(&current).unwrap();
        assert_eq!(repaired.revision, 8);
        assert_eq!(repaired.items[0].status, PlanStatus::Completed);
        assert_eq!(repaired.items[1].status, PlanStatus::Pending);
        assert_eq!(repaired.items[2].status, PlanStatus::Pending);
    }
}

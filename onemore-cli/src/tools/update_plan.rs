use serde::Deserialize;
use serde_json::Value;

use super::{
    Tool, ToolCapabilities, ToolContext, ToolEffect, ToolError, ToolErrorCode, ToolExecutionMode,
    ToolOutput, ToolPermissionSpec, ToolSpec,
};
use crate::plan::{self, PlanItem};

pub struct UpdatePlan;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanArgs {
    expected_revision: u64,
    #[serde(default)]
    explanation: Option<String>,
    plan: Vec<PlanItem>,
}

impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            description: "Replace the complete structured plan. Use for multi-step work; keep at most one item in_progress, preserve stable item ids, and pass the current revision as expected_revision. An empty plan clears it.".into(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "expected_revision": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "explanation": {
                        "type": "string",
                        "maxLength": plan::MAX_PLAN_EXPLANATION_CHARS
                    },
                    "plan": {
                        "type": "array",
                        "maxItems": plan::MAX_PLAN_ITEMS,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": plan::MAX_PLAN_ID_CHARS
                                },
                                "text": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": plan::MAX_PLAN_TEXT_CHARS
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["id", "text", "status"]
                        }
                    }
                },
                "required": ["expected_revision", "plan"]
            }),
            capabilities: ToolCapabilities {
                read_only: true,
                destructive: false,
                execution_mode: ToolExecutionMode::Sequential,
                supports_background: false,
            },
            permission: ToolPermissionSpec::default(),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let args: UpdatePlanArgs = serde_json::from_value(args.clone()).map_err(|error| {
            ToolError::invalid_arguments(format!("invalid update_plan arguments: {error}"))
        })?;
        let snapshot = plan::update_plan(
            ctx.current_plan(),
            args.expected_revision,
            args.plan,
            args.explanation,
        )
        .map_err(|error| {
            let code = match error.kind {
                plan::PlanErrorKind::Invalid => ToolErrorCode::InvalidArguments,
                plan::PlanErrorKind::Conflict => ToolErrorCode::Conflict,
            };
            ToolError::new(code, error.message)
        })?;
        let counts = snapshot.counts();
        let model_text = serde_json::json!({
            "revision": snapshot.revision,
            "pending": counts.pending,
            "in_progress": counts.in_progress,
            "completed": counts.completed,
        })
        .to_string();
        let ui_summary = Some(format!(
            "计划 #{}: {} 待处理，{} 进行中，{} 已完成",
            snapshot.revision, counts.pending, counts.in_progress, counts.completed
        ));
        ctx.record_effect(ToolEffect::PlanUpdated(snapshot));
        Ok(ToolOutput {
            model_text,
            ui_summary,
            details: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::plan::{PlanSnapshot, PlanStatus};
    use crate::workspace::Workspace;

    fn execute(
        args: Value,
        current_plan: PlanSnapshot,
    ) -> (Result<ToolOutput, ToolError>, Vec<ToolEffect>) {
        let workspace = Workspace::new(std::env::temp_dir());
        let cancel = AtomicBool::new(false);
        let mut progress = |_| {};
        let mut context = ToolContext {
            workspace: &workspace,
            cancel: &cancel,
            session_id: "test",
            current_plan,
            progress: &mut progress,
            effects: Vec::new(),
        };
        let result = UpdatePlan.execute(&args, &mut context);
        let effects = context.take_effects();
        (result, effects)
    }

    #[test]
    fn emits_an_explicit_plan_effect() {
        let (result, effects) = execute(
            serde_json::json!({
                "expected_revision": 0,
                "explanation": "Starting",
                "plan": [{"id": "inspect", "text": "Inspect code", "status": "in_progress"}]
            }),
            PlanSnapshot::default(),
        );
        assert!(result.is_ok());
        assert_eq!(effects.len(), 1);
        let ToolEffect::PlanUpdated(snapshot) = &effects[0];
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.items[0].status, PlanStatus::InProgress);
    }

    #[test]
    fn rejects_stale_revisions_without_an_effect() {
        let (result, effects) = execute(
            serde_json::json!({"expected_revision": 0, "plan": []}),
            PlanSnapshot {
                revision: 2,
                ..PlanSnapshot::default()
            },
        );
        assert_eq!(result.unwrap_err().code, ToolErrorCode::Conflict);
        assert!(effects.is_empty());
    }
}

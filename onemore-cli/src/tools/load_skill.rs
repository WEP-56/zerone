use std::sync::Arc;

use serde_json::{json, Value};

use crate::skills::{render_loaded_skill, SkillCatalog, SkillLoadError};

use super::{
    require_str, Tool, ToolCapabilities, ToolContext, ToolError, ToolOutput, ToolPermissionSpec,
    ToolSpec,
};

pub struct LoadSkill {
    catalog: Arc<SkillCatalog>,
}

impl LoadSkill {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        LoadSkill { catalog }
    }
}

impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".into(),
            description: "Load the full instructions for one skill from the startup catalog. "
                .to_string()
                + "Use this before following a named skill. If the user asks to install a skill "
                + "or provides a documentation/GitHub link, ask whether the target is global or "
                + "workspace-local and confirm their intent, then use the existing run_command "
                + "or write_file tools to install it; those tools enforce normal permissions. "
                + "The catalog refreshes after restarting Onemore.",
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "name": { "type": "string", "minLength": 1, "maxLength": 64 } },
                "required": ["name"]
            }),
            capabilities: ToolCapabilities::READ_ONLY,
            permission: ToolPermissionSpec::default(),
        }
    }

    fn execute(&self, args: &Value, _ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let name = require_str(args, "name")?;
        match self.catalog.load(name) {
            Ok(skill) => Ok(ToolOutput::text(render_loaded_skill(&skill))),
            Err(error) => Err(match error {
                SkillLoadError::NotFound(message) => {
                    ToolError::new(super::ToolErrorCode::NotFound, message)
                }
                SkillLoadError::Stale(message) => {
                    ToolError::new(super::ToolErrorCode::Conflict, message)
                }
                SkillLoadError::Io(message) => ToolError::new(super::ToolErrorCode::Io, message),
                SkillLoadError::Invalid(message) => {
                    ToolError::new(super::ToolErrorCode::InvalidArguments, message)
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanSnapshot;
    use crate::skills::discover;
    use crate::workspace::Workspace;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn loads_only_a_catalog_name_and_returns_bounded_marked_content() {
        let root =
            std::env::temp_dir().join(format!("onemore-load-skill-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("demo")).unwrap();
        fs::write(
            root.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\n<script>&",
        )
        .unwrap();
        let catalog = Arc::new(discover(&root, &root.join("missing")).catalog);
        let tool = LoadSkill::new(catalog);
        let workspace = Workspace::new(root.clone());
        let cancel = AtomicBool::new(false);
        let mut progress = |_output: ToolOutput| {};
        let mut ctx = ToolContext {
            workspace: &workspace,
            cancel: &cancel,
            session_id: "test",
            current_plan: PlanSnapshot::default(),
            progress: &mut progress,
            effects: Vec::new(),
        };
        let output = tool.execute(&json!({ "name": "demo" }), &mut ctx).unwrap();
        assert!(output.model_text.contains("&lt;script&gt;&amp;"));
        assert!(tool
            .execute(&json!({ "name": "../demo/SKILL.md" }), &mut ctx)
            .is_err());
        let _ = fs::remove_dir_all(root);
    }
}

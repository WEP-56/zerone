use std::sync::Arc;

use super::{ContextProvider, PromptContext};
use crate::skills::SkillCatalog;
use crate::workspace::Workspace;

pub struct SkillsContext {
    catalog: Arc<SkillCatalog>,
}

impl SkillsContext {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        SkillsContext { catalog }
    }
}

impl ContextProvider for SkillsContext {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn contribute(&self, prompt: &mut PromptContext, _ws: &Workspace) {
        let rendered = self.catalog.render_prompt();
        if !rendered.is_empty() {
            prompt.system_sections.push(rendered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextProvider;
    use crate::skills::discover;
    use std::fs;

    #[test]
    fn catalog_is_a_stable_system_section_without_skill_bodies() {
        let root =
            std::env::temp_dir().join(format!("onemore-skills-context-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("demo")).unwrap();
        fs::write(
            root.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nSECRET BODY",
        )
        .unwrap();
        let catalog = Arc::new(discover(&root, &root.join("missing")).catalog);
        let provider = SkillsContext::new(catalog);
        let mut prompt = PromptContext::default();
        provider.contribute(&mut prompt, &Workspace::new(root.clone()));
        let text = prompt.system_text();
        assert!(text.contains("name=\"demo\""));
        assert!(!text.contains("SECRET BODY"));
        let _ = fs::remove_dir_all(root);
    }
}

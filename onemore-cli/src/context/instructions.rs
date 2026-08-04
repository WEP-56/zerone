//! 基础行为准则:系统提示的第一段。
//! 可在 config.toml 用 `system_prompt` 整体替换;这里的默认值刻意精简——
//! 系统提示本身就是 harness 的运行时调参面板之一,可通过配置持续迭代。

use super::{ContextProvider, PromptContext};
use crate::workspace::Workspace;

pub struct Instructions {
    text: String,
}

/// 默认系统提示。用英文写(模型对英文指令的服从性最稳),
/// 但明确要求"跟随用户的语言回复"。
const DEFAULT: &str = "\
You are Onemore, a coding agent running in a terminal.

Guidelines:
- Use the provided tools to inspect and modify files and to run commands. \
Do not guess file contents: read before you edit.
- edit_file does exact string replacement. Copy the original text verbatim \
(including indentation) from a previous read_file result.
- Commands must be non-interactive. Never start commands that wait for input.
- After changing code, verify it when possible (build / run tests).
- Reply in the same language the user uses. Be concise; avoid dumping large \
file contents into your reply unless asked.";

impl Instructions {
    pub fn new(override_text: Option<String>) -> Self {
        Instructions {
            text: override_text.unwrap_or_else(|| DEFAULT.to_string()),
        }
    }
}

impl ContextProvider for Instructions {
    fn name(&self) -> &'static str {
        "instructions"
    }

    fn contribute(&self, prompt: &mut PromptContext, _ws: &Workspace) {
        prompt.system_sections.push(self.text.clone());
    }
}

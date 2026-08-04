//! 环境信息:让模型知道自己"站在哪里"。
//! 这是 Workspace Context 的最小形态,也是它的预留扩展位——
//! 未来的项目结构地图(Workspace Map)、git 状态、打开的文件等
//! 都应该扩展在这里,而不是塞进 Instructions。

use super::{ContextProvider, PromptContext};
use crate::tools::Shell;
use crate::workspace::Workspace;

pub struct WorkspaceInfo {
    shell_label: &'static str,
}

impl WorkspaceInfo {
    pub fn new(shell: &Shell) -> Self {
        WorkspaceInfo {
            shell_label: shell.label(),
        }
    }
}

impl ContextProvider for WorkspaceInfo {
    fn name(&self) -> &'static str {
        "workspace_info"
    }

    fn contribute(&self, prompt: &mut PromptContext, ws: &Workspace) {
        prompt.system_sections.push(format!(
            "Environment:\n- Working directory: {}\n- OS: {}\n- Shell for run_command: {}",
            ws.root().display(),
            std::env::consts::OS,
            self.shell_label
        ));
    }
}

//! # Context 系统:决定"每次调用模型时,模型看到什么"
//!
//! Agent 的一切能力都受限于它每轮看到的上下文。这一层把"组装上下文"
//! 抽象成两部分:
//!
//! 1. 可插拔的 [`ContextProvider`] 列表:每轮请求前,Runtime 依次调用
//!    每个 provider 的 `contribute()`,把系统提示片段注入 [`PromptContext`]。
//!    默认装配 [`instructions::Instructions`] 与 [`workspace_info::WorkspaceInfo`]。
//! 2. 消息视图来自 `session::project_model_messages` 的**单向投影**:
//!    事实日志(SessionEntry)→ 模型消息,再经 [`budget`] 做 token 预算
//!    (估算 → 折叠旧 ToolResult → 仍超预算就明确拒绝)。
//!
//! 屏幕历史、事实日志与模型上下文自此是三个不同的东西:UI-only 事实
//! (Notice 等)不进 Provider;压缩改变模型视图但不删除事实。
//!
//! ## 扩展位
//! Planning Context(当前计划/TODO 注入)、Workspace Map(项目结构地图)、
//! Memory(跨会话记忆)……都只是"再写一个 ContextProvider 并插进列表"。

pub mod budget;
pub mod instructions;
pub mod skills;
pub mod workspace_info;

use crate::message::ChatMessage;
use crate::workspace::Workspace;

/// 一轮请求最终的 prompt 形态:若干系统提示片段 + 消息列表。
/// 各 API 适配器决定 system 落到哪个字段(system / messages[0] / instructions)。
#[derive(Debug, Default, Clone)]
pub struct PromptContext {
    pub system_sections: Vec<String>,
    pub messages: Vec<ChatMessage>,
}

impl PromptContext {
    /// 拼出最终 system 文本。
    pub fn system_text(&self) -> String {
        self.system_sections.join("\n\n")
    }
}

/// 上下文提供者:向本轮 prompt 注入内容。
pub trait ContextProvider: Send {
    fn name(&self) -> &'static str;
    fn contribute(&self, prompt: &mut PromptContext, ws: &Workspace);
}

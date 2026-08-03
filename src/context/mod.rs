//! # Context 系统:决定"每次调用模型时,模型看到什么"
//!
//! Agent 的一切能力都受限于它每轮看到的上下文。这一层把"组装上下文"
//! 抽象成可插拔的 [`ContextProvider`] 列表:每轮请求前,Runtime 依次调用
//! 每个 provider 的 `contribute()`,把系统提示片段与消息注入 [`PromptContext`],
//! 最后整体交给模型 Provider 发出。
//!
//! 默认装配三个(见 `runtime.rs`):
//! 1. [`instructions::Instructions`] —— 基础行为准则(可被 config 覆盖);
//! 2. [`workspace_info::WorkspaceInfo`] —— 运行环境(cwd / OS / shell);
//! 3. [`conversation::Conversation`] —— 对话历史(唯一贡献 messages 的)。
//!
//! ## 预留的扩展位(本版刻意不实现)
//! 未来的 Planning Context(当前计划/TODO 注入)、Workspace Map
//! (项目结构地图)、Memory(跨会话记忆)……都只是"再写一个
//! ContextProvider 并插进列表"而已,Runtime 与模型层无需任何改动。
//! 压缩(Compression)也应做在这一层:Conversation 在 contribute 时
//! 把过老的消息折叠成摘要,而不是无脑全量搬运。

pub mod conversation;
pub mod instructions;
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

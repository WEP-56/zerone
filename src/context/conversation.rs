//! 对话历史:唯一向 PromptContext 贡献 messages 的 ContextProvider。
//!
//! Runtime 具体持有它(需要 push 消息),同时它也实现 ContextProvider
//! ——这样"历史进入上下文"和"环境信息进入上下文"走的是同一条路,
//! 未来做上下文压缩(Compression)时,只需要改这里的 contribute:
//! 把过老的消息折叠成一条摘要,而 Runtime 与 Provider 毫无感知。

use super::{ContextProvider, PromptContext};
use crate::message::ChatMessage;
use crate::workspace::Workspace;

#[derive(Default)]
pub struct Conversation {
    messages: Vec<ChatMessage>,
}

impl Conversation {
    pub fn new() -> Self {
        Conversation::default()
    }

    pub fn push(&mut self, m: ChatMessage) {
        self.messages.push(m);
    }

    pub fn from_messages(messages: Vec<ChatMessage>) -> Self {
        Conversation { messages }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl ContextProvider for Conversation {
    fn name(&self) -> &'static str {
        "conversation"
    }

    fn contribute(&self, prompt: &mut PromptContext, _ws: &Workspace) {
        // 全量克隆搬运:可读性优先(消息体量相对网络往返可以忽略)。
        // 上下文压缩的挂载点就在这一行。
        prompt.messages.extend(self.messages.iter().cloned());
    }
}

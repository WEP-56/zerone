//! # 统一消息模型(全项目的"通用语言")
//!
//! 这是 Onemore 最重要的设计决策:**内部只用一套消息表示**。
//!
//! 三种 API(Anthropic Messages / OpenAI Chat Completions / OpenAI Responses)
//! 的报文格式差异很大,但语义上都是同一件事:
//!
//! - 用户说了一段话
//! - 助手回了文本 和/或 若干次工具调用
//! - 工具结果作为"观察"回填给模型
//!
//! 所以我们定义与任何厂商无关的 [`ChatMessage`] / [`Block`],
//! 由 `provider/` 里的各适配器负责在"内部模型 ⇄ 厂商报文"之间双向转换。
//! 这样 Agent Loop、上下文系统、TUI 全都不需要知道你在用哪家 API,
//! 也是"运行时切换 provider 而对话不丢"的根基。
//!
//! 形状上最接近 Anthropic 的 content blocks——它是三者中对"一条消息内
//! 混合多种内容"表达得最干净的,适合当中间表示。

use serde::{Deserialize, Serialize};

/// 消息角色。注意没有 System / Tool 角色:
/// - 系统提示由 `context/` 组装,在 provider 适配层落到各 API 对应的位置;
/// - 工具结果是 User 消息里的 [`Block::ToolResult`](Anthropic 风格),
///   Chat Completions 适配器会把它拆成 `role:"tool"` 消息,Responses 适配器
///   会拆成 `function_call_output` item。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// 一条消息 = 角色 + 若干内容块。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<Block>,
}

/// 消息里的一个内容块。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Block {
    /// 普通文本。
    Text(String),

    /// 思考/推理内容(DeepSeek reasoning_content、Responses reasoning、
    /// Anthropic thinking 都归到这里)。
    ///
    /// `provider_kind` + `raw` 用来保存厂商私有数据:例如 OpenAI Responses
    /// 在 store=false 时要求把 reasoning item **原样回传**,否则带工具调用的
    /// 多轮请求会报错。适配器只回传自己家的 raw(靠 provider_kind 识别),
    /// 切换 provider 后其他家的 thinking 会被安全丢弃。
    Thinking {
        text: String,
        provider_kind: Option<String>,
        raw: Option<serde_json::Value>,
    },

    /// 模型发起的一次工具调用。
    /// `id` 用于和 ToolResult 配对(三种 API 都有这个概念,名字不同:
    /// tool_use.id / tool_calls[].id / function_call.call_id)。
    /// `input` 是已解析的 JSON 参数;如果模型输出的参数不是合法 JSON,
    /// 会以 `Value::String(原文)` 存进来,让工具层报错并把错误回给模型自愈。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// 一次工具调用的结果(即 ReAct 里的 Observation)。
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::User,
            blocks: vec![Block::Text(text.into())],
        }
    }

    /// 取出本消息里所有工具调用(Agent Loop 用它决定是否继续循环)。
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    /// 拼接本消息的全部纯文本(渲染与日志用)。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if let Block::Text(t) = b {
                out.push_str(t);
            }
        }
        out
    }
}

/// token 用量,跨轮累加后显示在状态栏。
/// 各 API 的上报位置不同(见 provider 各适配器),这里只管累加。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// 一次模型调用为何停止。Agent Loop 据此决定继续或收尾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 正常说完话(end_turn / stop / completed)。
    EndTurn,
    /// 停在工具调用上,需要执行工具后再喂回去。
    ToolUse,
    /// 撞到输出 token 上限,内容可能被截断。
    MaxTokens,
    /// 其他厂商自定义原因,原样保留。
    Other(String),
}

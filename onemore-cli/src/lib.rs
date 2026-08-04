//! Onemore:面向可靠性与实用性的 Coding Agent。
//!
//! 阅读顺序建议(每个模块头部都有详细注释):
//! 1. [`message`] —— 统一消息模型,全项目的"通用语言"
//! 2. [`workspace`] + [`tools`] —— 工具系统
//! 3. [`context`] —— 上下文组装
//! 4. [`provider`] —— 三种 API 的流式适配
//! 5. [`event`] + [`runtime`] —— Agent Loop 与事件系统
//! 6. [`storage`] —— 全局配置路径与 SQLite 会话持久化
//! 7. [`tui`] —— 只是事件流的一个消费者
//!
//! 完整文档见 docs/ 目录(01-architecture.md 起)。

pub mod config;
pub mod context;
pub mod event;
pub mod message;
pub mod provider;
pub mod runtime;
pub mod storage;
pub mod tools;
pub mod tui;
pub mod util;
pub mod workspace;

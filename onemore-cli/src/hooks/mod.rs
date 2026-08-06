//! Agent Loop 的四个稳定扩展点。
//!
//! Rust 版本使用事件专属的返回类型，避免把所有 Hook 结果塞进一个可产生非法组合的
//! TypeScript 风格联合体。Hook 按注册顺序运行；fail-open 只产生 warning，fail-closed
//! 则阻止尚未发生的副作用，或在副作用已经发生后要求 Runtime 完成事实提交再停止。

use serde_json::Value;

use crate::message::{Block, ChatMessage, Role};
use crate::tools::{ToolOutcome, ToolSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFailureMode {
    Open,
    Closed,
}

pub struct UserPromptContext<'a> {
    pub prompt: &'a str,
    pub session_id: &'a str,
}

pub struct PreToolUseContext<'a> {
    pub spec: &'a ToolSpec,
    pub arguments: &'a Value,
    pub session_id: &'a str,
}

pub struct PostToolUseContext<'a> {
    pub spec: &'a ToolSpec,
    pub arguments: &'a Value,
    pub outcome: &'a ToolOutcome,
    pub session_id: &'a str,
}

pub struct StopContext<'a> {
    pub assistant: &'a ChatMessage,
    pub session_id: &'a str,
}

pub enum UserPromptHookResult {
    Continue,
    Block(String),
    AddContext(ChatMessage),
}

pub enum PreToolUseHookResult {
    Continue,
    Block(String),
    ReplaceArguments(Value),
}

pub enum PostToolUseHookResult {
    Continue,
    ReplaceOutcome(ToolOutcome),
    StopAfterCommit(String),
}

pub enum StopHookResult {
    Continue,
    PreventStop(String),
}

pub trait Hook: Send {
    fn name(&self) -> &str;

    fn failure_mode(&self, _event: HookEvent) -> HookFailureMode {
        HookFailureMode::Closed
    }

    fn user_prompt_submit(
        &mut self,
        _ctx: &UserPromptContext<'_>,
    ) -> anyhow::Result<UserPromptHookResult> {
        Ok(UserPromptHookResult::Continue)
    }

    fn pre_tool_use(
        &mut self,
        _ctx: &PreToolUseContext<'_>,
    ) -> anyhow::Result<PreToolUseHookResult> {
        Ok(PreToolUseHookResult::Continue)
    }

    fn post_tool_use(
        &mut self,
        _ctx: &PostToolUseContext<'_>,
    ) -> anyhow::Result<PostToolUseHookResult> {
        Ok(PostToolUseHookResult::Continue)
    }

    fn stop(&mut self, _ctx: &StopContext<'_>) -> anyhow::Result<StopHookResult> {
        Ok(StopHookResult::Continue)
    }
}

#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Box<dyn Hook>>,
}

pub struct UserPromptPipeline {
    pub added_context: Vec<ChatMessage>,
    pub block: Option<String>,
    pub warnings: Vec<String>,
}

pub struct PreToolUsePipeline {
    pub arguments: Value,
    pub block: Option<String>,
    pub warnings: Vec<String>,
}

pub struct PostToolUsePipeline {
    pub outcome: ToolOutcome,
    pub stop_after_commit: Option<String>,
    pub warnings: Vec<String>,
}

pub struct StopPipeline {
    pub prevent_stop: Option<String>,
    pub warnings: Vec<String>,
}

impl HookRegistry {
    pub fn new(hooks: Vec<Box<dyn Hook>>) -> Self {
        HookRegistry { hooks }
    }

    pub fn run_user_prompt(&mut self, prompt: &str, session_id: &str) -> UserPromptPipeline {
        let mut pipeline = UserPromptPipeline {
            added_context: Vec::new(),
            block: None,
            warnings: Vec::new(),
        };
        for hook in &mut self.hooks {
            let ctx = UserPromptContext { prompt, session_id };
            match hook.user_prompt_submit(&ctx) {
                Ok(UserPromptHookResult::Continue) => {}
                Ok(UserPromptHookResult::Block(reason)) => {
                    pipeline.block = Some(format!("Hook {}: {}", hook.name(), reason));
                    break;
                }
                Ok(UserPromptHookResult::AddContext(message)) => {
                    if message.role != Role::User
                        || !message
                            .blocks
                            .iter()
                            .all(|block| matches!(block, Block::Text(_)))
                    {
                        pipeline.block = Some(format!(
                            "Hook {} 返回了非法上下文；只允许纯文本 user 消息",
                            hook.name()
                        ));
                        break;
                    }
                    pipeline.added_context.push(message);
                }
                Err(error) => handle_failure(
                    hook.as_ref(),
                    HookEvent::UserPromptSubmit,
                    error,
                    &mut pipeline.warnings,
                    &mut pipeline.block,
                ),
            }
            if pipeline.block.is_some() {
                break;
            }
        }
        pipeline
    }

    pub fn run_pre_tool(
        &mut self,
        spec: &ToolSpec,
        arguments: &Value,
        session_id: &str,
    ) -> PreToolUsePipeline {
        let mut pipeline = PreToolUsePipeline {
            arguments: arguments.clone(),
            block: None,
            warnings: Vec::new(),
        };
        for hook in &mut self.hooks {
            let ctx = PreToolUseContext {
                spec,
                arguments: &pipeline.arguments,
                session_id,
            };
            match hook.pre_tool_use(&ctx) {
                Ok(PreToolUseHookResult::Continue) => {}
                Ok(PreToolUseHookResult::Block(reason)) => {
                    pipeline.block = Some(format!("Hook {}: {}", hook.name(), reason));
                    break;
                }
                Ok(PreToolUseHookResult::ReplaceArguments(arguments)) => {
                    pipeline.arguments = arguments;
                }
                Err(error) => handle_failure(
                    hook.as_ref(),
                    HookEvent::PreToolUse,
                    error,
                    &mut pipeline.warnings,
                    &mut pipeline.block,
                ),
            }
            if pipeline.block.is_some() {
                break;
            }
        }
        pipeline
    }

    pub fn run_post_tool(
        &mut self,
        spec: &ToolSpec,
        arguments: &Value,
        outcome: ToolOutcome,
        session_id: &str,
    ) -> PostToolUsePipeline {
        let mut pipeline = PostToolUsePipeline {
            outcome,
            stop_after_commit: None,
            warnings: Vec::new(),
        };
        for hook in &mut self.hooks {
            let ctx = PostToolUseContext {
                spec,
                arguments,
                outcome: &pipeline.outcome,
                session_id,
            };
            match hook.post_tool_use(&ctx) {
                Ok(PostToolUseHookResult::Continue) => {}
                Ok(PostToolUseHookResult::ReplaceOutcome(mut outcome)) => {
                    // Harness-owned effects describe state already produced by the tool. A post
                    // hook may redact/replace the observation, but it must not forge or erase the
                    // state transition that will be validated and committed by Runtime.
                    outcome.effects = std::mem::take(&mut pipeline.outcome.effects);
                    pipeline.outcome = outcome;
                }
                Ok(PostToolUseHookResult::StopAfterCommit(reason)) => {
                    pipeline.stop_after_commit = Some(format!("Hook {}: {}", hook.name(), reason));
                }
                Err(error) => {
                    let mut block = None;
                    handle_failure(
                        hook.as_ref(),
                        HookEvent::PostToolUse,
                        error,
                        &mut pipeline.warnings,
                        &mut block,
                    );
                    if let Some(reason) = block {
                        pipeline.stop_after_commit = Some(reason);
                    }
                }
            }
        }
        pipeline
    }

    pub fn run_stop(&mut self, assistant: &ChatMessage, session_id: &str) -> StopPipeline {
        let mut pipeline = StopPipeline {
            prevent_stop: None,
            warnings: Vec::new(),
        };
        for hook in &mut self.hooks {
            let ctx = StopContext {
                assistant,
                session_id,
            };
            match hook.stop(&ctx) {
                Ok(StopHookResult::Continue) => {}
                Ok(StopHookResult::PreventStop(reason)) => {
                    pipeline.prevent_stop = Some(format!("Hook {}: {}", hook.name(), reason));
                    break;
                }
                Err(error) => handle_failure(
                    hook.as_ref(),
                    HookEvent::Stop,
                    error,
                    &mut pipeline.warnings,
                    &mut pipeline.prevent_stop,
                ),
            }
            if pipeline.prevent_stop.is_some() {
                break;
            }
        }
        pipeline
    }
}

fn handle_failure(
    hook: &dyn Hook,
    event: HookEvent,
    error: anyhow::Error,
    warnings: &mut Vec<String>,
    block: &mut Option<String>,
) {
    let message = format!("Hook {} 在 {:?} 失败: {:#}", hook.name(), event, error);
    match hook.failure_mode(event) {
        HookFailureMode::Open => warnings.push(message),
        HookFailureMode::Closed => *block = Some(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCapabilities, ToolOutput, ToolPermissionSpec, ToolSpec};
    use serde_json::json;

    struct ReplaceHook {
        name: &'static str,
        replacement: Value,
    }

    impl Hook for ReplaceHook {
        fn name(&self) -> &str {
            self.name
        }

        fn pre_tool_use(
            &mut self,
            _ctx: &PreToolUseContext<'_>,
        ) -> anyhow::Result<PreToolUseHookResult> {
            Ok(PreToolUseHookResult::ReplaceArguments(
                self.replacement.clone(),
            ))
        }
    }

    struct FailingHook {
        mode: HookFailureMode,
    }

    impl Hook for FailingHook {
        fn name(&self) -> &str {
            "failing"
        }

        fn failure_mode(&self, _event: HookEvent) -> HookFailureMode {
            self.mode
        }

        fn pre_tool_use(
            &mut self,
            _ctx: &PreToolUseContext<'_>,
        ) -> anyhow::Result<PreToolUseHookResult> {
            anyhow::bail!("boom")
        }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "test".into(),
            description: "test".into(),
            schema: json!({ "type": "object" }),
            capabilities: ToolCapabilities::READ_ONLY,
            permission: ToolPermissionSpec::default(),
        }
    }

    #[test]
    fn hooks_run_in_registration_order_with_deterministic_last_replacement() {
        let mut registry = HookRegistry::new(vec![
            Box::new(ReplaceHook {
                name: "first",
                replacement: json!({ "step": 1 }),
            }),
            Box::new(ReplaceHook {
                name: "second",
                replacement: json!({ "step": 2 }),
            }),
        ]);
        let result = registry.run_pre_tool(&spec(), &json!({}), "session");
        assert_eq!(result.arguments, json!({ "step": 2 }));
        assert!(result.block.is_none());
    }

    #[test]
    fn hook_failure_mode_is_explicit() {
        let mut open = HookRegistry::new(vec![Box::new(FailingHook {
            mode: HookFailureMode::Open,
        })]);
        let result = open.run_pre_tool(&spec(), &json!({}), "session");
        assert!(result.block.is_none());
        assert_eq!(result.warnings.len(), 1);

        let mut closed = HookRegistry::new(vec![Box::new(FailingHook {
            mode: HookFailureMode::Closed,
        })]);
        let result = closed.run_pre_tool(&spec(), &json!({}), "session");
        assert!(result.block.is_some());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn post_hook_can_replace_structured_outcome_without_losing_type() {
        struct PostHook;
        impl Hook for PostHook {
            fn name(&self) -> &str {
                "post"
            }

            fn post_tool_use(
                &mut self,
                _ctx: &PostToolUseContext<'_>,
            ) -> anyhow::Result<PostToolUseHookResult> {
                Ok(PostToolUseHookResult::ReplaceOutcome(ToolOutcome::success(
                    ToolOutput::text("redacted"),
                )))
            }
        }

        let mut registry = HookRegistry::new(vec![Box::new(PostHook)]);
        let result = registry.run_post_tool(
            &spec(),
            &json!({}),
            ToolOutcome::success(ToolOutput::text("secret")),
            "session",
        );
        assert_eq!(result.outcome.output.model_text, "redacted");
    }
}

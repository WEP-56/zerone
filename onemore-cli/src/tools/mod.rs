//! # 工具执行平台
//!
//! Runtime 只认识 [`ToolSpec`]、[`ToolContext`] 和类型化执行结果。工具失败仍会
//! 成为模型可见 Observation，但稳定错误码、UI 摘要和结构化 details 不再被压进
//! 同一根字符串。

use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::plan::PlanSnapshot;
use crate::util;
use crate::workspace::Workspace;

mod edit_file;
mod list_dir;
mod read_file;
mod run_command;
mod update_plan;
mod write_file;

pub use run_command::{detect_shell, Shell};

const RESULT_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    ParallelSafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCapabilities {
    pub read_only: bool,
    pub destructive: bool,
    pub execution_mode: ToolExecutionMode,
    pub supports_background: bool,
}

impl ToolCapabilities {
    pub const READ_ONLY: Self = ToolCapabilities {
        read_only: true,
        destructive: false,
        execution_mode: ToolExecutionMode::ParallelSafe,
        supports_background: false,
    };

    pub const MUTATION: Self = ToolCapabilities {
        read_only: false,
        destructive: true,
        execution_mode: ToolExecutionMode::Sequential,
        supports_background: false,
    };

    pub const COMMAND: Self = ToolCapabilities {
        read_only: false,
        destructive: true,
        execution_mode: ToolExecutionMode::Sequential,
        supports_background: false,
    };
}

/// 厂商无关的工具声明。Provider 只编码 name/description/schema；capabilities
/// 由 Harness 的权限和调度层消费。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub capabilities: ToolCapabilities,
    pub permission: ToolPermissionSpec,
}

/// 权限层读取的通用副作用声明。路径参数由工具自己声明，Runtime 和策略层不按工具名
/// 猜测参数形状；`always_ask` 用于命令、远端调用等无法可靠静态解析的副作用。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolPermissionSpec {
    pub path_arguments: Vec<String>,
    pub always_ask: bool,
}

impl ToolPermissionSpec {
    pub fn paths(arguments: &[&str]) -> Self {
        ToolPermissionSpec {
            path_arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            always_ask: false,
        }
    }

    pub fn opaque_side_effect(arguments: &[&str]) -> Self {
        ToolPermissionSpec {
            path_arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            always_ask: true,
        }
    }
}

/// 一次工具执行可访问的能力。后续扩展 Shell、ArtifactStore 等能力时继续从这里
/// 注入，不把 Agent、Config 或 TUI 传给工具。
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub cancel: &'a AtomicBool,
    pub session_id: &'a str,
    pub(crate) current_plan: PlanSnapshot,
    pub(crate) progress: &'a mut dyn FnMut(ToolOutput),
    pub(crate) effects: Vec<ToolEffect>,
}

impl ToolContext<'_> {
    pub fn report_progress(&mut self, mut output: ToolOutput) {
        output.model_text = sanitize_and_bound(&output.model_text);
        output.ui_summary = output
            .ui_summary
            .map(|summary| sanitize_and_bound(&summary));
        (self.progress)(output);
    }

    pub(crate) fn current_plan(&self) -> &PlanSnapshot {
        &self.current_plan
    }

    pub(crate) fn record_effect(&mut self, effect: ToolEffect) {
        self.effects.push(effect);
    }

    pub(crate) fn take_effects(&mut self) -> Vec<ToolEffect> {
        std::mem::take(&mut self.effects)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolEffect {
    PlanUpdated(PlanSnapshot),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// 唯一会进入 ToolResult 和模型上下文的正文。
    pub model_text: String,
    /// 可选的人类摘要。缺省时 UI 使用 model_text。
    pub ui_summary: Option<String>,
    /// 不进入模型的结构化信息，例如 diff、统计和截断元数据。
    pub details: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            model_text: text.into(),
            ui_summary: None,
            details: None,
        }
    }

    pub fn ui_text(&self) -> &str {
        self.ui_summary.as_deref().unwrap_or(&self.model_text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorCode {
    UnknownTool,
    InvalidArguments,
    NotFound,
    NotDirectory,
    Io,
    Conflict,
    TruncatedInput,
    Aborted,
    Timeout,
    ProcessExit,
    PermissionDenied,
    HookRejected,
    ExecutionFailed,
    Internal,
}

impl ToolErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolErrorCode::UnknownTool => "unknown_tool",
            ToolErrorCode::InvalidArguments => "invalid_arguments",
            ToolErrorCode::NotFound => "not_found",
            ToolErrorCode::NotDirectory => "not_directory",
            ToolErrorCode::Io => "io",
            ToolErrorCode::Conflict => "conflict",
            ToolErrorCode::TruncatedInput => "truncated_input",
            ToolErrorCode::Aborted => "aborted",
            ToolErrorCode::Timeout => "timeout",
            ToolErrorCode::ProcessExit => "process_exit",
            ToolErrorCode::PermissionDenied => "permission_denied",
            ToolErrorCode::HookRejected => "hook_rejected",
            ToolErrorCode::ExecutionFailed => "execution_failed",
            ToolErrorCode::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
    pub retryable: bool,
    pub details: Option<Value>,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        ToolError {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorCode::InvalidArguments, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorCode::Io, message)
    }

    pub fn execution(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorCode::ExecutionFailed, message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

/// `Send + Sync`:阶段 6 起,ParallelSafe 工具可能在 scoped threads 中并发执行,
/// 工具实现必须是无内部可变状态的(或自行加锁)。
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn prepare_arguments(&self, args: Value) -> Result<Value, ToolError> {
        Ok(args)
    }
    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    generation: u64,
}

#[derive(Debug, Clone)]
pub struct PreparedToolCall {
    tool_index: usize,
    generation: u64,
    pub spec: ToolSpec,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub output: ToolOutput,
    pub error: Option<ToolError>,
    pub(crate) effects: Vec<ToolEffect>,
}

impl ToolOutcome {
    pub fn success(output: ToolOutput) -> Self {
        ToolOutcome {
            output,
            error: None,
            effects: Vec::new(),
        }
    }

    pub fn failure(error: ToolError) -> Self {
        ToolOutcome {
            output: ToolOutput {
                model_text: error.message.clone(),
                ui_summary: None,
                details: error.details.clone(),
            },
            error: Some(error),
            effects: Vec::new(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        ToolRegistry {
            tools,
            generation: 0,
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub fn prepare(&self, name: &str, args: &Value) -> Result<PreparedToolCall, ToolError> {
        let Some((tool_index, tool)) = self
            .tools
            .iter()
            .enumerate()
            .find(|(_, tool)| tool.spec().name == name)
        else {
            let available = self
                .tools
                .iter()
                .map(|tool| tool.spec().name)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("未知工具 {:?}。可用工具: {}", name, available),
            ));
        };

        let spec = tool.spec();
        let arguments = tool.prepare_arguments(args.clone())?;
        let validation_errors = match validate_json_schema(&spec.schema, &arguments) {
            Ok(()) => Vec::new(),
            Err(errors) => errors,
        };
        if !validation_errors.is_empty() {
            return Err(ToolError {
                code: ToolErrorCode::InvalidArguments,
                message: format!(
                    "工具 {} 参数校验失败: {}",
                    spec.name,
                    validation_errors.join("; ")
                ),
                retryable: false,
                details: Some(serde_json::json!({ "errors": validation_errors })),
            });
        }

        Ok(PreparedToolCall {
            tool_index,
            generation: self.generation,
            spec,
            arguments,
        })
    }

    pub fn execute_prepared(
        &self,
        prepared: &PreparedToolCall,
        ctx: &mut ToolContext<'_>,
    ) -> ToolOutcome {
        if prepared.generation != self.generation {
            return ToolOutcome::failure(ToolError::new(
                ToolErrorCode::Conflict,
                "工具注册表已变化，请重新准备本次调用",
            ));
        }
        let Some(tool) = self.tools.get(prepared.tool_index) else {
            return ToolOutcome::failure(ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("工具 {} 已不可用", prepared.spec.name),
            ));
        };

        match tool.execute(&prepared.arguments, ctx) {
            Ok(output) => normalize_outcome(ToolOutcome::success(output)),
            Err(error) => normalize_outcome(ToolOutcome::failure(error)),
        }
    }

    pub fn execute(&self, name: &str, args: &Value, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        match self.prepare(name, args) {
            Ok(prepared) => self.execute_prepared(&prepared, ctx),
            Err(error) => ToolOutcome::failure(error),
        }
    }
}

pub(crate) fn normalize_outcome(mut outcome: ToolOutcome) -> ToolOutcome {
    outcome.output.model_text = sanitize_and_bound(&outcome.output.model_text);
    outcome.output.ui_summary = outcome
        .output
        .ui_summary
        .map(|summary| sanitize_and_bound(&summary));
    if let Some(error) = outcome.error.as_mut() {
        error.message = sanitize_and_bound(&error.message);
        // ToolOutcome::failure 从 error 构造模型 observation；保持二者严格一致。
        outcome.output.model_text = error.message.clone();
    }
    outcome
}

fn sanitize_and_bound(value: &str) -> String {
    util::truncate_middle(&util::sanitize(value), RESULT_MAX_CHARS)
}

/// 覆盖工具契约所需的 JSON Schema 子集。它不解析 `$ref`、外部 URI 或动态代码，
/// 因此 preflight 不会获得额外文件/网络能力；新增关键字时应先补测试再扩展。
fn validate_json_schema(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let Some(schema_object) = schema.as_object() else {
        return Err(vec!["schema 必须是 object".into()]);
    };
    let mut errors = Vec::new();
    if let Some(expected) = schema_object.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => {
                return Err(vec![format!("不支持的 schema type {:?}", expected)]);
            }
        };
        if !matches {
            errors.push(format!("期望类型 {},实际为 {}", expected, json_type(value)));
        }
    }
    if let Some(options) = schema_object.get("enum").and_then(Value::as_array) {
        if !options.iter().any(|option| option == value) {
            errors.push(format!("值不在 enum {:?} 中", options));
        }
    }
    if let Some(minimum) = schema_object.get("minimum").and_then(Value::as_f64) {
        if value.as_f64().is_some_and(|number| number < minimum) {
            errors.push(format!("数值不得小于 {}", minimum));
        }
    }
    if let Some(maximum) = schema_object.get("maximum").and_then(Value::as_f64) {
        if value.as_f64().is_some_and(|number| number > maximum) {
            errors.push(format!("数值不得大于 {}", maximum));
        }
    }
    if let Some(min_length) = schema_object.get("minLength").and_then(Value::as_u64) {
        if value
            .as_str()
            .is_some_and(|text| text.chars().count() < min_length as usize)
        {
            errors.push(format!("字符串长度不得小于 {}", min_length));
        }
    }
    if let Some(max_length) = schema_object.get("maxLength").and_then(Value::as_u64) {
        if value
            .as_str()
            .is_some_and(|text| text.chars().count() > max_length as usize)
        {
            errors.push(format!("字符串长度不得大于 {}", max_length));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema_object.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    errors.push(format!("缺少必填字段 {:?}", key));
                }
            }
        }
        let properties = schema_object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if schema_object
            .get("additionalProperties")
            .and_then(Value::as_bool)
            == Some(false)
        {
            for key in object.keys() {
                if !properties.contains_key(key) {
                    errors.push(format!("不允许额外字段 {:?}", key));
                }
            }
        }
        for (key, child_schema) in properties {
            if let Some(child) = object.get(&key) {
                if let Err(child_errors) = validate_json_schema(&child_schema, child) {
                    errors.extend(
                        child_errors
                            .into_iter()
                            .map(|error| format!("{}: {}", key, error)),
                    );
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn default_registry(shell: Shell) -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(read_file::ReadFile),
        Box::new(list_dir::ListDir),
        Box::new(write_file::WriteFile),
        Box::new(edit_file::EditFile),
        Box::new(run_command::RunCommand::new(shell)),
        Box::new(update_plan::UpdatePlan),
    ])
}

pub(crate) fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| ToolError::invalid_arguments(format!("缺少必填字符串参数 {:?}", key)))
}

pub(crate) fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid_arguments(format!("参数 {:?} 应为非负整数", key))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct TypedTestTool {
        name: String,
        fail: bool,
    }

    struct PreflightTestTool {
        prepares: Arc<AtomicUsize>,
        executes: Arc<AtomicUsize>,
    }

    impl Tool for PreflightTestTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "preflight".into(),
                description: "preflight test tool".into(),
                schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": { "type": "string", "minLength": 1 },
                        "count": { "type": "integer", "minimum": 1, "maximum": 3 }
                    },
                    "required": ["name", "count"]
                }),
                capabilities: ToolCapabilities::MUTATION,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn prepare_arguments(&self, args: Value) -> Result<Value, ToolError> {
            self.prepares.fetch_add(1, Ordering::Relaxed);
            if let Some(legacy) = args.get("legacy") {
                return Ok(serde_json::json!({ "name": legacy, "count": 2 }));
            }
            Ok(args)
        }

        fn execute(
            &self,
            args: &Value,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            self.executes.fetch_add(1, Ordering::Relaxed);
            Ok(ToolOutput::text(args.to_string()))
        }
    }

    struct ProgressTestTool;

    impl Tool for ProgressTestTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "progress".into(),
                description: "progress test tool".into(),
                schema: serde_json::json!({ "type": "object" }),
                capabilities: ToolCapabilities::READ_ONLY,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            ctx.report_progress(ToolOutput {
                model_text: "\u{1b}[31mworking\u{1b}[0m".into(),
                ui_summary: Some("\u{1b}[32mone item\u{1b}[0m".into()),
                details: Some(serde_json::json!({ "completed": 1 })),
            });
            Ok(ToolOutput::text("done"))
        }
    }

    impl Tool for TypedTestTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: "typed test tool".into(),
                schema: serde_json::json!({"type": "object"}),
                capabilities: ToolCapabilities::READ_ONLY,
                permission: ToolPermissionSpec::default(),
            }
        }

        fn execute(
            &self,
            _args: &Value,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            assert_eq!(ctx.session_id, "session-1");
            if self.fail {
                return Err(ToolError {
                    code: ToolErrorCode::Conflict,
                    message: "\u{1b}[31m冲突\u{1b}[0m".into(),
                    retryable: false,
                    details: Some(serde_json::json!({"source": "test"})),
                });
            }
            Ok(ToolOutput {
                model_text: "\u{1b}[32m模型正文\u{1b}[0m".into(),
                ui_summary: Some("UI 摘要".into()),
                details: Some(serde_json::json!({"count": 1})),
            })
        }
    }

    fn context<'a>(
        workspace: &'a Workspace,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(ToolOutput),
    ) -> ToolContext<'a> {
        ToolContext {
            workspace,
            cancel,
            session_id: "session-1",
            current_plan: PlanSnapshot::default(),
            progress,
            effects: Vec::new(),
        }
    }

    #[test]
    fn typed_output_keeps_model_ui_and_details_separate() {
        let registry = ToolRegistry::new(vec![Box::new(TypedTestTool {
            name: "dynamic_name".into(),
            fail: false,
        })]);
        let workspace = Workspace::new(PathBuf::from("."));
        let cancel = AtomicBool::new(false);
        let mut progress = |_| {};
        let outcome = registry.execute(
            "dynamic_name",
            &serde_json::json!({}),
            &mut context(&workspace, &cancel, &mut progress),
        );

        assert!(!outcome.is_error());
        assert_eq!(outcome.output.model_text, "模型正文");
        assert_eq!(outcome.output.ui_text(), "UI 摘要");
        assert_eq!(
            outcome.output.details,
            Some(serde_json::json!({"count": 1}))
        );
        assert_eq!(
            registry.specs()[0].capabilities,
            ToolCapabilities::READ_ONLY
        );
    }

    #[test]
    fn typed_error_code_survives_observation_conversion() {
        let registry = ToolRegistry::new(vec![Box::new(TypedTestTool {
            name: "fails".into(),
            fail: true,
        })]);
        let workspace = Workspace::new(PathBuf::from("."));
        let cancel = AtomicBool::new(false);
        let mut progress = |_| {};
        let outcome = registry.execute(
            "fails",
            &serde_json::json!({}),
            &mut context(&workspace, &cancel, &mut progress),
        );

        assert_eq!(outcome.output.model_text, "冲突");
        let error = outcome.error.unwrap();
        assert_eq!(error.code, ToolErrorCode::Conflict);
        assert_eq!(error.details, Some(serde_json::json!({"source": "test"})));
    }

    #[test]
    fn unknown_tool_has_stable_error_code() {
        let registry = ToolRegistry::new(Vec::new());
        let workspace = Workspace::new(PathBuf::from("."));
        let cancel = AtomicBool::new(false);
        let mut progress = |_| {};
        let outcome = registry.execute(
            "missing",
            &serde_json::json!({}),
            &mut context(&workspace, &cancel, &mut progress),
        );
        assert_eq!(outcome.error.unwrap().code, ToolErrorCode::UnknownTool);
    }

    #[test]
    fn schema_failures_never_reach_execute() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let executes = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new(vec![Box::new(PreflightTestTool {
            prepares: Arc::clone(&prepares),
            executes: Arc::clone(&executes),
        })]);
        let workspace = Workspace::new(PathBuf::from("."));
        let cancel = AtomicBool::new(false);
        let mut progress = |_| {};

        for arguments in [
            serde_json::json!({ "count": 1 }),
            serde_json::json!({ "name": "ok", "count": "one" }),
            serde_json::json!({ "name": "ok", "count": 1, "extra": true }),
            serde_json::json!({ "name": "ok", "count": 4 }),
        ] {
            let outcome = registry.execute(
                "preflight",
                &arguments,
                &mut context(&workspace, &cancel, &mut progress),
            );
            assert_eq!(
                outcome.error.as_ref().map(|error| error.code),
                Some(ToolErrorCode::InvalidArguments),
                "arguments should fail: {arguments}"
            );
        }

        assert_eq!(prepares.load(Ordering::Relaxed), 4);
        assert_eq!(executes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn compatibility_prepare_runs_before_validation_and_is_retained() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let executes = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new(vec![Box::new(PreflightTestTool {
            prepares: Arc::clone(&prepares),
            executes: Arc::clone(&executes),
        })]);

        let prepared = registry
            .prepare("preflight", &serde_json::json!({ "legacy": "converted" }))
            .unwrap();

        assert_eq!(
            prepared.arguments,
            serde_json::json!({ "name": "converted", "count": 2 })
        );
        assert_eq!(prepares.load(Ordering::Relaxed), 1);
        assert_eq!(executes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn progress_is_structured_and_sanitized_within_execute() {
        let registry = ToolRegistry::new(vec![Box::new(ProgressTestTool)]);
        let workspace = Workspace::new(PathBuf::from("."));
        let cancel = AtomicBool::new(false);
        let mut updates = Vec::new();
        let outcome = {
            let mut progress = |update| updates.push(update);
            registry.execute(
                "progress",
                &serde_json::json!({}),
                &mut context(&workspace, &cancel, &mut progress),
            )
        };

        assert!(!outcome.is_error());
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].model_text, "working");
        assert_eq!(updates[0].ui_text(), "one item");
        assert_eq!(
            updates[0].details,
            Some(serde_json::json!({ "completed": 1 }))
        );
    }
}

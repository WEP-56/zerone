//! # 工具系统:Tool trait + ToolRegistry
//!
//! 一个工具 = 名字 + 给模型看的说明 + JSON Schema 参数定义 + 执行逻辑。
//! Provider 适配器把 [`ToolSpec`] 翻译成各 API 的工具声明格式;
//! Agent Loop 拿到模型的 ToolUse 后,通过 [`ToolRegistry::execute`] 分发。
//!
//! ## 添加一个新工具的完整步骤(也见 docs/06-how-to-add-tool.md)
//! 1. 新建 `src/tools/my_tool.rs`,实现 [`Tool`];
//! 2. 在本文件底部 `mod` 声明 + 在 [`default_registry`] 里 push 一行;
//! 3. 完事。Agent Loop、Provider、TUI 都不需要改——它们只认 trait。
//!
//! ## 约定
//! - 返回 `Ok(内容)` 或 `Err(错误说明)`,两者都会回给模型
//!   (错误也是有效的 Observation:模型看到错误会自己纠正参数重试);
//! - 错误文案要写"模型看得懂、能行动"的话,而不是给人看的堆栈;
//! - 一切文件访问必须通过 [`Workspace`],不要直接用 `std::fs`;
//! - 输出统一经过 sanitize + truncate(在 registry 层做,工具自己不用管)。

use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::util;
use crate::workspace::Workspace;

mod edit_file;
mod list_dir;
mod read_file;
mod run_command;
mod write_file;

pub use run_command::{detect_shell, Shell};

/// 单个工具结果回传给模型前的截断上限(字符)。
/// read_file 单独放宽(见其实现),因为读文件本来就是要内容的。
const RESULT_MAX_CHARS: usize = 24_000;

/// 传给 Provider 的工具声明(与厂商无关的中间表示)。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// 标准 JSON Schema(object)。三种 API 都直接吃这个,只是包装位置不同。
    pub schema: Value,
}

/// 所有工具实现这个 trait。`Send` 是因为工具在 Runtime 工作线程上执行。
pub trait Tool: Send {
    fn name(&self) -> &'static str;
    /// 给模型看的使用说明。写清楚"什么时候用、参数含义、注意事项",
    /// 这段文字的质量直接决定模型用得好不好——它就是工具的"提示工程"。
    fn description(&self) -> String;
    /// 参数的 JSON Schema。
    fn schema(&self) -> Value;
    /// 执行。`cancel` 置位时应尽快返回(目前只有 run_command 真正轮询它)。
    fn execute(&self, args: &Value, ws: &Workspace, cancel: &AtomicBool) -> Result<String, String>;
}

/// 工具注册表:持有一组工具,负责声明导出与按名分发。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

/// 执行结果(已清洗、已截断,可直接进事件流与消息历史)。
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        ToolRegistry { tools }
    }

    /// 导出给 Provider 的工具声明列表。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description(),
                schema: t.schema(),
            })
            .collect()
    }

    /// 按名字执行一个工具调用。
    ///
    /// 注意:名字不存在、参数不是 object,这些也走"错误结果回给模型"路径
    /// 而不是 panic——Agent Loop 的健壮性大半来自"任何失败都变成 Observation"。
    pub fn execute(
        &self,
        name: &str,
        args: &Value,
        ws: &Workspace,
        cancel: &AtomicBool,
    ) -> ToolOutcome {
        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            return ToolOutcome {
                content: format!(
                    "未知工具 {:?}。可用工具: {}",
                    name,
                    self.tools
                        .iter()
                        .map(|t| t.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                is_error: true,
            };
        };
        let result = tool.execute(args, ws, cancel);
        let (raw, is_error) = match result {
            Ok(s) => (s, false),
            Err(s) => (s, true),
        };
        ToolOutcome {
            content: util::truncate_middle(&util::sanitize(&raw), RESULT_MAX_CHARS),
            is_error,
        }
    }
}

/// 组装默认工具集。`shell` 由启动时探测/配置得出,传给 run_command。
pub fn default_registry(shell: Shell) -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(read_file::ReadFile),
        Box::new(list_dir::ListDir),
        Box::new(write_file::WriteFile),
        Box::new(edit_file::EditFile),
        Box::new(run_command::RunCommand::new(shell)),
    ])
}

/// 从 JSON 参数里取必填字符串字段的小工具函数,错误信息面向模型。
pub(crate) fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("缺少必填字符串参数 {:?}", key))
}

pub(crate) fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("参数 {:?} 应为非负整数", key)),
    }
}

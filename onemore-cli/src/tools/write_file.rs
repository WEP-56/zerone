//! write_file:整文件写入(新建或覆盖),自动创建父目录。
//! 局部修改请用 edit_file——整写大文件既费 token 又容易丢内容。

use serde_json::{json, Value};

use super::{
    require_str, Tool, ToolCapabilities, ToolContext, ToolError, ToolOutput, ToolPermissionSpec,
    ToolSpec,
};

pub struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "把完整内容写入文件(不存在则创建,存在则整体覆盖,父目录自动创建)。只在新建文件或整文件重写时使用;修改既有文件的局部请用 edit_file。".into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "文件路径" },
                    "content": { "type": "string", "description": "写入的完整内容" }
                },
                "required": ["path", "content"]
            }),
            capabilities: ToolCapabilities::MUTATION,
            permission: ToolPermissionSpec::paths(&["path"]),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let path = ctx.workspace.resolve(require_str(args, "path")?);
        let content = require_str(args, "content")?;
        // 整个写入在同路径 mutation 锁内:并发批次里同文件不交错。
        let (bytes, existed) = ctx
            .workspace
            .with_file_mutation(&path, || ctx.workspace.write_text(&path, content))
            .map_err(ToolError::io)?;
        let summary = format!(
            "{} {},{} 字节,{} 行",
            if existed { "已覆盖" } else { "已创建" },
            ctx.workspace.display(&path),
            bytes,
            content.split('\n').count()
        );
        Ok(ToolOutput {
            model_text: summary.clone(),
            ui_summary: Some(summary),
            details: Some(json!({
                "path": ctx.workspace.display(&path),
                "bytes": bytes,
                "lines": content.split('\n').count(),
                "overwritten": existed,
            })),
        })
    }
}

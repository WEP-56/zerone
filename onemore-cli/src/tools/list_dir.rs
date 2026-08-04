//! list_dir:列目录。支持 depth 递归,自动跳过体积巨大的常见目录。

use std::path::Path;

use serde_json::{json, Value};

use super::{
    optional_u64, Tool, ToolCapabilities, ToolContext, ToolError, ToolErrorCode, ToolOutput,
    ToolPermissionSpec, ToolSpec,
};
use crate::workspace::Workspace;

/// 这些目录几乎不会是模型想看的,递归时跳过(仍会显示名字并标注)。
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "__pycache__", ".venv"];
/// 输出条目上限,防止对着盘根一列几万行。
const MAX_ENTRIES: usize = 500;

pub struct ListDir;

impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "列出目录内容(目录在前,附文件大小)。depth 可控制递归层数(默认 1,最大 4);.git/target/node_modules 等目录不会展开。".into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "description": "目录路径,默认工作目录" },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 4, "description": "递归层数,默认 1" }
                },
                "required": []
            }),
            capabilities: ToolCapabilities::READ_ONLY,
            permission: ToolPermissionSpec::paths(&["path"]),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let given = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let depth = optional_u64(args, "depth")?.unwrap_or(1).clamp(1, 4);
        let root = ctx.workspace.resolve(given);
        if !root.is_dir() {
            return Err(ToolError::new(
                ToolErrorCode::NotDirectory,
                format!("{} 不是目录", root.display()),
            ));
        }

        let mut out = format!("{}\n", ctx.workspace.display(&root));
        let mut count = 0usize;
        walk(
            ctx.workspace,
            &root,
            depth as usize,
            1,
            &mut out,
            &mut count,
        )?;
        let truncated = count >= MAX_ENTRIES;
        if count >= MAX_ENTRIES {
            out.push_str(&format!(
                "……[超过 {} 条,已截断;请指定子目录查看]\n",
                MAX_ENTRIES
            ));
        }
        Ok(ToolOutput {
            model_text: out,
            ui_summary: Some(format!("已列出 {} 个条目", count)),
            details: Some(json!({
                "path": ctx.workspace.display(&root),
                "entries": count,
                "depth": depth,
                "truncated": truncated,
            })),
        })
    }
}

fn walk(
    ws: &Workspace,
    dir: &Path,
    max_depth: usize,
    cur_depth: usize,
    out: &mut String,
    count: &mut usize,
) -> Result<(), ToolError> {
    let entries = ws.read_dir_sorted(dir).map_err(ToolError::io)?;
    let indent = "  ".repeat(cur_depth);
    for e in entries {
        if *count >= MAX_ENTRIES {
            return Ok(());
        }
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        *count += 1;
        if is_dir {
            let skipped = SKIP_DIRS.contains(&name.as_str());
            out.push_str(&format!(
                "{}{}/{}\n",
                indent,
                name,
                if skipped { "  [不展开]" } else { "" }
            ));
            if !skipped && cur_depth < max_depth {
                walk(ws, &e.path(), max_depth, cur_depth + 1, out, count)?;
            }
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            out.push_str(&format!("{}{}  ({})\n", indent, name, fmt_size(size)));
        }
    }
    Ok(())
}

fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

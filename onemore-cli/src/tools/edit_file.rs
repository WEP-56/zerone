//! edit_file:精确字符串替换,agent 修改代码的主力工具。
//!
//! 语义与 Claude Code 的 Edit 一致:
//! - `old_string` 必须在文件中**唯一**出现(除非 replace_all),
//!   多处匹配时报错并让模型带更多上下文重试——这是防误改的关键设计;
//! - 找不到时报错,并提示常见原因(空白不一致)。
//!
//! ## Windows 关键细节:CRLF
//! read_file 展示给模型的内容不带 \r,模型照着它拼 old_string,
//! 而磁盘上的文件很可能是 CRLF。因此这里统一:**在 LF 域里做匹配替换**,
//! 写回时若原文件以 CRLF 为主则还原成 CRLF。没有这一步,
//! edit 在 Windows 上会大面积"明明看见了却匹配不到"。

use serde_json::{json, Value};

use super::{
    require_str, Tool, ToolCapabilities, ToolContext, ToolError, ToolErrorCode, ToolOutput,
    ToolPermissionSpec, ToolSpec,
};

pub struct EditFile;

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "对文件做精确字符串替换:old_string 必须与文件内容逐字符一致(含缩进),且在文件中唯一;若出现多次,请扩大 old_string 的上下文,或设 replace_all=true 全部替换。使用前先 read_file 确认原文。".into(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "文件路径" },
                    "old_string": { "type": "string", "minLength": 1, "description": "要被替换的原文片段(逐字符精确)" },
                    "new_string": { "type": "string", "description": "替换后的内容" },
                    "replace_all": { "type": "boolean", "description": "替换所有出现,默认 false" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            capabilities: ToolCapabilities::MUTATION,
            permission: ToolPermissionSpec::paths(&["path"]),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let path = ctx.workspace.resolve(require_str(args, "path")?);
        let old = require_str(args, "old_string")?;
        let new = require_str(args, "new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old.is_empty() {
            return Err(ToolError::invalid_arguments(
                "old_string 不能为空;新建文件请用 write_file",
            ));
        }
        if old == new {
            return Err(ToolError::invalid_arguments(
                "old_string 与 new_string 相同,无事可做",
            ));
        }

        // 完整的 read-modify-write 在同路径 mutation 锁内进行:
        // 即使未来调度层允许并发,同一文件的编辑也不会交错(丢失更新)。
        let (replacements, was_crlf) = ctx.workspace.with_file_mutation(&path, || {
            let raw = ctx.workspace.read_text(&path).map_err(ToolError::io)?;
            // 进入 LF 域匹配;记录原文件是否 CRLF 以便写回时还原
            let was_crlf = raw.contains("\r\n");
            let content = raw.replace("\r\n", "\n");
            let old_lf = old.replace("\r\n", "\n");
            let new_lf = new.replace("\r\n", "\n");

            let count = content.matches(&old_lf).count();
            match count {
                0 => {
                    return Err(ToolError::new(
                        ToolErrorCode::NotFound,
                        "old_string 在文件中未找到。常见原因:缩进/空白与原文不一致、\
                         内容已被上一次编辑改动。请重新 read_file 并逐字符复制原文。"
                            .to_string(),
                    ))
                }
                1 => {}
                n if !replace_all => {
                    return Err(ToolError::new(
                        ToolErrorCode::Conflict,
                        format!(
                            "old_string 出现了 {} 次,无法确定改哪一处。\
                             请在 old_string 中包含更多上下文使其唯一,或设 replace_all=true。",
                            n
                        ),
                    ))
                }
                _ => {}
            }

            let updated = if replace_all {
                content.replace(&old_lf, &new_lf)
            } else {
                content.replacen(&old_lf, &new_lf, 1)
            };
            let final_text = if was_crlf {
                updated.replace('\n', "\r\n")
            } else {
                updated
            };
            ctx.workspace
                .write_text(&path, &final_text)
                .map_err(ToolError::io)?;
            Ok((if replace_all { count } else { 1 }, was_crlf))
        })?;
        let summary = format!(
            "已替换 {} 处,文件 {}",
            replacements,
            ctx.workspace.display(&path)
        );
        Ok(ToolOutput {
            model_text: summary.clone(),
            ui_summary: Some(summary),
            details: Some(json!({
                "path": ctx.workspace.display(&path),
                "replacements": replacements,
                "line_endings": if was_crlf { "crlf" } else { "lf" },
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn run(dir: &std::path::Path, args: Value) -> Result<ToolOutput, ToolError> {
        let ws = Workspace::new(dir.to_path_buf());
        let cancel = AtomicBool::new(false);
        EditFile.execute(
            &args,
            &mut ToolContext {
                workspace: &ws,
                cancel: &cancel,
                session_id: "test",
                current_plan: crate::plan::PlanSnapshot::default(),
                progress: &mut |_| {},
                effects: Vec::new(),
            },
        )
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onemore-test-{}", name));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unique_replace_works() {
        let d = tmpdir("edit1");
        std::fs::write(d.join("a.txt"), "hello world\nbye\n").unwrap();
        let r = run(
            &d,
            json!({"path":"a.txt","old_string":"world","new_string":"rust"}),
        );
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "hello rust\nbye\n"
        );
    }

    #[test]
    fn ambiguous_requires_replace_all() {
        let d = tmpdir("edit2");
        std::fs::write(d.join("a.txt"), "x x x").unwrap();
        let r = run(
            &d,
            json!({"path":"a.txt","old_string":"x","new_string":"y"}),
        );
        assert!(r.is_err());
        let r = run(
            &d,
            json!({"path":"a.txt","old_string":"x","new_string":"y","replace_all":true}),
        );
        assert!(r.is_ok());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "y y y");
    }

    #[test]
    fn crlf_file_matches_lf_pattern_and_stays_crlf() {
        let d = tmpdir("edit3");
        std::fs::write(d.join("a.txt"), "line1\r\nline2\r\n").unwrap();
        // 模型从 read_file 看到的是 LF 版本,old_string 不含 \r
        let r = run(
            &d,
            json!({"path":"a.txt","old_string":"line1\nline2","new_string":"line1\nlineX"}),
        );
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "line1\r\nlineX\r\n"
        );
    }

    #[test]
    fn not_found_reports_clearly() {
        let d = tmpdir("edit4");
        std::fs::write(d.join("a.txt"), "abc").unwrap();
        let r = run(
            &d,
            json!({"path":"a.txt","old_string":"zzz","new_string":"y"}),
        );
        assert!(r.unwrap_err().message.contains("未找到"));
    }
}

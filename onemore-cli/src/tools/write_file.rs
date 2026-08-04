//! write_file:整文件写入(新建或覆盖),自动创建父目录。
//! 局部修改请用 edit_file——整写大文件既费 token 又容易丢内容。

use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use super::{require_str, Tool};
use crate::workspace::Workspace;

pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "把完整内容写入文件(不存在则创建,存在则整体覆盖,父目录自动创建)。\
         只在新建文件或整文件重写时使用;修改既有文件的局部请用 edit_file。"
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径" },
                "content": { "type": "string", "description": "写入的完整内容" }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(
        &self,
        args: &Value,
        ws: &Workspace,
        _cancel: &AtomicBool,
    ) -> Result<String, String> {
        let path = ws.resolve(require_str(args, "path")?);
        let content = require_str(args, "content")?;
        let (bytes, existed) = ws.write_text(&path, content)?;
        Ok(format!(
            "{} {},{} 字节,{} 行",
            if existed { "已覆盖" } else { "已创建" },
            ws.display(&path),
            bytes,
            content.split('\n').count()
        ))
    }
}

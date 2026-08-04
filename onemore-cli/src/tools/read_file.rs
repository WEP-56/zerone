//! read_file:带行号读取文本文件,支持 offset/limit 分段读大文件。
//!
//! 行号输出(`   12 | 内容`)不只是好看:edit_file 要求模型提供精确的
//! 原文片段,行号能帮模型定位;行号格式用 " | " 分隔避免与内容混淆。

use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use super::{optional_u64, require_str, Tool};
use crate::workspace::Workspace;

/// 默认/最大单次读取行数。要更多就带 offset 再调一次(教模型分页)。
const DEFAULT_LIMIT: u64 = 1000;
const MAX_LIMIT: u64 = 4000;
/// 单行超过这个字符数会被折断显示(防止压缩过的 js 一行几万字符)。
const MAX_LINE_CHARS: usize = 500;

pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "读取文本文件内容,带行号返回。\
         大文件可用 offset(起始行号,从 1 开始)与 limit(行数,默认 1000)分段读取。\
         修改文件前应先用本工具查看现状。"
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径,相对工作目录或绝对路径" },
                "offset": { "type": "integer", "description": "起始行号(1-based),默认 1" },
                "limit": { "type": "integer", "description": "最多读取的行数,默认 1000" }
            },
            "required": ["path"]
        })
    }

    fn execute(
        &self,
        args: &Value,
        ws: &Workspace,
        _cancel: &AtomicBool,
    ) -> Result<String, String> {
        let path = ws.resolve(require_str(args, "path")?);
        let offset = optional_u64(args, "offset")?.unwrap_or(1).max(1);
        let limit = optional_u64(args, "limit")?
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        let content = ws.read_text(&path)?;
        // 统一按 \n 分行(\r 在显示层已无意义,这里直接修剪)
        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len();

        if offset as usize > total {
            return Err(format!("offset={} 超出范围,文件共 {} 行", offset, total));
        }

        let start = (offset - 1) as usize;
        let end = (start + limit as usize).min(total);
        let mut out = String::new();
        for (idx, raw) in lines[start..end].iter().enumerate() {
            let line = raw.trim_end_matches('\r');
            let shown: String = if line.chars().count() > MAX_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("{}……[本行截断]", cut)
            } else {
                line.to_string()
            };
            out.push_str(&format!("{:>6} | {}\n", start + idx + 1, shown));
        }
        if end < total {
            out.push_str(&format!(
                "……[仅显示第 {}-{} 行,共 {} 行;继续读请传 offset={}]",
                start + 1,
                end,
                total,
                end + 1
            ));
        }
        Ok(out)
    }
}

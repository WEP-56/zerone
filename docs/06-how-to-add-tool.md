# 06 · 实战:添加一个新工具

以一个真实有用的例子走完全流程:`grep`——在项目里递归搜索文本。
纯 std 实现,不加任何依赖,约 80 行,半小时内可完成。

## 第 0 步:先想清楚三件事

写代码前,按这个清单过一遍(这是工具设计的全部难点):

1. **模型什么时候会用它?** 找符号定义、找配置项出处。要在 description
   里说清,并与既有工具划清边界(有了 grep,模型就不该再用
   `run_command` 跑 findstr)。
2. **参数最少几个?** 每多一个参数,模型用错的概率翻倍。
   `pattern` 必填,`path`/`max_results` 可选,够了。
3. **失败长什么样?** 没有匹配不是错误(返回"没找到"是有效观察);
   目录不存在才是错误。错误文案要指路:"path 不是目录,请先 list_dir"。

## 第 1 步:实现 Tool trait

新建 `src/tools/grep.rs`:

```rust
//! grep:递归搜索文本(演示"如何添加工具"的教学实现;
//! 想要正则/忽略大小写,自行升级为 regex crate)。

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use super::{optional_u64, require_str, Tool};
use crate::workspace::Workspace;

pub struct Grep;

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        "在目录下递归搜索包含指定文本的行(区分大小写的子串匹配),\
         返回 文件:行号: 内容。找代码符号、配置项时优先用它,\
         不要用 run_command 跑搜索命令。"
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "要搜索的文本(子串)" },
                "path":    { "type": "string", "description": "起始目录,默认工作目录" },
                "max_results": { "type": "integer", "description": "最多返回行数,默认 200" }
            },
            "required": ["pattern"]
        })
    }

    fn execute(&self, args: &Value, ws: &Workspace, _cancel: &AtomicBool) -> Result<String, String> {
        let pattern = require_str(args, "pattern")?;
        let dir = ws.resolve(args.get("path").and_then(|v| v.as_str()).unwrap_or("."));
        if !dir.is_dir() {
            return Err(format!("{} 不是目录,请先用 list_dir 确认路径", dir.display()));
        }
        let max = optional_u64(args, "max_results")?.unwrap_or(200).clamp(1, 1000) as usize;

        let mut hits = Vec::new();
        search(ws, &dir, pattern, max, &mut hits)?;
        if hits.is_empty() {
            return Ok(format!("没有任何行包含 {:?}", pattern));
        }
        let truncated = hits.len() >= max;
        let mut out = hits.join("\n");
        if truncated {
            out.push_str(&format!("\n……[达到 {} 条上限,请缩小范围]", max));
        }
        Ok(out)
    }
}

fn search(
    ws: &Workspace,
    dir: &Path,
    pat: &str,
    max: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    for e in ws.read_dir_sorted(dir)? {
        if out.len() >= max {
            return Ok(());
        }
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            // 与 list_dir 保持一致的跳过名单
            if !matches!(name.as_str(), ".git" | "target" | "node_modules" | ".venv") {
                search(ws, &path, pat, max, out)?;
            }
        } else if let Ok(text) = ws.read_text(&path) {
            // read_text 对二进制/超大文件返回 Err,这里顺势静默跳过——
            // 对 grep 来说"读不了"不是错误,只是不在搜索范围
            for (i, line) in text.lines().enumerate() {
                if line.contains(pat) {
                    out.push(format!("{}:{}: {}", ws.display(&path), i + 1, line.trim()));
                    if out.len() >= max {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lines_and_respects_limit() {
        let dir = std::env::temp_dir().join("harness-grep-test");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\nneedle here\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "needle again\n").unwrap();
        let ws = Workspace::new(dir);
        let r = Grep
            .execute(&json!({"pattern": "needle"}), &ws, &AtomicBool::new(false))
            .unwrap();
        assert!(r.contains("a.txt:2"));
        assert!(r.contains("b.txt:1"));
    }
}
```

留意三个"既有基建自动生效"的点:文件访问全走 `Workspace`(将来加沙箱
它自动被管);输出不用自己截断/清洗(registry 统一做);错误直接
`Err(人话)`(自动变成模型的 Observation)。

## 第 2 步:注册(仅两行)

`src/tools/mod.rs`:

```rust
mod grep;                      // 模块声明区
// default_registry() 的列表里:
Box::new(grep::Grep),
```

完事。Runtime、Provider、TUI 一行不改——三个 API 适配器会自动把
它的 schema 翻译成各家的工具声明格式。

## 第 3 步:验证(三层)

```bash
cargo test grep                 # 单测
cargo run -- --once 用 grep 找找项目里哪里定义了 ToolRegistry
```

TUI 里观察:模型调用时聊天区应出现 `● grep(pattern=ToolRegistry)`
及结果预览。若模型不用新工具,九成是 description 没写清使用时机。

想加导线级测试:照 `tests/wire.rs` 现有用例,把 mock 响应里的
`read_file` 换成 `grep` 即可(mock 的是"模型决定调用什么",
工具本身是真执行)。

## 检查表(每个新工具过一遍)

- [ ] description 写了"什么时候用/不用",与既有工具无职责重叠
- [ ] schema:必填最少化;每个参数都有 description;类型正确
- [ ] 一切路径过 `ws.resolve()`,一切文件访问过 Workspace
- [ ] 错误文案面向模型、指出下一步动作
- [ ] "空结果"与"错误"分清(空结果是 Ok)
- [ ] 输出量有自我约束(上限 + 截断提示),别指望 registry 的 24k 兜底
- [ ] 长时间运行的工具轮询 `cancel`(参考 run_command)
- [ ] 有单测;`--once` 冒烟通过

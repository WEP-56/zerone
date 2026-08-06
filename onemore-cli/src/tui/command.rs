//! Slash-command catalog and the small amount of matching state used by the TUI.
//!
//! Onemore intentionally has few commands. Keeping this catalog declarative gives the
//! composer Codex-like discovery without pulling command dispatch into the widget.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Model,
    Reasoning,
    Provider,
    Session,
    Skill,
    Queue,
    Compact,
    Clear,
    Help,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: SlashCommand,
    pub name: &'static str,
    pub description: &'static str,
    pub accepts_args: bool,
}

// Presentation order follows frequency, matching Codex's command palette behavior.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: SlashCommand::Model,
        name: "model",
        description: "选择当前 provider 使用的模型",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Reasoning,
        name: "reasoning",
        description: "调整当前模型的思考程度",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Provider,
        name: "provider",
        description: "切换 API provider，保留对话历史",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Session,
        name: "session",
        description: "列出或恢复当前 workspace 的会话",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Skill,
        name: "skill",
        description: "选择并加载一个本地技能",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Queue,
        name: "queue",
        description: "排队后续任务,当前任务结束后执行",
        accepts_args: true,
    },
    CommandSpec {
        command: SlashCommand::Compact,
        name: "compact",
        description: "压缩历史:摘要替代模型视图,事实日志保留",
        accepts_args: false,
    },
    CommandSpec {
        command: SlashCommand::Clear,
        name: "clear",
        description: "清空当前会话",
        accepts_args: false,
    },
    CommandSpec {
        command: SlashCommand::Help,
        name: "help",
        description: "显示命令与键盘操作",
        accepts_args: false,
    },
    CommandSpec {
        command: SlashCommand::Quit,
        name: "quit",
        description: "退出 Onemore",
        accepts_args: false,
    },
];

pub fn find(name: &str) -> Option<&'static CommandSpec> {
    let canonical = match name {
        "exit" => "quit",
        "effort" => "reasoning",
        _ => name,
    };
    COMMANDS.iter().find(|spec| spec.name == canonical)
}

/// Exact matches first, then prefixes, then subsequence matches. Stable ordering within each
/// group makes keyboard selection predictable as the filter changes.
pub fn matches(query: &str) -> Vec<&'static CommandSpec> {
    let query = query.to_ascii_lowercase();
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut fuzzy = Vec::new();

    for spec in COMMANDS {
        if query.is_empty() {
            prefix.push(spec);
        } else if spec.name == query {
            exact.push(spec);
        } else if spec.name.starts_with(&query) {
            prefix.push(spec);
        } else if is_subsequence(&query, spec.name) {
            fuzzy.push(spec);
        }
    }
    exact.extend(prefix);
    exact.extend(fuzzy);
    exact
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = needle.chars();
    let mut next = chars.next();
    for c in haystack.chars() {
        if next == Some(c) {
            next = chars.next();
            if next.is_none() {
                return true;
            }
        }
    }
    next.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_prefix_matches_are_ranked() {
        assert_eq!(matches("mo")[0].command, SlashCommand::Model);
        assert_eq!(matches("model")[0].command, SlashCommand::Model);
    }

    #[test]
    fn fuzzy_match_is_supported() {
        assert_eq!(matches("prv")[0].command, SlashCommand::Provider);
        assert!(matches("zzz").is_empty());
    }

    #[test]
    fn exit_is_a_quit_alias() {
        assert_eq!(find("exit").map(|s| s.command), Some(SlashCommand::Quit));
    }

    #[test]
    fn effort_is_a_reasoning_alias() {
        assert_eq!(
            find("effort").map(|s| s.command),
            Some(SlashCommand::Reasoning)
        );
    }

    #[test]
    fn skill_command_accepts_arguments() {
        let spec = find("skill").unwrap();
        assert!(spec.accepts_args);
        assert_eq!(matches("ski")[0].command, SlashCommand::Skill);
    }

    #[test]
    fn copy_command_is_not_available() {
        assert!(find("copy").is_none());
        assert!(matches("copy").is_empty());
    }
}

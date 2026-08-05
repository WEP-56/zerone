//! 配置:`config.toml` 的结构与解析。
//!
//! 设计:`[providers.*]` 是一组命名的"接入方案"(profile),
//! `[agent].provider` 选当前用哪个;运行时可用 `/provider 名字` 热切换,
//! 对话历史不丢(得益于统一消息模型)。
//!
//! API key 的解析顺序(见 [`Config::resolve_provider`]):
//! 1. `api_key` 字段直接写明(`""` 表示"该服务无需鉴权",如本地 ollama);
//! 2. `api_key_env` 指定的环境变量;
//! 3. 按接口类型的默认环境变量(ANTHROPIC_API_KEY / OPENAI_API_KEY)。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::permission::{PermissionRule, PermissionRules};

/// 两类接口。字符串来自 config 的 `api = "..."`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// Anthropic Messages API
    Messages,
    /// OpenAI Responses API
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProfile {
    OpenAiResponses,
    AnthropicMessages,
    DeepSeekResponses,
    DeepSeekMessages,
}

impl ProviderProfile {
    fn parse(value: Option<&str>, api: ApiKind) -> Result<Self> {
        let profile = match value {
            None => match api {
                ApiKind::Messages => ProviderProfile::AnthropicMessages,
                ApiKind::Responses => ProviderProfile::OpenAiResponses,
            },
            Some("openai") => ProviderProfile::OpenAiResponses,
            Some("anthropic") => ProviderProfile::AnthropicMessages,
            Some("deepseek-responses") => ProviderProfile::DeepSeekResponses,
            Some("deepseek-messages") => ProviderProfile::DeepSeekMessages,
            Some(other) => bail!(
                "未知 provider profile {:?},可选: openai | anthropic | deepseek-responses | deepseek-messages",
                other
            ),
        };
        let valid = matches!(
            (api, profile),
            (ApiKind::Messages, ProviderProfile::AnthropicMessages)
                | (ApiKind::Messages, ProviderProfile::DeepSeekMessages)
                | (ApiKind::Responses, ProviderProfile::OpenAiResponses)
                | (ApiKind::Responses, ProviderProfile::DeepSeekResponses)
        );
        if !valid {
            bail!("provider profile 与 api 类型不匹配");
        }
        Ok(profile)
    }
}

impl ApiKind {
    fn parse(s: &str) -> Result<ApiKind> {
        match s {
            "messages" => Ok(ApiKind::Messages),
            "responses" => Ok(ApiKind::Responses),
            other => bail!("未知 api 类型 {:?},可选: messages | responses", other),
        }
    }

    fn default_key_env(&self) -> &'static str {
        match self {
            ApiKind::Messages => "ANTHROPIC_API_KEY",
            ApiKind::Responses => "OPENAI_API_KEY",
        }
    }
}

/// 解析完成、可直接构造 Provider 的设置(key 已就位)。
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub name: String,
    pub api: ApiKind,
    pub profile: ProviderProfile,
    pub base_url: String,
    /// 空字符串 = 不发鉴权头。
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    /// 模型上下文窗口(token)。配置后启用上下文预算强制。
    pub context_window: Option<u64>,
}

// ---- config.toml 的原始形状(serde 直接映射) ----

#[derive(Debug, Deserialize)]
struct FileConfig {
    agent: AgentSection,
    #[serde(default)]
    permissions: PermissionsSection,
    #[serde(default)]
    providers: BTreeMap<String, ProviderSection>,
}

#[derive(Debug, Deserialize, Default)]
struct PermissionsSection {
    #[serde(default)]
    workspace_read: Option<String>,
    #[serde(default)]
    workspace_write: Option<String>,
    #[serde(default)]
    outside_workspace: Option<String>,
    #[serde(default)]
    commands: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSection {
    provider: String,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    tool_timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct ProviderSection {
    api: String,
    #[serde(default)]
    profile: Option<String>,
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    context_window: Option<u64>,
}

/// 校验过的配置。
#[derive(Debug)]
pub struct Config {
    pub active_provider: String,
    /// auto | gitbash | powershell | cmd
    pub shell: String,
    pub system_prompt: Option<String>,
    /// 一轮对话里最多连续调用模型的次数(失控保护)。
    pub max_turns: u32,
    /// 单个工具调用的执行超时(None = 不限制;run_command 另有自己的超时)。
    pub tool_timeout: Option<std::time::Duration>,
    pub permission_rules: PermissionRules,
    providers: BTreeMap<String, ProviderSection>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置 {} 失败", path.display()))?;
        let raw: FileConfig =
            toml::from_str(&text).with_context(|| format!("解析配置 {} 失败", path.display()))?;
        if raw.providers.is_empty() {
            bail!("配置里没有任何 [providers.*]");
        }
        if !raw.providers.contains_key(&raw.agent.provider) {
            bail!(
                "[agent].provider = {:?} 不存在,可选: {}",
                raw.agent.provider,
                raw.providers.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        let defaults = PermissionRules::default();
        let permission_rules = PermissionRules {
            workspace_read: parse_permission_rule(
                raw.permissions.workspace_read.as_deref(),
                defaults.workspace_read,
                "workspace_read",
            )?,
            workspace_write: parse_permission_rule(
                raw.permissions.workspace_write.as_deref(),
                defaults.workspace_write,
                "workspace_write",
            )?,
            outside_workspace: parse_permission_rule(
                raw.permissions.outside_workspace.as_deref(),
                defaults.outside_workspace,
                "outside_workspace",
            )?,
            opaque_side_effect: parse_permission_rule(
                raw.permissions.commands.as_deref(),
                defaults.opaque_side_effect,
                "commands",
            )?,
        };
        Ok(Config {
            active_provider: raw.agent.provider,
            shell: raw.agent.shell.unwrap_or_else(|| "auto".into()),
            system_prompt: raw.agent.system_prompt,
            max_turns: raw.agent.max_turns.unwrap_or(50).max(1),
            tool_timeout: raw
                .agent
                .tool_timeout_secs
                .filter(|secs| *secs > 0)
                .map(std::time::Duration::from_secs),
            permission_rules,
            providers: raw.providers,
        })
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// TUI model picker 的初始目录。这里只暴露已配置的模型名，不猜测远端服务目录。
    pub fn model_names(&self) -> Vec<String> {
        self.providers
            .values()
            .map(|provider| provider.model.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// 把某个 profile 解析成可用设置(含 key 查找)。
    pub fn resolve_provider(&self, name: &str) -> Result<ProviderSettings> {
        let sec = self.providers.get(name).ok_or_else(|| {
            anyhow!(
                "provider {:?} 不存在,可选: {}",
                name,
                self.provider_names().join(", ")
            )
        })?;
        let api = ApiKind::parse(&sec.api)?;
        let profile = ProviderProfile::parse(sec.profile.as_deref(), api)?;
        let api_key = match &sec.api_key {
            Some(k) => k.clone(), // 允许 ""(无鉴权)
            None => {
                let env_name = sec
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| api.default_key_env().to_string());
                std::env::var(&env_name).map_err(|_| {
                    anyhow!(
                        "找不到 API key:请设置环境变量 {},或在 config.toml 的 \
                         [providers.{}] 里写 api_key = \"...\"(本地无鉴权服务写 api_key = \"\")",
                        env_name,
                        name
                    )
                })?
            }
        };
        Ok(ProviderSettings {
            name: name.to_string(),
            api,
            profile,
            base_url: sec.base_url.trim_end_matches('/').to_string(),
            api_key,
            model: sec.model.clone(),
            max_tokens: sec.max_tokens,
            context_window: sec.context_window,
        })
    }
}

fn parse_permission_rule(
    value: Option<&str>,
    default: PermissionRule,
    field: &str,
) -> Result<PermissionRule> {
    value
        .map(PermissionRule::parse)
        .transpose()
        .map_err(|error| anyhow!("[permissions].{}: {}", field, error))
        .map(|rule| rule.unwrap_or(default))
}

/// 首次运行时写出的模板(同时也是 config.example.toml 的内容)。
pub const EXAMPLE_CONFIG: &str = r#"# Onemore 全局配置文件(~/.onemore/config.toml)
# [agent].provider 决定当前用哪个 [providers.*];TUI 里可用 /provider 名字 热切换。

[agent]
provider = "anthropic"
# run_command 用的 shell:auto | gitbash | powershell | cmd
# auto = 找到 Git Bash 就用它(模型对 bash 语法最熟),否则退回 PowerShell
shell = "auto"
# 一轮对话里最多连续调用模型的次数(防止失控空转)
max_turns = 50
# 想完全接管系统提示就取消下面的注释:
# system_prompt = "You are ..."

# 权限规则:allow | ask | deny。hard deny(设备路径、无法安全解析的路径)不受这里覆盖。
[permissions]
workspace_read = "allow"
workspace_write = "allow"
outside_workspace = "ask"
commands = "ask"

# ---- Anthropic Messages API ----
[providers.anthropic]
api = "messages"
profile = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-sonnet-5"          # 也可: claude-opus-5 / claude-fable-5
api_key_env = "ANTHROPIC_API_KEY"
# 配置上下文窗口后启用预算:估算超限时先折叠旧工具结果,仍超限则拒绝发请求并提示 /compact
context_window = 200000
# api_key = "sk-ant-..."           # 不想用环境变量就直接写(请保护好此文件)

# ---- OpenAI Responses API(OpenAI 当前主推)----
[providers.openai]
api = "responses"
profile = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key_env = "OPENAI_API_KEY"

# ---- DeepSeek Responses API ----
[providers.deepseek]
api = "responses"
profile = "deepseek-responses"
base_url = "https://api.deepseek.com"
model = "deepseek-v4-flash"
api_key_env = "DEEPSEEK_API_KEY"
"#;

#[cfg(test)]
mod tests {
    use super::{parse_permission_rule, ApiKind, ProviderProfile, EXAMPLE_CONFIG};
    use crate::permission::PermissionRule;

    #[test]
    fn bundled_example_matches_checked_in_file() {
        assert_eq!(
            EXAMPLE_CONFIG.trim_end(),
            include_str!("../config.example.toml").trim_end()
        );
    }

    #[test]
    fn permission_rules_reject_unknown_values() {
        assert_eq!(
            parse_permission_rule(Some("deny"), PermissionRule::Allow, "commands").unwrap(),
            PermissionRule::Deny
        );
        assert!(parse_permission_rule(Some("maybe"), PermissionRule::Allow, "commands").is_err());
    }

    #[test]
    fn chat_completions_is_not_a_supported_api_kind() {
        assert!(ApiKind::parse("chat").is_err());
    }

    #[test]
    fn provider_profiles_are_family_checked() {
        assert_eq!(
            ProviderProfile::parse(None, ApiKind::Responses).unwrap(),
            ProviderProfile::OpenAiResponses
        );
        assert!(ProviderProfile::parse(Some("anthropic"), ApiKind::Responses).is_err());
    }
}

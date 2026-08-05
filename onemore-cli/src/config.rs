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
use serde::{Deserialize, Serialize};

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

pub const DEFAULT_REASONING_EFFORT: &str = "medium";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "effort", rename_all = "snake_case")]
pub enum ReasoningEffortPolicy {
    Omit,
    Send(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveModelSelection {
    pub provider: String,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalogEntry {
    pub name: String,
    pub default_model: String,
    pub models: Vec<ModelCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub efforts: Vec<String>,
    pub sends_effort: bool,
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
    pub selected_effort: String,
    pub reasoning_effort: ReasoningEffortPolicy,
}

// ---- config.toml 的原始形状(serde 直接映射) ----

#[derive(Debug, Deserialize)]
struct FileConfig {
    agent: AgentSection,
    #[serde(default)]
    permissions: PermissionsSection,
    #[serde(default)]
    providers: BTreeMap<String, RawProviderSection>,
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
struct RawProviderSection {
    api: String,
    #[serde(default)]
    profile: Option<String>,
    base_url: String,
    model: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, RawModelSection>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    context_window: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct RawModelSection {
    context_window: u64,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    reasoning: Option<RawReasoningSection>,
}

#[derive(Debug, Deserialize, Clone)]
struct RawReasoningSection {
    send_effort: bool,
    #[serde(default)]
    efforts: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProviderSection {
    api: ApiKind,
    profile: ProviderProfile,
    base_url: String,
    default_model: String,
    models: BTreeMap<String, ModelSection>,
    api_key: Option<String>,
    api_key_env: Option<String>,
}

#[derive(Debug, Clone)]
struct ModelSection {
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    reasoning: ModelReasoning,
}

#[derive(Debug, Clone)]
enum ModelReasoning {
    Omit,
    Send(Vec<String>),
}

impl ModelReasoning {
    fn efforts(&self) -> Vec<String> {
        match self {
            ModelReasoning::Omit => vec![DEFAULT_REASONING_EFFORT.to_string()],
            ModelReasoning::Send(efforts) => efforts.clone(),
        }
    }

    fn resolve(&self, effort: &str) -> Result<ReasoningEffortPolicy> {
        match self {
            ModelReasoning::Omit if effort == DEFAULT_REASONING_EFFORT => {
                Ok(ReasoningEffortPolicy::Omit)
            }
            ModelReasoning::Omit => bail!(
                "该模型未配置可发送的 reasoning effort,只能使用 {}",
                DEFAULT_REASONING_EFFORT
            ),
            ModelReasoning::Send(efforts) if efforts.iter().any(|item| item == effort) => {
                Ok(ReasoningEffortPolicy::Send(effort.to_string()))
            }
            ModelReasoning::Send(efforts) => bail!(
                "未知 reasoning effort {:?},可选: {}",
                effort,
                efforts.join(", ")
            ),
        }
    }
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
        let providers = raw
            .providers
            .into_iter()
            .map(|(name, provider)| {
                normalize_provider(&name, provider).map(|provider| (name, provider))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
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
            providers,
        })
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    pub fn provider_catalog(&self) -> Vec<ProviderCatalogEntry> {
        self.providers
            .iter()
            .map(|(name, provider)| ProviderCatalogEntry {
                name: name.clone(),
                default_model: provider.default_model.clone(),
                models: provider
                    .models
                    .iter()
                    .map(|(id, model)| ModelCatalogEntry {
                        id: id.clone(),
                        context_window: model.context_window,
                        max_tokens: model.max_tokens,
                        efforts: model.reasoning.efforts(),
                        sends_effort: matches!(model.reasoning, ModelReasoning::Send(_)),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn default_selection(&self, provider: &str) -> Result<ActiveModelSelection> {
        let section = self.provider(provider)?;
        Ok(ActiveModelSelection {
            provider: provider.to_string(),
            model: section.default_model.clone(),
            effort: DEFAULT_REASONING_EFFORT.to_string(),
        })
    }

    /// 把某个 provider 的默认模型解析成可用设置(含 key 查找)。
    pub fn resolve_provider(&self, name: &str) -> Result<ProviderSettings> {
        let selection = self.default_selection(name)?;
        self.resolve_selection(&selection)
    }

    pub fn resolve_selection(&self, selection: &ActiveModelSelection) -> Result<ProviderSettings> {
        let sec = self.provider(&selection.provider)?;
        let model = sec.models.get(&selection.model).ok_or_else(|| {
            anyhow!(
                "provider {:?} 没有模型 {:?},可选: {}",
                selection.provider,
                selection.model,
                sec.models.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        let reasoning_effort = model.reasoning.resolve(&selection.effort)?;
        let api_key = match &sec.api_key {
            Some(k) => k.clone(), // 允许 ""(无鉴权)
            None => {
                let env_name = sec
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| sec.api.default_key_env().to_string());
                std::env::var(&env_name).map_err(|_| {
                    anyhow!(
                        "找不到 API key:请设置环境变量 {},或在 config.toml 的 \
                         [providers.{}] 里写 api_key = \"...\"(本地无鉴权服务写 api_key = \"\")",
                        env_name,
                        selection.provider
                    )
                })?
            }
        };
        Ok(ProviderSettings {
            name: selection.provider.clone(),
            api: sec.api,
            profile: sec.profile,
            base_url: sec.base_url.trim_end_matches('/').to_string(),
            api_key,
            model: selection.model.clone(),
            max_tokens: model.max_tokens,
            context_window: model.context_window,
            selected_effort: selection.effort.clone(),
            reasoning_effort,
        })
    }

    pub fn validate_selection(&self, selection: &ActiveModelSelection) -> Result<()> {
        let sec = self.provider(&selection.provider)?;
        let model = sec.models.get(&selection.model).ok_or_else(|| {
            anyhow!(
                "provider {:?} 没有模型 {:?}",
                selection.provider,
                selection.model
            )
        })?;
        model.reasoning.resolve(&selection.effort)?;
        Ok(())
    }

    fn provider(&self, name: &str) -> Result<&ProviderSection> {
        self.providers.get(name).ok_or_else(|| {
            anyhow!(
                "provider {:?} 不存在,可选: {}",
                name,
                self.provider_names().join(", ")
            )
        })
    }
}

fn normalize_provider(name: &str, raw: RawProviderSection) -> Result<ProviderSection> {
    let api = ApiKind::parse(&raw.api).with_context(|| format!("[providers.{}].api 无效", name))?;
    let profile = ProviderProfile::parse(raw.profile.as_deref(), api)
        .with_context(|| format!("[providers.{}].profile 无效", name))?;
    let uses_new_format = raw.default_model.is_some() || !raw.models.is_empty();
    let uses_legacy_format =
        raw.model.is_some() || raw.max_tokens.is_some() || raw.context_window.is_some();
    if uses_new_format && uses_legacy_format {
        bail!(
            "[providers.{}] 不能混用旧 model/max_tokens/context_window 与新 default_model/models",
            name
        );
    }

    let (default_model, models) = if uses_new_format {
        let default_model = raw
            .default_model
            .ok_or_else(|| anyhow!("[providers.{}] 使用 models 时必须配置 default_model", name))?;
        if raw.models.is_empty() {
            bail!("[providers.{}].models 不能为空", name);
        }
        let models = raw
            .models
            .into_iter()
            .map(|(model_id, model)| {
                normalize_model(name, &model_id, profile, model).map(|model| (model_id, model))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        if !models.contains_key(&default_model) {
            bail!(
                "[providers.{}].default_model {:?} 不在 models 中",
                name,
                default_model
            );
        }
        (default_model, models)
    } else {
        let model = raw
            .model
            .ok_or_else(|| anyhow!("[providers.{}] 缺少 model 或 models", name))?;
        validate_model_limits(name, &model, raw.context_window, raw.max_tokens)?;
        let mut models = BTreeMap::new();
        models.insert(
            model.clone(),
            ModelSection {
                context_window: raw.context_window,
                max_tokens: raw.max_tokens,
                reasoning: ModelReasoning::Omit,
            },
        );
        (model, models)
    };

    Ok(ProviderSection {
        api,
        profile,
        base_url: raw.base_url,
        default_model,
        models,
        api_key: raw.api_key,
        api_key_env: raw.api_key_env,
    })
}

fn normalize_model(
    provider_name: &str,
    model_id: &str,
    profile: ProviderProfile,
    raw: RawModelSection,
) -> Result<ModelSection> {
    let context_window = Some(raw.context_window);
    validate_model_limits(provider_name, model_id, context_window, raw.max_tokens)?;
    let reasoning = match raw.reasoning {
        None => ModelReasoning::Omit,
        Some(reasoning) if !reasoning.send_effort => {
            if !reasoning.efforts.is_empty() {
                bail!(
                    "[providers.{}.models.{:?}.reasoning] send_effort=false 时不能配置 efforts",
                    provider_name,
                    model_id
                );
            }
            ModelReasoning::Omit
        }
        Some(reasoning) => {
            if !matches!(
                profile,
                ProviderProfile::OpenAiResponses | ProviderProfile::AnthropicMessages
            ) {
                bail!(
                    "[providers.{}.models.{:?}.reasoning] profile {:?} 尚不支持发送 effort",
                    provider_name,
                    model_id,
                    profile
                );
            }
            let mut efforts = Vec::with_capacity(reasoning.efforts.len());
            for effort in reasoning.efforts {
                let effort = effort.trim().to_string();
                if effort.is_empty() || effort.chars().count() > 64 {
                    bail!(
                        "[providers.{}.models.{:?}.reasoning].efforts 含空值或超过 64 字符",
                        provider_name,
                        model_id
                    );
                }
                if efforts.iter().any(|existing| existing == &effort) {
                    bail!(
                        "[providers.{}.models.{:?}.reasoning].efforts 重复: {:?}",
                        provider_name,
                        model_id,
                        effort
                    );
                }
                efforts.push(effort);
            }
            if !efforts
                .iter()
                .any(|effort| effort == DEFAULT_REASONING_EFFORT)
            {
                bail!(
                    "[providers.{}.models.{:?}.reasoning].efforts 必须包含默认值 {:?}",
                    provider_name,
                    model_id,
                    DEFAULT_REASONING_EFFORT
                );
            }
            ModelReasoning::Send(efforts)
        }
    };
    Ok(ModelSection {
        context_window,
        max_tokens: raw.max_tokens,
        reasoning,
    })
}

fn validate_model_limits(
    provider_name: &str,
    model_id: &str,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
) -> Result<()> {
    if context_window == Some(0) {
        bail!(
            "[providers.{}.models.{:?}].context_window 必须大于 0",
            provider_name,
            model_id
        );
    }
    if max_tokens == Some(0) {
        bail!(
            "[providers.{}.models.{:?}].max_tokens 必须大于 0",
            provider_name,
            model_id
        );
    }
    if let (Some(window), Some(max)) = (context_window, max_tokens) {
        if max > window {
            bail!(
                "[providers.{}.models.{:?}].max_tokens 不能大于 context_window",
                provider_name,
                model_id
            );
        }
    }
    Ok(())
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
api_key_env = "ANTHROPIC_API_KEY"
default_model = "claude-sonnet-5"
# api_key = "sk-ant-..."           # 不想用环境变量就直接写(请保护好此文件)

[providers.anthropic.models."claude-sonnet-5"]
context_window = 200000
max_tokens = 32000

[providers.anthropic.models."claude-sonnet-5".reasoning]
send_effort = true
efforts = ["low", "medium", "high"]

[providers.anthropic.models."claude-opus-5"]
context_window = 200000
max_tokens = 32000

# ---- OpenAI Responses API(OpenAI 当前主推)----
[providers.openai]
api = "responses"
profile = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
default_model = "gpt-5"

[providers.openai.models."gpt-5"]
context_window = 400000
max_tokens = 128000

[providers.openai.models."gpt-5".reasoning]
send_effort = true
efforts = ["none", "low", "medium", "high"]

[providers.openai.models."gpt-5-pro"]
context_window = 400000
max_tokens = 128000

[providers.openai.models."gpt-5-pro".reasoning]
send_effort = true
efforts = ["low", "medium", "high", "xhigh"]

# ---- DeepSeek Responses API ----
[providers.deepseek]
api = "responses"
profile = "deepseek-responses"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-v4-flash"

[providers.deepseek.models."deepseek-v4-flash"]
context_window = 131072
max_tokens = 8192
# 没有 reasoning table：TUI 默认 medium，请求不发送 effort 字段。
"#;

#[cfg(test)]
mod tests {
    use super::{
        parse_permission_rule, ActiveModelSelection, ApiKind, Config, ProviderProfile,
        ReasoningEffortPolicy, EXAMPLE_CONFIG,
    };
    use crate::permission::PermissionRule;

    fn load_config(text: &str) -> anyhow::Result<Config> {
        let root = std::env::temp_dir().join(format!(
            "onemore-config-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root)?;
        let path = root.join("config.toml");
        std::fs::write(&path, text)?;
        let result = Config::load(&path);
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn bundled_example_matches_checked_in_file() {
        assert_eq!(
            EXAMPLE_CONFIG.trim_end(),
            include_str!("../config.example.toml").trim_end()
        );
        load_config(EXAMPLE_CONFIG).unwrap();
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

    #[test]
    fn multi_model_catalog_resolves_limits_and_reasoning() {
        let config = load_config(
            r#"
[agent]
provider = "mock"

[providers.mock]
api = "responses"
profile = "openai"
base_url = "https://example.invalid/v1"
api_key = ""
default_model = "gpt.main/v1"

[providers.mock.models."gpt.main/v1"]
context_window = 400000
max_tokens = 128000

[providers.mock.models."gpt.main/v1".reasoning]
send_effort = true
efforts = ["none", "medium", "vendor_ultra"]

[providers.mock.models."small:model"]
context_window = 64000
max_tokens = 8000
"#,
        )
        .unwrap();

        let catalog = config.provider_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].default_model, "gpt.main/v1");
        assert_eq!(catalog[0].models.len(), 2);
        let main = catalog[0]
            .models
            .iter()
            .find(|model| model.id == "gpt.main/v1")
            .unwrap();
        assert_eq!(main.context_window, Some(400000));
        assert_eq!(main.max_tokens, Some(128000));
        assert_eq!(main.efforts, ["none", "medium", "vendor_ultra"]);

        let settings = config
            .resolve_selection(&ActiveModelSelection {
                provider: "mock".into(),
                model: "gpt.main/v1".into(),
                effort: "vendor_ultra".into(),
            })
            .unwrap();
        assert_eq!(settings.context_window, Some(400000));
        assert_eq!(settings.max_tokens, Some(128000));
        assert_eq!(
            settings.reasoning_effort,
            ReasoningEffortPolicy::Send("vendor_ultra".into())
        );

        let small = config
            .resolve_selection(&ActiveModelSelection {
                provider: "mock".into(),
                model: "small:model".into(),
                effort: "medium".into(),
            })
            .unwrap();
        assert_eq!(small.context_window, Some(64000));
        assert_eq!(small.reasoning_effort, ReasoningEffortPolicy::Omit);
    }

    #[test]
    fn legacy_single_model_config_still_loads() {
        let config = load_config(
            r#"
[agent]
provider = "legacy"
[providers.legacy]
api = "responses"
base_url = "https://example.invalid/v1"
api_key = ""
model = "old-model"
context_window = 32000
max_tokens = 4096
"#,
        )
        .unwrap();
        let settings = config.resolve_provider("legacy").unwrap();
        assert_eq!(settings.model, "old-model");
        assert_eq!(settings.context_window, Some(32000));
        assert_eq!(settings.reasoning_effort, ReasoningEffortPolicy::Omit);
    }

    #[test]
    fn rejects_mixed_formats_and_reasoning_without_medium() {
        let mixed = r#"
[agent]
provider = "mock"
[providers.mock]
api = "responses"
base_url = "https://example.invalid/v1"
api_key = ""
model = "old"
default_model = "new"
[providers.mock.models.new]
context_window = 32000
"#;
        assert!(format!("{:#}", load_config(mixed).unwrap_err()).contains("不能混用"));

        let no_medium = r#"
[agent]
provider = "mock"
[providers.mock]
api = "responses"
base_url = "https://example.invalid/v1"
api_key = ""
default_model = "new"
[providers.mock.models.new]
context_window = 32000
[providers.mock.models.new.reasoning]
send_effort = true
efforts = ["low", "high"]
"#;
        assert!(format!("{:#}", load_config(no_medium).unwrap_err()).contains("必须包含默认值"));
    }
}

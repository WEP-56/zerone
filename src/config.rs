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

/// 三类接口。字符串来自 config 的 `api = "..."`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// Anthropic Messages API
    Messages,
    /// OpenAI Chat Completions API(以及一切兼容它的服务)
    Chat,
    /// OpenAI Responses API
    Responses,
}

impl ApiKind {
    fn parse(s: &str) -> Result<ApiKind> {
        match s {
            "messages" => Ok(ApiKind::Messages),
            "chat" => Ok(ApiKind::Chat),
            "responses" => Ok(ApiKind::Responses),
            other => bail!(
                "未知 api 类型 {:?},可选: messages | chat | responses",
                other
            ),
        }
    }

    fn default_key_env(&self) -> &'static str {
        match self {
            ApiKind::Messages => "ANTHROPIC_API_KEY",
            ApiKind::Chat | ApiKind::Responses => "OPENAI_API_KEY",
        }
    }
}

/// 解析完成、可直接构造 Provider 的设置(key 已就位)。
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub name: String,
    pub api: ApiKind,
    pub base_url: String,
    /// 空字符串 = 不发鉴权头。
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u64>,
}

// ---- config.toml 的原始形状(serde 直接映射) ----

#[derive(Debug, Deserialize)]
struct FileConfig {
    agent: AgentSection,
    #[serde(default)]
    providers: BTreeMap<String, ProviderSection>,
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
}

#[derive(Debug, Deserialize, Clone)]
struct ProviderSection {
    api: String,
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
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
        Ok(Config {
            active_provider: raw.agent.provider,
            shell: raw.agent.shell.unwrap_or_else(|| "auto".into()),
            system_prompt: raw.agent.system_prompt,
            max_turns: raw.agent.max_turns.unwrap_or(50).max(1),
            providers: raw.providers,
        })
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
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
            base_url: sec.base_url.trim_end_matches('/').to_string(),
            api_key,
            model: sec.model.clone(),
            max_tokens: sec.max_tokens,
        })
    }
}

/// 首次运行时写出的模板(同时也是 config.example.toml 的内容)。
pub const EXAMPLE_CONFIG: &str = r#"# Zerone 全局配置文件(~/.zerone/config.toml)
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

# ---- Anthropic Messages API ----
[providers.anthropic]
api = "messages"
base_url = "https://api.anthropic.com"
model = "claude-sonnet-5"          # 也可: claude-opus-5 / claude-fable-5
api_key_env = "ANTHROPIC_API_KEY"
# api_key = "sk-ant-..."           # 不想用环境变量就直接写(请保护好此文件)

# ---- OpenAI Responses API(OpenAI 当前主推)----
[providers.openai]
api = "responses"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key_env = "OPENAI_API_KEY"

# ---- 同一家也可以走 Chat Completions,对比学习用 ----
[providers.openai-chat]
api = "chat"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key_env = "OPENAI_API_KEY"

# ---- Chat Completions 兼容服务举例:DeepSeek ----
[providers.deepseek]
api = "chat"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"            # deepseek-reasoner 可以看到思考流
api_key_env = "DEEPSEEK_API_KEY"

# ---- 本地模型举例:ollama(无需鉴权)----
[providers.ollama]
api = "chat"
base_url = "http://127.0.0.1:11434/v1"
model = "qwen3:8b"
api_key = ""
"#;

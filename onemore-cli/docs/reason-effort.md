# Reasoning Effort、多模型配置与 TUI 选择设计

> 状态：已实现（2026-08-05）
>
> 范围：`config.toml`、Responses/Messages 请求编码、Runtime 模型选择状态、Session Fact、TUI `/provider`、`/model` 与 reasoning 选择。
>
> 本文记录 Pi 源码研究、最终语义与实现约束。引用路径相对于工作区根目录 `E:\harness from scratch`。

## 决策摘要

Onemore 应支持一个 Provider 配置多个模型，并让每个模型独立声明：

- `context_window`；
- `max_tokens`；
- 是否发送 reasoning effort 控制字段；
- 可选的厂商原始 effort 名称。

运行时请求策略不是一个 bool：

```rust
pub enum ReasoningEffortPolicy {
    Omit,          // 不发送 effort 控制字段
    Send(String),  // 原样发送厂商 effort 值，包括 "none"
}
```

没有 reasoning 配置与 `send_effort=false` 都解析为 `Omit`，保持原有请求行为。`Send("none")` 则明确把字符串 `none` 发给远端，与省略字段语义不同。

所有 workspace、Provider 和模型的初始思考程度固定为 `medium`。用户选择非 `medium` 后，覆盖值按 `workspace + provider + model` 保存；切回 `medium` 删除覆盖。配置不允许改变这个默认值。

TUI 使用两阶段选择：

```text
/model
  -> 当前 Provider 的模型
  -> 该模型的思考程度
  -> 一次性提交完整选择

/provider
  -> Provider
  -> 切换到该 Provider 的 default_model
```

单独更改当前模型的思考程度使用 `/reasoning`，并提供 `/effort` 别名。`/provider` 只负责切换 Provider；目标 Provider 使用自己的 `default_model`，以及该 workspace 为该默认模型保存的 effort（没有保存值时为 `medium`）。

## 改造前缺口

### 配置只能表达单 Provider 单模型

当前 `ProviderSection` 只有一个 `model`、一个 `max_tokens` 和一个 `context_window`（`onemore-cli/src/config.rs:135-150`）。`ProviderSettings` 也只携带一个已解析模型（`onemore-cli/src/config.rs:84-96`）。

这会导致三个问题：

1. 同一 API key/base URL 下的多个模型只能复制成多个 `[providers.*]`，把“连接身份”和“模型身份”混在一起。
2. `/model` 可以输入任意模型，但 Runtime 只调用 `provider.set_model`，不会重新计算 context budget（`onemore-cli/src/runtime.rs:261-270`）。切到不同窗口的模型后，预算仍来自旧模型。
3. Provider 级 `max_tokens/context_window` 无法准确描述同一 Provider 下的不同模型。

### TUI 模型目录跨 Provider 混合

`Config::model_names()` 当前遍历全部 Provider，把模型合并成一个全局集合（`onemore-cli/src/config.rs:225-233`）；启动时这个全局列表被复制到 `RuntimeHandle`（`onemore-cli/src/runtime.rs:1357-1419`）。`open_model_picker()` 直接使用该列表（`onemore-cli/src/tui/mod.rs:998-1026`），所以 `/model` 会显示其他 Provider 的模型。

### 模型与 Provider 切换不是一个完整状态

当前命令分成 `SwitchProvider(String)` 和 `SetModel(String)`（`onemore-cli/src/event.rs:22-49`）：

- `SwitchProvider` 会重新构造 Provider 并更新预算；
- `SetModel` 只改模型字符串；
- 两者都立刻记录 `ModelChange` Fact（`onemore-cli/src/runtime.rs:243-311`）。

加入 effort 后若继续逐步发命令，会产生中间状态：模型已经切换，但 effort 选择被取消或尚未完成。TUI 必须先在本地暂存选择，最后发送一个原子命令。

### 请求体没有 effort 控制

Responses 请求当前只处理 reasoning item 回放与 `include: ["reasoning.encrypted_content"]`，没有 `reasoning.effort`（`onemore-cli/src/provider/openai_responses.rs:110-170`）。Anthropic Messages 当前完全不回传 thinking block，也没有 `thinking/output_config` 请求字段（`onemore-cli/src/provider/anthropic.rs:63-145`）。

因此新增 effort 不是简单加一个 TOML 字符串；它同时涉及模型目录、请求格式、历史回放、预算切换、事实记录和前端状态。

## Pi 源码观察

研究快照为 Pi `a96fb984d8c8b065fc5d193309fc812a882adee0`，快照说明见 `example/pi/SNAPSHOT.md:1-18`。该快照经过清洗，不包含 lockfile 和完整构建配置，因此以下结论来自源码检查，不代表已在本地重新运行全部测试。

### 统一等级与模型级映射

Pi 定义统一等级：

```ts
type ThinkingLevel = "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
type ModelThinkingLevel = "off" | ThinkingLevel;
type ThinkingLevelMap = Partial<Record<ModelThinkingLevel, string | null>>;
```

见 `example/pi/packages/ai/src/types.ts:27-46`。`string | null` 是关键：字符串表示厂商原始值，`null` 表示该等级不可用或应省略，而不是发送字符串 `null`。

Pi 会按 `thinkingLevelMap` 过滤可用等级；`xhigh/max` 只有显式映射时才暴露，并能把不受支持的请求夹到最近可用等级（`example/pi/packages/ai/src/models.ts:97-124`）。

这说明 Onemore 的 TUI 不应显示一套写死的全局等级。每个模型必须提供自己的候选项，厂商原始值可以是 `none`、`high`、`xhigh`、`max` 或未来的新字符串。

### OpenAI Responses

Pi 的直接 Provider 选项是：

```ts
reasoningEffort?: "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
reasoningSummary?: "auto" | "detailed" | "concise" | null;
```

见 `example/pi/packages/ai/src/api/openai-responses.ts:90-96`。简单 API 先根据模型能力 clamp，再交给 Responses adapter（同文件 `196-210`）。

请求编码会先应用 `thinkingLevelMap`，再发送：

```json
{
  "reasoning": {
    "effort": "<mapped value>",
    "summary": "auto"
  },
  "include": ["reasoning.encrypted_content"]
}
```

源码见 `example/pi/packages/ai/src/api/openai-responses.ts:312-327`。其中 `off` 映射为 `null` 时会省略显式关闭；不是 `null` 时可能发送模型映射值或 `none`。

Onemore 应采用的结论：

- effort 值按模型配置原样发送，不写死 enum；
- effort 控制与 encrypted reasoning 回放是不同能力；
- `Omit` 只省略 effort 控制，不能顺带关闭既有 reasoning 解析和安全回放。

### Anthropic Messages

Pi 同时支持两种 Messages thinking 模式：

1. 新模型 adaptive thinking：`thinking.type = "adaptive"`，程度在 `output_config.effort`；
2. 旧模型 budget thinking：`thinking.type = "enabled"`，程度由 `budget_tokens` 控制。

Provider 选项与说明见 `example/pi/packages/ai/src/api/anthropic-messages.ts:202-249`；实际请求编码见同文件 `1027-1055`。简单接口在 `forceAdaptiveThinking=true` 时映射为 effort，否则把统一等级换算为 token budget（同文件 `776-840`）。默认 budget 是 minimal 1024、low 2048、medium 8192、high 16384（`example/pi/packages/ai/src/api/simple-options.ts:53-82`）。

Onemore 首版只应实现 adaptive effort。旧式 `budget_tokens` 是另一种参数模型，必须以后用独立配置表达，不能把数字塞进 effort 字符串。

### Pi 值得采用和不应照搬的部分

值得采用：

- Provider 无关的用户意图与 Provider 请求格式分离；
- 模型级 effort 映射；
- 显式区分“发送 none/disabled”和“省略字段”；
- 可用程度由模型数据驱动；
- Responses 与 Messages 分别编码，不在 Runtime 判断厂商。

不应直接照搬：

- Pi 的 `off = null` 对 TOML 用户不够直观，Onemore 应用显式 `send_effort=false`。
- Pi 的 OpenAI-compatible Chat adapter 支持大量 `thinkingFormat`，但 Onemore 已明确只支持 Responses 与 Messages，不需要恢复 Chat 复杂度。
- 清洗快照没有保留专门验证 effort 请求体的 fixture；Onemore 必须补齐自己的 wire tests。

## 新 config.toml 结构

### 规范格式

Provider table 只保存连接和协议身份；模型 table 保存模型能力：

```toml
[agent]
provider = "openai"

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
efforts = ["none", "minimal", "low", "medium", "high"]

[providers.openai.models."gpt-5-pro"]
context_window = 400000
max_tokens = 128000

[providers.openai.models."gpt-5-pro".reasoning]
send_effort = true
efforts = ["low", "medium", "high", "xhigh"]

# 这个模型明确禁止 Onemore 发送 effort 控制字段。
[providers.openai.models."proxy-owned-model"]
context_window = 131072
max_tokens = 32768

[providers.openai.models."proxy-owned-model".reasoning]
send_effort = false
```

Messages 示例：

```toml
[providers.anthropic]
api = "messages"
profile = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
default_model = "claude-opus-4-7"

[providers.anthropic.models."claude-opus-4-7"]
context_window = 200000
max_tokens = 32000

[providers.anthropic.models."claude-opus-4-7".reasoning]
send_effort = true
efforts = ["low", "medium", "high", "xhigh"]

# 没有 reasoning table：TUI 显示 medium，请求不发送 effort 字段。
[providers.anthropic.models."claude-sonnet-4"]
context_window = 200000
max_tokens = 16000
```

模型 ID 经常包含点、斜杠、冒号或连字符，TOML key 必须允许 quoted key。实现不能用点分割模型 ID。

### Reasoning 配置语义

| 配置 | 解析结果 | TUI 候选 | 请求行为 |
|---|---|---|---|
| 没有 `reasoning` table | `Omit` | `medium` | 保持 adapter 既有行为，不新增字段 |
| `send_effort=false` | `Omit` | `medium` | 明确不创建 effort 控制字段 |
| `send_effort=true` | `Send("medium")` | `efforts` 中每个原始值 | 默认发送 `medium`，选择后发送对应原始值 |
| `send_effort=true` 且 TUI 选中 `none` | `Send("none")` | `none` | 明确发送字符串 `none`，不是省略 |

配置校验：

1. `default_model` 必须存在于 `models`。
2. `context_window` 和 `max_tokens` 必须大于 0，且 `max_tokens <= context_window`。
3. `send_effort=true` 时，`efforts` 必须非空、无重复、每项 trim 后非空且不超过 64 字符，并必须包含 `medium`。
4. `send_effort=false` 时不得同时配置 `efforts`，避免看似生效但被忽略。
5. `send_effort=true` 但 Provider Profile 没有 effort encoder 时，启动直接报配置错误，不能把字段发到猜测位置。
6. Provider/model 目录按 TOML 的逻辑 ID 查找；当前实现使用 `BTreeMap` 的稳定字典序。

### 兼容旧配置

当前格式：

```toml
[providers.openai]
model = "gpt-5"
max_tokens = 128000
context_window = 400000
```

应在一个过渡版本内解析成仅含一个模型的目录：

```text
default_model = model
models[model].max_tokens = provider.max_tokens
models[model].context_window = provider.context_window
models[model].reasoning = absent -> Omit（UI effort 仍为 medium）
```

同一个 Provider 不能同时使用旧 `model` 字段和新 `default_model/models`，混用应报错。`config.example.toml` 和首次启动模板只展示新格式。

## Rust 数据模型

建议原始配置与解析后配置分离：

```rust
#[derive(Debug, Deserialize, Clone)]
struct ProviderSection {
    api: String,
    profile: Option<String>,
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,

    // Canonical multi-model format.
    default_model: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, ModelSection>,

    // One-release legacy input only.
    model: Option<String>,
    max_tokens: Option<u64>,
    context_window: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct ModelSection {
    context_window: u64,
    max_tokens: Option<u64>,
    reasoning: Option<ReasoningSection>,
}

#[derive(Debug, Deserialize, Clone)]
struct ReasoningSection {
    send_effort: bool,
    #[serde(default)]
    efforts: Vec<String>,
}
```

解析后的运行时目录：

```rust
pub struct ProviderCatalogEntry {
    pub name: String,
    pub api: ApiKind,
    pub profile: ProviderProfile,
    pub default_model: String,
    pub models: Vec<ModelCatalogEntry>,
}

pub struct ModelCatalogEntry {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub efforts: Vec<String>,
    pub sends_effort: bool,
}

pub struct ActiveModelSelection {
    pub provider: String,
    pub model: String,
    pub effort: String,
}
```

`ProviderSettings` 必须从 `(provider_name, model_id, reasoning_selection)` 一次解析，不能先 resolve Provider 后再用 `set_model()` 绕开模型配置。

## Provider 请求编码

### Capability 不是 bool

现有 `ProviderCapabilities` 只描述 reasoning 的流格式和回放能力（`onemore-cli/src/provider/mod.rs:38-72`）。effort 需要描述“如何编码”：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffortFormat {
    Unsupported,
    OpenAiResponses,
    AnthropicAdaptive,
}

pub struct ProviderCapabilities {
    // existing fields...
    pub reasoning_effort_format: ReasoningEffortFormat,
}
```

建议初始矩阵：

| Profile | Effort format | `Send("high")` |
|---|---|---|
| OpenAI Responses | `OpenAiResponses` | `reasoning.effort = "high"` |
| Anthropic Messages | `AnthropicAdaptive` | `thinking.type = "adaptive"` + `output_config.effort = "high"` |
| DeepSeek Responses | `Unsupported`，直到按官方协议补 fixture | 配置时拒绝 |
| DeepSeek Messages | `Unsupported`，直到按官方协议补 fixture | 配置时拒绝 |

未来发现某个 Provider Profile 使用不同字段时，应新增受测试的 enum variant，不允许用户在 TOML 中填写任意 JSON path。

### Responses

```rust
match &settings.reasoning {
    ReasoningEffortPolicy::Omit => {
        // Do not create body["reasoning"] for effort control.
    }
    ReasoningEffortPolicy::Send(value) => {
        body["reasoning"] = json!({ "effort": value });
    }
}
```

`include: ["reasoning.encrypted_content"]`、reasoning stream 解析和历史原样回放仍由原 capability 控制，不受 `Omit` 影响。否则“不要设置程度”会意外退化多轮 reasoning 连续性。

### Messages

```rust
match &settings.reasoning {
    ReasoningEffortPolicy::Omit => {
        // Do not create thinking/output_config effort controls.
    }
    ReasoningEffortPolicy::Send(value) => {
        body["thinking"] = json!({ "type": "adaptive" });
        body["output_config"] = json!({ "effort": value });
    }
}
```

启用 Anthropic thinking 后，历史 thinking/signature 回放和 interleaved thinking 规则也必须完整实现；不能只发送 `output_config.effort`。这部分风险高于 Responses，应有独立 wire fixture。

### 指纹与缓存

完整 `prompt_fingerprint` 当前包含 profile、model、system、tools 和 messages；稳定 `prompt_cache_key` 只包含 profile、model、system 和 tools（`onemore-cli/src/provider/mod.rs:189-225`）。新增 effort 后：

- 完整请求 fingerprint 必须加入 resolved reasoning policy，保证 `Omit/Send("medium")/Send("none")/Send("high")` 可审计地区分。
- `prompt_cache_key` 可以继续不包含 effort，因为 effort 是生成参数，不改变输入 token 前缀；相同模型与上下文仍可复用前缀缓存。
- 如果未来厂商明确把 effort 纳入缓存隔离键，再按 Provider Profile 调整，不能全局猜测。

## TUI 行为

### 前端需要结构化目录

删除全局 `RuntimeHandle.model_names: Vec<String>`，改为只读 catalog：

```rust
pub struct RuntimeHandle {
    // channels...
    pub initial_selection: ActiveModelSelection,
    pub provider_catalog: Vec<ProviderCatalogEntry>,
}
```

TUI 不再从 `provider_label.split(" / ")` 反解析状态。`App` 直接保存结构化 `ActiveModelSelection`；label 只用于渲染：

```text
openai / gpt-5 / effort=high
anthropic / claude-sonnet-4 / effort=medium
proxy / special-model / effort=medium
```

显式 `none` 必须显示为 `effort=none`，不能显示成 `omit`。

### `/model`

1. 从 `App.selection.provider` 找到当前 Provider catalog。
2. 只显示该 Provider 的 `models`。
3. 不再显示“自定义模型…”；未知模型没有 context window 与 reasoning metadata，不能安全切换。
4. 选择模型后不立即发 Runtime 命令，转到该模型的 reasoning picker。
5. reasoning 确认后发送一个完整 `SelectModel` 命令。
6. reasoning picker 按 Esc 时，Provider/model/effort 全部保持原值。

直接输入 `/model gpt-5` 时，先验证 `gpt-5` 属于当前 Provider，再打开它的 reasoning picker，不能立即切换。可选的无交互完整形式为：

```text
/model gpt-5 high
```

只有 model 和 effort 都有效时才发送一次原子命令。

reasoning picker 候选：

| 模型配置 | 显示项 |
|---|---|
| 没有 reasoning table | `medium`（请求不发送字段） |
| `send_effort=false` | `medium`（请求不发送字段） |
| `send_effort=true` | 按配置顺序显示所有 effort，初始选中 workspace 偏好或 `medium` |

即使只有一个候选，也保留第二步确认，满足“选择模型后再次选择思考程度”的一致交互。

### `/provider`

1. 显示 Provider picker。
2. 选择 Provider 后立即发送 `SwitchProvider(provider)`，不打开模型或 reasoning picker。
3. Runtime 切换到目标 Provider 的 `default_model`。
4. 该模型在当前 workspace 有保存的 effort 时恢复它，否则使用 `medium`。
5. Esc 关闭 Provider picker，不产生 Session Fact。

直接输入 `/provider openai` 时执行同样的 Provider 切换：

```text
/provider openai
```

### `/reasoning` 与 `/effort`

`/reasoning` 打开当前 Provider/current model 的 reasoning picker，只改变 reasoning policy。`/effort` 是别名，不增加第二套行为。

可选完整形式：

```text
/reasoning high
/effort none
```

对于不发送 effort 的模型只允许 `medium`；对于可发送模型只接受配置中的精确值。

### 选择器状态机

```text
Idle
  |-- /model ----------------> ModelPicker(current provider)
  |-- /provider -------------> ProviderPicker
  |-- /reasoning ------------> ReasoningPicker(current provider/model)

ProviderPicker --select------> send SwitchProvider(provider)
ModelPicker ----select-------> ReasoningPicker(current provider/selected model)
ReasoningPicker -select------> send SelectModel { model, effort }

Any picker ------Esc---------> Idle (no command, no mutation)
```

建议用一个 overlay state 携带 pending selection：

```rust
struct PendingModelSelection {
    model: String,
    effort_only: bool,
}

enum PickerKind {
    Provider,
    Model,
    Reasoning,
    Session,
}
```

## Runtime、事件与持久事实

### 原子命令

替换分离的模型选择命令：

```rust
AgentCommand::SelectModel { model: String, effort: String }
```

Runtime 处理顺序：

1. 以当前 Provider 组成并校验 provider/model/effort 组合。
2. 解析 API key 与完整 `ProviderSettings`。
3. 用模型自己的 `context_window/max_tokens` 构造新 budget 与 Provider。
4. 先提交 Session Fact。
5. commit 成功后一次性替换 provider、budget 和 active selection。
6. 发送结构化事件。

如果 Provider 构造或 Fact commit 失败，旧 provider/model/effort/budget 必须全部保持不变。

活动 turn 中收到选择命令时沿用现有 deferred command 机制，在当前 turn 完整结束后执行，不中途改变请求编码。

### Workspace 偏好与 Session Fact

workspace 偏好存放在：

```text
~/.onemore/workspaces/<sha256(canonical-workspace-key)>.json
```

只保存偏离 `medium` 的 `provider -> model -> effort` 覆盖。新 workspace、新模型和已删除覆盖都自然回到 `medium`；选择 `medium` 会删除对应项，文件为空时删除文件。

Session Fact 用于审计实际发生过的选择：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelChangeRecord {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_reasoning_effort")]
    pub effort: String,
}
```

旧 Fact 缺少 effort 时反序列化为 `medium`。偏好负责下次启动/选择时的恢复，Fact 不反向覆盖当前 workspace 偏好。

### Agent event

替换只有 label 的 `ProviderChanged`：

```rust
AgentEvent::ModelSelectionChanged {
    provider: String,
    model: String,
    effort: String,
    label: String,
}
```

TUI 只在收到此事件后更新当前状态，不能在 picker Enter 时先乐观改变 label。

## 测试矩阵

### 配置

- 一个 Provider 多模型成功解析；每个模型保留自己的窗口和输出上限。
- `default_model` 不存在、空 models、零窗口、`max_tokens > context_window` 均拒绝。
- model ID 含点、斜杠、冒号和连字符时精确查找。
- reasoning table 缺失 → 选中 `medium`、请求策略为 Omit。
- `send_effort=false` → Omit；与 efforts 混用拒绝。
- `send_effort=true` 的空/重复/超长 efforts、缺失 `medium` 均拒绝。
- 自定义 effort `none/xhigh/vendor_ultra` 原样保留。
- 旧单模型格式迁移；新旧格式混用拒绝。

### Provider wire fixtures

| Profile | Fixture |
|---|---|
| OpenAI Responses | Omit 不发送；Send medium/high/none；encrypted replay 不受 Omit 影响 |
| Anthropic Messages | Omit；Send high 产生 adaptive + output_config；thinking signature 多轮回放 |
| DeepSeek Responses | Send 配置在本地拒绝，直到实现格式 fixture |
| DeepSeek Messages | Send 配置在本地拒绝，直到实现格式 fixture |

每个 fixture 同时断言请求里没有其他 Profile 的私有字段。

### Runtime

- 选择不同模型会同步更新 provider model、context budget、max tokens 和 effort。
- 无效组合与存储失败保持旧状态。
- 一个选择只追加一个 ModelChange Fact、发送一个 event。
- 活动 turn 中选择延迟到 turn 结束，不影响当前请求。
- workspace 偏好按模型隔离；新 workspace/模型回到 medium，切回 medium 删除覆盖。
- 完整 fingerprint 区分 Omit/Send 及不同值；稳定 cache key 不因 effort 改变。

### TUI

- `/model` 只显示当前 Provider 模型，绝不出现其他 Provider 模型。
- 模型确认后必定进入 reasoning picker，不提前发送命令。
- `/provider` 只发送 Provider 切换，不串联 model/reasoning picker。
- `/reasoning` 只显示当前模型允许的候选。
- 每一层 Esc 都不发送命令、不改变 label。
- 最终确认只发送一个完整 `SelectModel`。
- medium、显式 none 与其他厂商值在状态栏中原样显示。
- direct slash 参数合法/缺失/未知时行为确定。

## 实施顺序

1. 引入多模型 config 数据结构、严格校验和旧格式迁移测试。
2. 用 `ActiveModelSelection` 与原子 `SelectModel` 替换分离的切换路径，修复 `/model` 不更新 budget 的现有问题。
3. 扩展 ModelChange Fact 和结构化 Agent event。
4. 重构 RuntimeHandle/TUI catalog，实现 Provider 过滤和分阶段 picker。
5. 先实现 OpenAI Responses effort wire fixtures 与编码。
6. 完整实现 Anthropic adaptive thinking、signature replay 和 wire fixtures。
7. 最后评估 DeepSeek 两个 Profile 的官方 effort 格式；没有证据前保持 Unsupported。
8. 更新 `config.example.toml`、README、帮助文本和中英文 API 兼容文档。

## 验收标准

- 一个 `[providers.*]` 可以配置多个模型，每个模型有独立 context window 和 max tokens。
- `/model` 目录严格限定当前 Provider。
- 选择模型后必须完成 reasoning 选择，最终只有一次原子状态变更。
- 未配置 reasoning 的模型保持现有行为。
- `send_effort=false` 时不发送 effort 控制字段。
- 显式 `none` 会被发送，且不会被误判为 Omit。
- Responses 与 Messages 仅发送自身 Profile 支持的字段。
- 模型切换同步更新 context budget，不再沿用旧模型窗口。
- 非 medium effort 按 workspace/provider/model 持久恢复，并可从完整 fingerprint 审计。
- reasoning effort 不造成稳定 prompt cache key 抖动。

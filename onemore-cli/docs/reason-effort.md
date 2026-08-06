# Reasoning Effort 配置与 TUI 行为

## 目标

Onemore 将“用户可选的思考程度”和“是否在请求中发送 effort”归一化为每模型能力。
TUI 只消费已校验的模型目录，不猜测远端支持哪些字符串，也不重命名自定义值。

配置结构拒绝未知字段，层级或拼写错误会在启动时直接报错。当前格式以
`config.example.toml` 为准。

## Profile 标准列表

模型没有配置 `efforts` 时，Onemore 根据 provider profile 使用固定标准列表：

| profile | 默认可选值 | 请求字段 |
|---|---|---|
| `openai` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `reasoning.effort` |
| `anthropic` | `low`, `medium`, `high`, `xhigh`, `max` | `output_config.effort`，同时启用 adaptive thinking |
| `deepseek-responses` | 不发送；TUI 仅显示本地默认 `medium` | 无 |
| `deepseek-messages` | 不发送；TUI 仅显示本地默认 `medium` | 无 |

`profile` 描述线路协议，而不是模型品牌。通过 OpenAI Responses 兼容网关调用其他
品牌模型时，只要网关接受 `reasoning.effort`，就应使用 `profile = "openai"`。

## 模型配置

### 使用 profile 标准

省略 `efforts`：

```toml
[providers.openai.models."gpt-5"]
context_window = 400000
max_tokens = 128000
```

OpenAI profile 的 TUI 会显示七个标准值，初始选择 `medium`。

### 自定义完整列表

非空数组完整覆盖 profile 标准，不做并集：

```toml
[providers.openai.models."deepseek-v4-flash-0731"]
context_window = 400000
max_tokens = 32000
efforts = ["low", "high", "max"]
```

该模型的 TUI 只显示 `low`、`high`、`max`。列表不要求包含 `medium`；未配置
`default_effort` 时，优先使用 `medium`，若列表没有 `medium`，则使用第一项，此例为
`low`。

可显式指定默认值，但它必须出现在 `efforts` 中：

```toml
efforts = ["low", "high", "max"]
default_effort = "high"
```

### 禁止发送 effort

对挂在 `openai` 或 `anthropic` profile 下、但实际不支持 effort 的模型，写空数组：

```toml
[providers.openai.models."plain-model"]
context_window = 131072
max_tokens = 8192
efforts = []
```

此时 TUI 只显示 `medium` 并注明“不发送 effort 字段”。`efforts = []` 不能同时配置
`default_effort`。

DeepSeek profiles 本身不定义 effort 编码，因此不允许配置 `efforts` 或
`default_effort`；需要通过兼容网关发送 OpenAI reasoning 字段时使用 `openai` profile。

## 校验规则

- `efforts` 保持配置顺序，trim 后每项必须非空、不超过 64 字符且不得重复。
- `default_effort` trim 后必须非空、不超过 64 字符，并且必须属于最终列表。
- 空数组表示明确关闭；缺省表示使用 profile 标准，两者语义不同。
- 所有配置结构都拒绝未知字段，避免拼写错误或层级错误静默失效。
- `api_key` 与 `api_key_env` 互斥；`shell`、`max_turns`、URL 和模型 ID 在启动时校验。

## TUI 与持久偏好

`/model` 先选择模型，再打开该模型的 effort picker；`/reasoning` 直接打开当前模型的
picker。picker 的顺序与 `efforts` 顺序一致，当前 workspace 的有效偏好优先于模型
默认值。

选中值按 `workspace/provider/model` 保存。切回该模型的 `default_effort` 时删除覆盖，
不会把一个模型的选择泄漏给另一个模型。配置变更使旧偏好失效时，启动会回退到新的
模型默认值。

## 请求编码

OpenAI Responses：

```json
{
  "reasoning": { "effort": "max" }
}
```

Anthropic Messages：

```json
{
  "thinking": { "type": "adaptive" },
  "output_config": { "effort": "max" }
}
```

自定义值原样传递。Onemore 不把 `max`、`xhigh` 或网关私有字符串转换成其他名称。
当模型归一化为“不发送”时，上述控制字段均不会出现在请求中。

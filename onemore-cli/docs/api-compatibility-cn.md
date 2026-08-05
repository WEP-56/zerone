# API 兼容性与 Chat Completions 移除

## 决策

移除 OpenAI Chat Completions 适配器。Onemore 将支持两类提供商协议：

- OpenAI Responses 风格 API；
- Anthropic Messages 风格 API。

虽然 Chat Completions 在更广泛的生态系统中仍然有价值，但维护第三种一等协议会成倍增加请求编码、流解析、工具调用配对、用量统计解析、上下文标准化、兼容性测试以及缓存策略的复杂度。对于该学习项目的目标而言，它并非必需。

这是一次破坏性配置变更。任何使用 `api = "chat"` 的配置文件都会被作为未知 API 类型直接拒绝。本项目尚未公开发布，因此不提供配置迁移路径；调用方需要明确选择 `messages` 或 `responses`。

---

## 兼容性原则

Responses 和 Messages 是提供商协议（Provider Protocols），而不是具备完全语义一致性的通用标准。

某个提供商可能接受相同的 JSON 结构，但会：

- 忽略某些字段；
- 重映射模型名称；
- 省略部分流式事件；
- 对状态（state）、推理（reasoning）、缓存（caching）和工具（tools）赋予不同含义。

因此，Onemore 采用三层架构：

```text
Session and Runtime
  provider-neutral messages, tool calls, results, permissions, and events

Provider family adapter
  Responses or Messages request/stream conversion

Provider profile capabilities
  model and vendor-specific supported behavior
```

即：

```text
会话与运行时
  与提供商无关的消息、工具调用、结果、权限和事件

提供商协议族适配器
  Responses 或 Messages 的请求/流转换

提供商能力配置（Capabilities）
  模型及厂商特定的功能支持情况
```

Runtime（运行时）不得基于提供商名称进行分支处理。协议适配器负责所有网络协议细节，而 Capabilities 负责决定哪些可选行为被启用。

---

## 基础协议族

### Responses

Responses 使用类型化（typed）的输入和输出项。

其中：

- `message`
- `reasoning`
- `function_call`
- `function_call_output`

均为独立条目（item）。

工具结果通过 `call_id` 进行关联。

OpenAI Responses 可能支持：

- 加密推理回放（encrypted reasoning replay）
- 有状态链式调用（stateful chaining）
- Conversations
- Cache Keys
- 显式缓存断点（explicit cache breakpoints）
- 内置工具（built-in tools）

从 Onemore 的角度来看，这些都属于可选能力。

### Messages

Messages 使用用户消息与助手消息交替出现的结构，并通过类型化内容块（typed content blocks）表示内容。

其中：

- 工具调用使用 `tool_use`
- 工具结果使用 `tool_result`

系统指令（system instructions）、思考内容（thinking）、缓存控制（cache controls）、图片、文档以及服务端工具等功能，都属于由 Capability 控制的扩展能力。

---

## 初始 Provider Profile

第一版能力矩阵覆盖：

- OpenAI
- Anthropic
- DeepSeek

能力描述应为数据驱动，而不是在适配器中到处分散编写厂商名称判断。

```rust
pub struct ProviderCapabilities {
    pub encrypted_reasoning_replay: bool,
    pub reasoning_summary_stream: bool,
    pub reasoning_text_stream: bool,
    pub previous_response_id: bool,
    pub conversations: bool,
    pub prompt_cache_key: bool,
    pub explicit_cache_control: bool,
    pub input_images: bool,
    pub input_files: bool,
    pub server_web_search: bool,
    pub parallel_tool_calls_control: bool,
    pub reasoning_effort_format: ReasoningEffortFormat,
}
```

`ReasoningEffortFormat` 是编码器选择，而不是简单的支持/不支持 bool：OpenAI
Responses 使用 `reasoning.effort`；Anthropic Messages 使用 adaptive `thinking` 与
`output_config.effort`。DeepSeek Profile 在有对应请求 Fixture 前保持不支持。

具体表示形式未来可以演进，但每项功能都必须明确声明默认值为“不支持”。

| Profile | Family | 重要限制 |
|----------|----------|----------|
| OpenAI | Responses | 支持标准 Responses Item 模型；可选能力仍取决于具体模型。 |
| Anthropic | Messages | 支持标准 Messages Block 模型；高级 Block 依赖具体模型支持。 |
| DeepSeek Responses | Responses | 无状态；不支持 `previous_response_id`、Conversation、Cache Key、数据保留、加密推理、图片和文件。上下文缓存自动处理。 |
| DeepSeek Anthropic | Messages | Anthropic 兼容接口；会忽略 `cache_control`、`anthropic-version` 和 `anthropic-beta`；部分多模态能力和 MCP Block 不受支持。 |

DeepSeek 的兼容性文档明确指出：不支持的字段可能会被静默忽略。

因此，Onemore 不得依赖远端返回错误来判断功能是否受支持。

---

## 已知 Responses 兼容性缺口

当前 Responses 适配器处理的是：

```text
response.reasoning_summary_text.delta
```

而 DeepSeek Responses 文档定义的是：

```text
response.reasoning_text.delta
response.reasoning_text.done
```

适配器必须在对应 Profile 声明支持的情况下，将这两种事件格式统一标准化处理。

此外，最终生成的 reasoning item 也必须按照对应 Profile 的规则解析：

- OpenAI 可能要求保留加密后的原始推理项，以便后续轮次回放；
- DeepSeek 返回的是明文推理内容，并且不支持加密推理回放。

不要通过接受任意第三方原始推理数据（raw reasoning items）来解决此问题。

原始推理数据只能回放到生成它的同一 Provider Profile 中。

---

## Chat Completions 移除范围

本次移除涉及所有公开接口和测试中对 `ApiKind::Chat` 的引用：

- 删除 `src/provider/openai_chat.rs` 及其模块导出；
- 删除 `Chat` 枚举成员及对应解析逻辑；
- 删除 Chat 请求、响应体与流式处理测试，以及相关 Wire Fixture；
- 在配置阶段将 `api = "chat"` 作为不支持的 API 类型直接拒绝；
- 删除 `openai-chat` 示例配置及相关文档引用；
- 更新软件包和 README 中的验证统计数量。

现有用户应创建：

```toml
api = "responses"
```

配置，并选择支持 Responses 的提供商。

仅支持 Chat Completions 的提供商将在本次变更后明确不再属于项目支持范围。

---

## 交付计划

1. 删除 Chat Completions，并明确实现配置拒绝逻辑。
2. 在新增能力字段之前，先保留并扩展 Responses 与 Messages 的 Wire Fixture。
3. 引入带保守默认值的 Provider Capability 对象。
4. 实现以下 Provider Profile：
   - OpenAI
   - Anthropic
   - DeepSeek Responses
   - DeepSeek Messages
5. 标准化基于 Capability 控制的推理流（Reasoning Stream）与用量统计（Usage Details）。
6. 在完成 Prompt Cache 测量机制后再引入缓存控制功能。
7. 未来新增厂商时，必须同时提供：
   - Profile 文档
   - 请求 Fixture
   - 正常流式 Fixture
   - 失败/不支持功能 Fixture

---

## 验收标准

- 不再保留任何 Chat Completions 代码路径或配置项。
- Responses 与 Messages 的完整工具调用往返（tool round trip）测试持续通过。
- Provider 特有的可选字段仅在 Capability 声明支持时发送。
- 不支持的字段必须在本地被拒绝或省略，不能因为某些提供商会静默忽略它们就默认发送。
- 为 DeepSeek Responses 的推理文本增量事件和结束事件提供对应 Fixture。
- 切换提供商时，绝不会将某个厂商私有的推理数据回放给其他 Provider Profile。

---

## 参考资料

- OpenAI Responses 迁移指南  
  https://developers.openai.com/api/docs/guides/migrate-to-responses

- Anthropic Messages API  
  https://docs.anthropic.com/en/api/messages

- DeepSeek Responses API 兼容性说明  
  https://api-docs.deepseek.com/zh-cn/guides/responses_api

- DeepSeek Anthropic API 兼容性说明  
  https://api-docs.deepseek.com/zh-cn/guides/

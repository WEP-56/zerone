# Prompt Cache 设计

## 决策

Onemore 将 Prompt Cache（提示词缓存）视为一种由提供商在服务端实现的优化机制。客户端不存储或回放模型的 KV 张量。

客户端的职责是：

- 构造一个足够长且具备确定性的提示词前缀；
- 仅在提供商支持时发送该提供商专用的缓存控制参数；
- 记录提供商返回的缓存用量数据。

主要目标是在不改变模型可见语义的前提下，降低输入 Token 成本和预填充（prefill）延迟。

缓存未命中是可以接受的；但为了缓存而改写提示词，导致对话语义或工具协议发生变化，则不可接受。

## 模型

Prompt Cache 重用的是精确 Token 前缀对应的 Attention KV 状态，而不是语义缓存。

如果提示词前部发生变化，其后的所有 Token 都无法复用缓存。

```text
稳定前缀
  系统指令
  稳定的工作区策略和工具声明
  仅追加的对话历史

变化后缀
  最新的用户输入
  最新的工具执行结果
  引导信息和后续输入
```

提示词的内容和顺序必须保持稳定。

不要在稳定前缀中加入以下动态信息：

- 时间戳；
- 随机标识符；
- 用量计数器；
- 易变的 Git 状态；
- 动态排序的工具声明。

## 当前基线

当前 Session 模型已经比较有利于实现提示词前缀复用：

- Session Facts 采用仅追加方式存储，并按顺序投影为模型消息；
- 正常的一轮交互会在尾部追加新的用户消息、助手消息和工具结果消息；
- 内置工具按照确定性顺序注册；
- 系统提示词和工作区上下文会在对话历史之前完成组装。

以下操作会有意创建新的缓存边界：

- 切换提供商或模型；
- 修改系统提示词或工具 Schema；
- 执行手动压缩，即使用新的摘要替换模型当前看到的上下文；
- 为满足上下文预算而缩短旧的工具结果。

即使这些操作会导致缓存未命中，也必须确保其行为正确。

Reasoning effort 使用两套彼此分离的标识：完整诊断 Fingerprint 包含解析后的
effort 策略，使 `Omit`、`medium`、`none`、`high` 可审计地区分；稳定的 OpenAI
Prompt Family Cache Key 不包含 effort，因为 effort 改变生成行为，不改变可复用的
输入 Token 前缀。加密 reasoning 回放也不受“是否发送 effort 字段”影响。

## Provider 策略

不能根据 Endpoint 名称推断提供商是否支持缓存。

- **OpenAI Responses**：符合条件的模型可使用自动缓存。部分较新的模型还支持 Cache Key 和显式缓存断点；
- **Anthropic Messages**：是否支持显式 Cache-Control Block 取决于提供商和具体模型；
- **DeepSeek Responses**：Context Cache 自动生效，并会报告已缓存的输入 Token，但不支持 Prompt Cache Key 和缓存保留控制；
- **DeepSeek Anthropic 兼容接口**：`cache_control` 会被忽略，因此不能将 Anthropic 格式的请求视为一次显式缓存写入。

所有缓存参数都必须受 Provider Capabilities 控制。

不能仅仅因为某个兼容提供商会静默忽略不受支持的参数，就将这些参数发送出去。

## 测量指标

第一阶段应优先实现可观测性，而不是立即加入缓存控制字段。

当提供商返回以下缓存用量数据时，应完整保留：

```rust
pub struct CacheUsage {
    pub read_tokens: u64,
    pub write_tokens: u64,
}
```

`Usage` 除了保留常规的输入和输出 Token 数量，还应保留可选的缓存用量数据。

随后，UI 和 Session Facts 可以报告：

- 输入 Token 数；
- 缓存输入 Token 数；
- 缓存写入 Token 数；
- 缓存读取比例，即 `read_tokens / input_tokens`；
- 在已配置模型价格的情况下计算出的有效输入成本。

不要为了测量缓存效果而持久化完整请求体。

应当持久化以下内容：

- Usage 数据；
- 不包含秘密信息的 Prompt Fingerprint。

## Prompt Fingerprint

在加入提供商缓存控制之前，应基于提供商渲染后的语义提示词构建确定性的 Fingerprint：

```text
提供商协议族
模型
系统提示词版本
工具 Schema 摘要
稳定的工作区上下文版本
投影后的消息前缀摘要
```

Fingerprint 属于诊断数据，不能替代提供商自身的缓存匹配机制。

它应当能够在不暴露用户内容的前提下，识别为什么连续两次请求不能共享同一个前缀。

正常情况下，新一轮对话应当扩展上一轮的 Fingerprint，而不是重写它。

## Cache Key 与断点

当提供商支持 Cache Key 时，Cache Key 应标识一个提示词族，而不是某一轮对话。

Cache Key 中不得包含：

- 消息序列号；
- 时间戳；
- 随机 UUID；
- Session ID。

合适的 Key 结构如下：

```text
onemore:v1:<provider>:<model>:<workspace-policy>:<system>:<toolset>
```

显式缓存断点应放在同时满足以下条件的内容之后：

- 内容规模较大；
- 预期会被重复使用。

只有当 Provider Capability 明确声明支持时，显式缓存断点才有效。

缓存写入成本必须能够通过后续的缓存读取得到摊销。不要将体积很大但只会使用一次的工具结果标记为缓存写入候选项。

## 交付计划

当前实现状态：

- 已解析、累计并持久化 OpenAI Responses、DeepSeek Responses 与 Anthropic Messages 的缓存读写 Token，并在 CLI/TUI 中展示；
- 已通过 Provider Profile 控制厂商私有字段，DeepSeek 不会收到 OpenAI Cache Key 或加密 Reasoning 请求；
- 工具声明按名称稳定排序，Prompt Fingerprint 随 Assistant 事实持久化，OpenAI 请求使用稳定的 Prompt Family Cache Key；
- 显式 OpenAI Breakpoint 与 Anthropic Cache 写入仍保持关闭，直到加入模型级能力与写入成本策略。

1. 解析并持久化提供商返回的缓存用量数据，同时为每个受支持的 Provider Profile 添加 Fixture 测试。
2. 添加确定性的 Prompt Fingerprint，并提供说明前缀发生变化原因的提示信息。
3. 保持现有提示词布局稳定，移除意外加入的动态字段，并明确工具声明的排序规则。
4. 在已配置模型支持的前提下，加入受 Provider Capability 控制的 OpenAI Cache Key 和缓存断点。
5. 加入受 Capability 控制的 Anthropic Cache-Control。
6. 根据真实 Usage 数据，判断显式缓存写入是否能够降低特定模型和工作负载的成本。

## 验收标准

- 不返回缓存用量数据的提供商，其行为与以前完全一致；
- 永远不发送提供商不支持的缓存字段；
- 提示词前缀与生成设置都相同的请求，必须生成相同的本地 Fingerprint；
- 只改变 reasoning effort 时，完整 Fingerprint 应变化，稳定 Prompt Family Cache Key 不应变化；
- 缓存未命中不能改变消息、工具调用、权限或 Session Facts；
- Wire Fixture 必须验证缓存用量解析以及提供商专用的请求结构。

## 参考资料

- [OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching)
- [DeepSeek Responses API 兼容性说明](https://api-docs.deepseek.com/zh-cn/guides/responses_api)
- [DeepSeek Anthropic API 兼容性说明](https://api-docs.deepseek.com/zh-cn/guides/anthropic_api)

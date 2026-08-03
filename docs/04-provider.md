# 04 · Provider 层:三种 API 的统一

对应源码:`src/provider/mod.rs`(契约与共用件)、`sse.rs`、
`anthropic.rs`、`openai_chat.rs`、`openai_responses.rs`、`src/message.rs`。

这是 harness 里工程量最大、也最"值钱"的一层:它把三种互不兼容的
LLM API 压成一个 trait,使 Agent Loop、上下文、TUI 对厂商完全无感,
也使 `/provider` 热切换后历史不丢。

## 契约:Provider trait

```rust
fn stream_turn(&self, prompt, tools, on_event, cancel)
    -> Result<Option<TurnOutput>, ProviderError>;
```

语义逐条读:

- 输入是统一的 `PromptContext`(system 片段 + `ChatMessage` 历史)与
  `ToolSpec` 列表;适配器负责**编码**成自家请求体;
- 流式过程中把增量翻成统一的 `ProviderEvent`(TextDelta / ThinkingDelta /
  ToolCallBegun)回调给上层;
- 每个 SSE 事件之间检查 `cancel`,置位则返回 `Ok(None)`(取消不是错误);
- 正常结束返回 `TurnOutput { message, usage, stop }`——**解码**回统一模型;
- `ProviderError.retryable` 标记 408/429/5xx/网络错误,重试与否由
  Runtime 决定(见 02)。

**新增 Provider 不需要改 Agent Loop**:实现 trait,然后在
`build_provider()` 的 match 里加一行(教程见 07)。

## 统一消息模型(message.rs)

形状照着 Anthropic 的 content blocks 设计——三者中它对"一条消息内
混合文本与多次工具调用"表达得最干净,适合当中间表示:

```rust
ChatMessage { role: User|Assistant, blocks: Vec<Block> }
Block::Text(String)
Block::Thinking { text, provider_kind, raw }   // 思考流;raw 见下文 Responses
Block::ToolUse { id, name, input }             // 模型发起调用
Block::ToolResult { tool_use_id, content, is_error } // Observation(在 User 消息里)
```

注意**没有** System/Tool 角色:系统提示由 context 层管理、在编码时落位;
工具结果统一放 User 消息的 ToolResult 块,由适配器按需拆分。

## 三接口对照表(本层的核心知识)

| 概念 | Messages(Anthropic) | Chat Completions | Responses(OpenAI) |
|---|---|---|---|
| 端点 | `/v1/messages` | `/v1/chat/completions` | `/v1/responses` |
| 鉴权头 | `x-api-key` + `anthropic-version` | `Authorization: Bearer` | `Authorization: Bearer` |
| 系统提示 | 顶层 `system` 字段 | `messages[0]` role=system | 顶层 `instructions` 字段 |
| 历史形态 | 消息列表(严格 user/assistant 交替) | 消息列表(role 四种) | **item 列表**(消息/调用/结果/推理并列) |
| 助手文本 | `content[]` 里的 text 块 | `content` 字符串 | message item 的 `output_text` part |
| 工具声明 | `{name, description, input_schema}` | `{type:"function", function:{name,…,parameters}}` | `{type:"function", name,…,parameters}` **平铺** |
| 工具调用 | `tool_use` 块(id) | `tool_calls[]`(id,arguments 是**字符串**) | `function_call` item(call_id,arguments 字符串) |
| 工具结果 | user 消息里 `tool_result` 块(有 is_error) | 独立 `role:"tool"` 消息(无错误标记,用 `ERROR:` 前缀) | `function_call_output` item(同左) |
| 工具参数流式 | `input_json_delta.partial_json` 碎片 | `tool_calls[].function.arguments` 按 index 碎片 | `function_call_arguments.delta` |
| max tokens | `max_tokens` **必填** | `max_tokens`(新模型改名 `max_completion_tokens`,本项目不配则不发) | `max_output_tokens` 可选 |
| 用量 | `message_start` + `message_delta` | 末块 usage(**须请求** `stream_options.include_usage`,且该块 `choices` 为空数组) | `response.completed.usage` |
| 停止原因 | `stop_reason`: end_turn/tool_use/max_tokens | `finish_reason`: stop/tool_calls/length | `status` + `incomplete_details` |
| 流结束标志 | `message_stop` 事件 | `data: [DONE]` | `response.completed` 事件 |
| 思考流 | `thinking_delta`(需开 extended thinking,本项目未开) | 非标字段 `reasoning_content`(DeepSeek R1) | `reasoning` item + summary delta |

## SSE:一个解析器服务三家

三家的流都是标准 Server-Sent Events,差异只在事件内容,所以解析器
只有一个(`sse.rs`,几十行):按行读、`data:`/`event:` 字段、空行分帧、
`:` 注释(keep-alive)忽略、宽容非 UTF-8。适配器拿到
`SseEvent{event, data}` 后各自 `match data.type`。

统一的防御姿势(三个适配器一致):

- 未知事件/未知块类型一律忽略——厂商加新事件不至于弄崩你;
- 脏 JSON 行跳过而不是断流;
- 兼容 `[DONE]`(哪怕该 API 规范里没有);
- 流意外掐断时,把拼到一半的内容按序收编,不丢已收数据。

## 各适配器的坑清单(读源码时对照)

### anthropic.rs
- 消息必须 user/assistant **严格交替**且 content 非空 → 编码时合并相邻
  同角色消息(工具结果消息 + 用户新输入常常相邻)、跳过空消息;
- `max_tokens` 必填,不配则用默认 8192;
- 工具参数从 `partial_json` 碎片拼出,空字符串按 `{}` 解析(无参工具);
- `message_delta.usage.output_tokens` 是**累计值**,直接覆盖。

### openai_chat.rs(兼容面最广,方言也最多)
- 工具调用按 `index` 分片,id/name 只在首片。`index` 只能当分片关联键:
  兼容服务可能从 1 开始或产生稀疏编号,不能按数组下标补占位调用;
  个别服务不发 index(按"有新 id = 新调用"兜底)、不发 id
  (必须**造一个**,否则结果无法配对);
- 缺少 `function.name` 的调用必须在进入 Runtime 前拒绝。请求编码还会跳过
  旧版本留下的无效 ToolUse/ToolResult 对,使已经污染的会话可以继续使用;
- 用量块 `choices` 是空数组——任何 `choices[0]` 的无脑索引都会 panic;
- 纯工具调用的 assistant 消息 `content` 置 null;
- `role:"tool"` 消息必须**紧跟**发起调用的 assistant 消息 → 编码 User
  消息时先输出 ToolResult 再输出用户文本;
- DeepSeek 的 `reasoning_content` 顺手收进 ThinkingDelta,但**不回传**
  (DeepSeek 明确要求)。

### openai_responses.rs(概念差异最大)
- 不是消息列表而是 **item 列表**;
- 本项目以无状态方式使用(`store:false`,每轮全量带历史),与另两家
  心智一致。代价:推理模型的 **reasoning item 必须原样回传**,否则下轮
  带 function_call 的请求直接 400。做法:请求
  `include:["reasoning.encrypted_content"]`,把整个 item 存进
  `Block::Thinking.raw`,编码时原样吐回,且必须**排在对应 function_call
  之前**(顺序天然保持,因为按 item 完成顺序入 blocks);
- `provider_kind` 标记 raw 的归属——切到别家 provider 后,这些 item
  会被安全丢弃而不是误发;
- 工具声明是平铺的(没有 `function` 包一层),与 Chat 不同,极易写混;
- 流事件几十种,只消费必要子集,以 `output_item.done` 里的完整 item
  为权威定稿。

## 调试手段(按成本从低到高)

1. `cargo test`:每个适配器都有**请求体形状回归测试**(不碰网络),
   改编码逻辑先跑它;
2. `cargo test --test wire`:mock SSE 服务器过完整工具回路,
   校验双向翻译(模板见 07);
3. `cargo run -- --once 你好`:真 API 连通性,错误带 HTTP 状态码与
   服务端原话;
4. curl 看裸流:`curl -N ... -d '{"stream":true,…}'`,对照适配器的 match。

## 扩展指引

- **Anthropic prompt caching**:编码时给 system/最后一条消息加
  `cache_control` 即可,入口在 `anthropic.rs::build_body`;
- **开启 extended thinking**:请求加 `thinking` 字段;回传 thinking 块
  需要带签名,`Block::Thinking.raw` 的机制已为此预留;
- **非流式模式**:`stream_turn` 旁加一个 `complete_turn` 默认实现
  (内部收集流事件),对不支持 SSE 的网关有用;
- **每 profile 自定义 header**(Azure、企业网关):在
  `ProviderSettings` 加 `extra_headers`,`post_sse` 已接受 header 列表。

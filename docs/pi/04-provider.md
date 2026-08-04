# 04 · Pi 的 Provider 层:从三份适配器到协议平台

> 对应 Zerone 基线:[04 · Provider 层](../04-provider.md)。
> Pi 重点源码:`packages/ai/src/types.ts`、`models.ts`、`api/`、`utils/`。

## Zerone 当前实现

Zerone 用 `Provider` trait 把三种 API 压到同一边界:

```rust
fn stream_turn(
    prompt: &PromptContext,
    tools: &[ToolSpec],
    on_event: &mut dyn FnMut(ProviderEvent),
    cancel: &AtomicBool,
) -> Result<Option<TurnOutput>, ProviderError>;
```

Messages、Chat Completions、Responses 各自负责请求编码和 SSE 解码;共同使用
`post_sse`、`SseReader`、错误分类与参数解析。Runtime 只认识 Text/Thinking/ToolCall
增量和最终 `TurnOutput`。

这已经实现了最重要的隔离。Pi 在此基础上继续解决四类生产问题:

- 不同模型的能力、费用、窗口和兼容方言怎样数据驱动;
- setup、首包、增量、正常结束和失败怎样组成终止完备协议;
- SDK 自带重试、请求重试和整轮重试怎样不重复;
- 跨 Provider 历史中的 thinking、图片和 tool call ID 怎样合法回放。

## Model 是路由与能力数据,不只是名字

Pi 的 `Model<TApi>` 包含:

- `api/provider/baseUrl/id`;
- 是否 reasoning、thinking level 映射;
- 输入支持 text/image;
- context window 与 max tokens;
- 输入、输出、cache read/write 费率和长上下文阶梯价;
- 默认 sampling 参数;
- API 特定 `compat` 配置。

`createProvider` 接收模型列表和一份 API 实现或 `api → implementation` map。调用时按
`model.api` 选择 `ProviderStreams`;Agent Loop 始终只看到一个 `StreamFn`。

```text
Agent StreamFn
    → Provider.streamSimple(model,...)
        → dispatch model.api
            → anthropic-messages.streamSimple
            → openai-completions.streamSimple
            → openai-responses.streamSimple
```

Zerone 当前 `ProviderSettings.api` 在构造时选择 trait object,切模型只是修改字符串。
当开始支持模型能力差异时,应引入只读 `ModelProfile`,不要把 `if model.contains(...)`
散进 Runtime。

## 统一契约是事件流,不是 callback + 可选结果

Pi 的 `AssistantMessageEventStream` 同时是:

- `AsyncIterable<AssistantMessageEvent>`;
- 可 `result()` 得到 final `AssistantMessage` 的 Future;
- 只在 `done` 或 `error` 事件上完成的终止协议。

事件序列为:

```text
start
  text_start / text_delta* / text_end
  thinking_start / thinking_delta* / thinking_end
  toolcall_start / toolcall_delta* / toolcall_end
done | error
```

终止事件携带完整 final message:

- `done.reason`:仅 `stop | length | toolUse`;
- `error.reason`:仅 `error | aborted`。

相比 Zerone 的 `Result<Option<TurnOutput>, ProviderError>`,这个设计让 setup 失败、流中
失败和 abort 都以同一可消费形状收尾。调用者不需要同时处理 callback 已吐出多少、
Result 是 Err 还是 None、有没有 final message 三套状态。

## lazyStream:异步初始化也不能破坏同步 API

API 模块可能要动态 import、刷新认证或做异步 setup,但 `stream()` 仍需立即返回一个
事件流。[lazy.ts](../../example/pi/packages/ai/src/api/lazy.ts) 创建 outer stream,
异步取得 inner 后转发全部事件;setup reject 时构造零 usage 的 error
`AssistantMessage`,发 `error` 并结束。

这条原则比动态 import 本身更重要:

> 任何发生在“调用 stream”之后的预期失败,都必须进入 stream 协议,不能让调用方因
> 失败时机不同而写两套恢复逻辑。

## 一次请求的标准管线

三个 Pi API 模块都遵循近似顺序:

```text
streamSimple
  → buildBaseOptions
      · 根据上下文窗口收紧 maxTokens
      · 合并 sampling / auth / timeout / cache / transport
      · clamp thinking level
  → stream
      · 创建 pending AssistantMessage
      · 解析 API key / compat / cache session
      · 构造 client 与请求体
      · onPayload 观测或替换请求
      · retryProviderRequest(只包请求建立)
      · onResponse 观测状态与 headers
      · emit start
      · 消费上游流并生成 typed deltas
      · 校验终止条件
      · emit done
  catch
      · 删除流式临时字段
      · normalize/format error
      · emit error
```

`onPayload`、`onResponse` 是观测、测试、审计和网关定制的稳定挂点。Zerone 现在要看
请求体只能在 adapter 内打日志;迁移 Hook 后应注意 secret 脱敏,并明确 hook 是否允许
改写 payload。

## 消息模型为跨 Provider 回放保留足够信息

Pi 的统一消息比 Zerone 更丰富:

- assistant 记录 `api/provider/model/responseModel/responseId`;
- text 可带 provider text signature;
- thinking 可带签名或 redacted opaque payload;
- tool call 可带 provider thought signature;
- usage 区分 input/output/cache read/cache write/reasoning 和费用;
- stop 同时保存归一化 `stopReason` 与可选 `rawStopReason`。

这不是为了让上层理解厂商字段,而是为了**同模型续聊时能原样回放,换模型时能安全
降级**。

[transform-messages.ts](../../example/pi/packages/ai/src/api/transform-messages.ts) 在进入
具体 adapter 前做:

1. 非视觉模型把图片替换成明确 placeholder;
2. 同模型保留有效 thinking signature,跨模型丢 opaque/redacted 数据或降成文本;
3. 跨 Provider 规范化超长/非法 tool call ID,并同步修改 ToolResult ID;
4. 为孤立 tool calls 合成错误结果;
5. 过滤 `error/aborted` assistant,避免回放半成品。

Zerone 已用 `provider_kind + raw` 处理 Responses reasoning,方向正确。下一步应把
“是否可回放”的判断从每个 adapter 中抽成统一历史规范化步骤。

## OpenAI 兼容不是一个布尔值

Pi 的 Chat Completions adapter 很长,主要不是因为 Loop 复杂,而是兼容方言多。
`OpenAICompletionsCompat` 表达:

- 是否支持 store、developer role、usage streaming、strict tools;
- token 上限字段名;
- reasoning 字段与格式;
- cache control 与 session affinity;
- finish reason 是否可靠;
- tool stream、deferred tools、chat template 参数等。

请求构建只读 compat,而不是到处按 base URL 写分支;URL 检测只是默认值,模型配置可
显式覆盖。

对 Zerone 来说,不必一次实现完整矩阵。先把现有 `openai_chat.rs` 中每个方言分支列成
`ChatCompat` 字段,测试默认 profile 与显式 override。未来接内部网关时就不需要再
复制整个 adapter。

## 三种 Adapter 各自守住哪些坑

### Anthropic Messages

- system、content blocks、tool result 和 thinking 按 Messages 语义编码;
- thinking budget 与 max tokens 协调;
- prompt cache retention 和 cache usage 分项;
- eager tool input streaming 与旧 beta header 兼容;
- 未知 stop reason 不静默当成功。

### OpenAI Chat Completions

- tool call 参数按 index/id 拼接,结束时清掉 `partialArgs` 等 scratch 字段;
- reasoning details 可能先于对应 tool call 到达,需要暂存再配对;
- Provider 不给 finish reason 时,只有 compat 明确允许才能从内容推断;
- unknown/content_filter/network finish reason 变成可解释 error;
- raw stop reason 保留用于诊断。

### OpenAI Responses

- output item 按 index 建 slot,文本、reasoning 与 function call 可交错;
- terminal `completed/incomplete/failed` 决定最终状态,不能只看中途 final-answer 事件;
- 流结束但没有 terminal event 必须报错;
- `incomplete` 的 token limit 映射为 `length`,content filter 映射非重试 error;
- `partialJson`、custom input 等解析 scratch 永不进入 session;
- `store=false` 时需要保留 encrypted reasoning 以支持多轮工具调用。

这些专项行为都有独立测试,而不是只靠一个端到端 mock。

## 两层重试必须分清

Pi 保留了两套不同重试工具。

### 1. Provider request retry

[provider-retry.ts](../../example/pi/packages/ai/src/utils/provider-retry.ts) 只包“建立
SDK 请求/取得响应流”:

- SDK 自带 retry 显式设为 0,避免双重重试;
- 408/409/429/5xx、无 status 传输错误可重试;
- `x-should-retry` 可显式覆盖;
- 读取 `retry-after-ms`、`retry-after`,否则指数退避加 jitter;
- 默认拒绝服务器要求超过 60 秒的等待;
- sleep 可被 AbortSignal 立即打断;
- 流已经建立后的中途失败不在此层偷偷重播。

### 2. Assistant-turn retry

[retry.ts](../../example/pi/packages/ai/src/utils/retry.ts) 对最终
`AssistantMessage{stopReason:"error"}` 分类,使用有界指数退避重做整次 assistant call,
并提供 retry 生命周期回调。它把 quota/billing/usage limit 明确排除,对 overloaded、
网络、server error 和已知 premature stream 文案才重试。

在当前教学切片里,这套 utility **没有直接接进低层 Agent Loop**;它是上层可选择的
策略。文档不能把“存在 helper”写成“每个 Agent 默认都会整轮重试”。

Zerone 当前“只有零增量时 Runtime 才重试”更保守,建议继续作为默认。若以后增加整轮
重试,必须明确 UI 是否清除旧 partial、历史从哪个合法点重开、费用怎样统计。

## 错误保真:不要只读 `error.message`

不同 SDK 把 HTTP 信息放在不同字段。Pi 的
[error-body.ts](../../example/pi/packages/ai/src/utils/error-body.ts) 会:

- 从 `statusCode/status/$metadata/$response` 取状态;
- 从 string body、OpenAI parsed error 或 Bedrock response body 取原因;
- 忽略 stream 和 class instance,避免把 SDK 内部对象序列化成噪声;
- 检测 message 是否已包含 body,避免重复;
- 将 provider body 截到 4000 字符;
- 对非 Error throw 做安全 stringify。

这能把“403 status code (no body)”恢复成网关实际返回的 WAF/权限原因。Zerone 当前
`extract_api_error` 覆盖常见 JSON 形状,但错误截到 400 字符且无结构字段。可以先迁移
status/body/message 三段式规范化,再决定显示长度。

## Token 与费用也是协议数据

`buildBaseOptions` 先用上次真实 usage 加尾部估算,计算当前 context token,再从模型窗口
扣 4096 safety tokens,收紧 max output。`calculateCost` 同时考虑 cache 和长上下文费率。

这比固定 `max_tokens` 实用,但估算只是护栏,不是 compaction。可用窗口小于 1 时 Pi
仍至少发 1 token;可靠产品还应在请求前触发压缩或给出明确 context overflow 状态。

## Zerone 推荐迁移顺序

1. Provider 流升级为 `Start/BlockDelta/Done/Error`,final message 总可取得;
2. 丰富 `Usage` 和 `ModelProfile`,先记录 context window/input modalities;
3. 引入共享 message normalization,处理跨模型 thinking 和孤立工具结果;
4. 将 adapter 流式 scratch 与持久消息类型分开;
5. 抽出 `ChatCompat/ResponsesCompat`,以配置覆盖 URL 推断;
6. 关闭底层 HTTP 库隐式 retry,只留一套可取消 request retry;
7. 规范化错误 status/body/message,并添加 body 大小上限;
8. 增加 payload/response 观测 Hook,默认脱敏;
9. 最后才考虑 lazy API 模块与 assistant-turn retry。

## Provider 验收

- setup、请求、首包后断流、abort 都产生唯一终止事件;
- `result()` 在所有结束路径都有 final message;
- length 截断不会执行工具;
- 三种 API 的 tool call/result ID 始终配对;
- 跨 Provider 切换不会回放不兼容 opaque thinking;
- SDK retry 与 harness retry 不叠加;
- Retry-After 可取消且有最大等待;
- malformed/unknown terminal reason 不静默成功;
- 流式临时字段不会落进 Session;
- 错误 body 有用、受限、无重复、无 secret。


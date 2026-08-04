# 07 · 实战:按 Pi 的标准给 Zerone 添加 Provider

> 对应 Zerone 基线:[07 · 添加一个新 Provider](../07-how-to-add-provider.md)。
> 本篇沿用“接入一个原生新协议”的场景,重点补齐生产级边界。

## Zerone 当前接入路径

Zerone 新增原生协议时,先在 `ApiKind` 加变体,实现一份 `Provider` adapter,然后在
`build_provider` 注册。adapter 将统一 `ChatMessage/Block` 编成请求体,再把 SSE
解成 `ProviderEvent + TurnOutput`. HTTP、SSE、错误分类、URL 和工具参数解析已有共享
helper,测试以纯请求形状和本地 mock wire 为主。

这个边界已经保证新增 Provider 不改 Agent Loop。本篇在其上补充模型能力数据、
终止完备事件、流式 scratch、兼容 profile、唯一重试层与专项协议测试。

## 第 0 步:先判断你要加的是 Provider、API 还是兼容 Profile

Pi 明确区分:

| 概念 | 例子 | 改动 |
|---|---|---|
| Provider/service | 公司网关、OpenAI、某聚合服务 | 模型列表、base URL、认证与 API 映射 |
| API protocol | anthropic-messages、openai-responses | 新 `ProviderStreams` 实现 |
| Compatibility profile | 某 OpenAI-compatible 方言 | 模型 `compat` 数据,复用原 adapter |

Zerone 现有文档也指出:只改 `base_url` 就能接的 OpenAI-compatible 服务不该复制
adapter。工程化后的判断应更严格:

- 端点和事件语义相同,只是字段支持不同 → compat profile;
- 同一服务有多个 API → provider 按 `model.api` dispatch;
- 请求/流事件/工具协议本质不同 → 新 API module。

这一步判断错了,后面会得到几十个只有三行差异的 Provider 实现。

## Pi 的接入面

每个 Pi API module 对外只有统一形状:

```ts
stream(model, context, apiSpecificOptions): AssistantMessageEventStream
streamSimple(model, context, SimpleStreamOptions): AssistantMessageEventStream
```

三份 `*.lazy.ts` 再把模块包装为 `ProviderStreams`;`createProvider` 接收一个实现或
`api → implementation` map,按 `model.api` 选择。

映射到 Zerone,推荐把当前 `Provider` trait 拆成:

```text
ProviderProfile   # 服务、认证、模型与 compat 数据
ApiAdapter        # 请求/流协议
ModelProfile      # 单模型能力、窗口、费用
```

第一版可继续用 trait object,不需要真的分 crate。

## 第 1 步:先抓协议 fixture,再写代码

至少保存这些脱敏 fixture:

1. 纯文本成功;
2. thinking + text;
3. 单工具调用,参数分多 chunk;
4. 同一消息多个工具调用且事件交错;
5. usage 与正常 terminal;
6. output token 截断;
7. content filter/refusal;
8. HTTP 4xx/429/5xx 错误体;
9. 首包前断开;
10. 首包后无 terminal 断开;
11. 用户 abort。

用 fixture 先画状态表:

| 上游事件 | 前置状态 | 动作 | 发出的统一事件 |
|---|---|---|---|
| response.start | pending | 建立 response id | `start` |
| text.delta | text slot exists | append | `text_delta` |
| function.args.delta | tool slot exists | append scratch | `toolcall_delta` |
| terminal.completed | 所有 slot closed | finalize usage/stop | `done` |
| stream EOF | 无 terminal | protocol error | `error` |

Pi 的 Responses 专项测试证明:不能看见一个“final answer”中间事件就提前当成功,真正
终止原因可能随后变成 incomplete/content filter。

## 第 2 步:定义 Model 与 API-specific options

新协议先回答:

- 模型支持 text/image 哪些输入;
- context window、max output;
- thinking 是等级、token budget 还是不支持;
- cache retention 与 session affinity;
- usage 能否区分 cache/reasoning;
- tool schema/strict/custom grammar 能力;
- stop reasons 完整集合;
- transport、timeout、header 和 retry 选项。

Pi 的 `streamSimple` 将跨 API 的 `SimpleStreamOptions` 映射为具体 options;复杂调用仍可
直接用 `stream` 暴露协议特有字段。Zerone 也应避免两种极端:

- 把所有 Provider 特有字段塞进全局 config struct;
- Provider trait 只允许最小公共字段,导致高级能力只能硬编码。

可用公共 `RequestOptions` + adapter 自有配置,模型能力由 `ModelProfile` 数据驱动。

## 第 3 步:先写纯请求转换

将请求构建拆成可直接单测的纯函数:

```text
normalize history for target model
  → encode system/messages/tools
  → apply compat
  → apply cache/thinking/token options
  → apply caller sampling overrides last
```

最后一条顺序要明确。Pi 的 OpenAI adapters 把 `samplingParams` 最后 `Object.assign`,
允许调用者覆盖命名字段;这很灵活,也有风险。Zerone 若采用相同策略,必须限制不能覆盖
`stream/model/messages/tools` 等结构字段,或在文档中明确责任。

为纯转换写 shape tests:

- system 落位;
- text/image/thinking;
- ToolUse/ToolResult ID;
- tool schema 与 strict;
- 空内容;
- 跨 Provider 历史;
- max token 与 thinking budget;
- compat 每个 override。

不要通过真实网络验证 JSON 形状。

## 第 4 步:使用可丢弃的流式 scratch state

Adapter 解析流时通常需要临时字段:

```text
partialArgs / partialJson
provider output index
custom input buffer
pending reasoning detail
stream item id
```

Pi 将这些字段放在流式 block 的临时扩展上,finalize/catch 时统一删除,保证不会进入
Session。Zerone 应从类型上区分:

```rust
struct StreamingAssistant { slots: Vec<StreamingSlot>, ... }
struct ChatMessage { blocks: Vec<Block>, ... }
```

只允许 `StreamingAssistant::finalize()` 产生可持久 `ChatMessage`. 这样新增 scratch 字段
不会被 `serde` 顺手存盘。

### Tool call 参数不要只用一个全局 String

多调用可能交错。按 provider 的 index/id 建 slot:

```text
Map<OutputIndex, ToolSlot { id, name, partial_args }>
```

每个 delta 更新对应 slot;结束时解析。解析失败可以保留 raw 让工具返回 actionable error,
但 `length` 终止时整条 assistant 内的工具都不能执行。

## 第 5 步:实现终止完备的 Stream

标准路径:

```text
创建 pending final message
try:
    完成 setup / request
    emit Start
    for upstream event:
        检查 abort
        更新 scratch
        emit typed delta
    验证看到了合法 terminal
    finalize scratch → durable message
    emit Done(final)
catch:
    清除 scratch
    normalize error
    final.stop = Aborted | Error
    emit Error(final)
```

规则:

- `Done` 与 `Error` 必有其一且只有一个;
- final result 在两者上都可读取;
- EOF 不是成功证据;
- unknown stop reason 默认 error,除非 compat 明确允许推断;
- abort 与网络错误分开;
- partial 可展示,不可直接写事实历史。

Zerone 目前取消返回 `Ok(None)`,错误返回 `Err`,正常返回 `Some`. 迁移时可以先保留同步
函数签名,但把内部结果统一为:

```rust
enum StreamTerminal {
    Done(TurnOutput),
    Error(FailedTurn),
    Aborted(FailedTurn),
}
```

ProviderEvent 增加 Start/Done/Error,Runtime 不再根据 Result 形状补协议。

## 第 6 步:复用一套可取消 request retry

Pi 明确关闭 OpenAI/Anthropic SDK retry,使用 `retryProviderRequest` 统一:

- status/header 分类;
- Retry-After;
- 最大等待;
- jitter;
- AbortSignal。

新 adapter 必须遵守同一策略,不能同时打开 SDK retry。request retry 只包取得响应流,
不包已经产生增量的整个消费过程。

若新协议 SDK 的 error shape 不同,扩展共享 `normalizeProviderError`,不要在 catch 中只写:

```text
error.to_string()
```

新增一份合成 SDK error 单测,证明 status/body/message 被保留且 body 有大小上限。

## 第 7 步:提供观测 Hook,但不泄漏 secret

Pi 的 API options 支持:

- `onPayload`:发送前查看或替换 payload;
- `onResponse`:body 消费前读取 status/headers;
- custom headers/fetch/timeout/transport/sessionId。

Zerone 可先做只读 Hook:

```rust
trait ProviderObserver {
    fn request_built(&self, redacted: &Value);
    fn response_started(&self, status: u16, headers: &SafeHeaders);
    fn stream_finished(&self, terminal: &StreamTerminal);
}
```

API key、authorization、cookie、图片 base64、完整 prompt 默认不进日志。允许 Hook 改写
payload 会扩大测试状态空间,应晚于只读观测实现。

## 第 8 步:lazy wrapper 与注册

Pi 的 lazy wrapper 只有四行,因为所有 setup failure 已能变成 error stream。Rust 单体
暂时不需要动态加载 module;真正值得迁移的是失败契约。

注册分两层:

1. `ApiKind`/adapter factory 注册新协议;
2. Provider profile/model catalog 将目标模型的 `api` 指向它。

若一个 Provider 同时提供 Chat 和新原生 API,不要创建两个互不相关的 provider 名字;
让不同 model profile 选择不同 `api`,宿主仍通过一个 provider 管理模型列表。

当前 Pi 教学切片明确移除了完整认证和模型目录刷新,因此本篇不根据残留类型猜 OAuth、
keychain 或 catalog update 的具体实现。Zerone 新 Provider 的认证仍应遵循现有 config/
env 规则,secret 不写入 Session 和诊断日志。

## 第 9 步:测试分五层

### 1. Pure conversion tests

固定 `Context + ToolSpec + ModelProfile`,断言请求 JSON. 每个 compat 字段至少一例。

### 2. Stream parser tests

用录制 fixture 喂 parser,断言完整事件序列、content index、final message 和 scratch 清理。

### 3. Terminal/error tests

- 正常 terminal;
- length;
- refusal/filter;
- failed;
- unknown reason;
- EOF before terminal;
- abort before start / after deltas;
- malformed JSON chunk。

Pi 的 `openai-responses-terminal-event.test.ts`、
`openai-responses-partial-json-cleanup.test.ts` 和
`openai-completions-raw-stop-reason.test.ts` 是直接参照。

### 4. Retry/error normalization tests

- 429 + Retry-After;
- server explicit no-retry;
- 超过最大 delay;
- backoff 中 abort;
- SDK retry 确认关闭;
- opaque message + parsed body;
- 超长 body 截断。

### 5. Zerone wire tests

继续使用本地 mock server,完整验证:

```text
request shape → SSE chunks → AgentEvent → ToolUse → ToolResult → 第二次 request
```

真实网络只做人工 smoke,不作为确定性 CI。

## 兼容矩阵要成为数据与测试

新 API 若还有方言,为每个差异建 compat 字段:

| 能力 | Default | Override test |
|---|---:|---|
| finish reason reliable | true | false 时从内容推断 |
| strict tool schema | true | false 时移除 strict |
| usage in stream | true | false 时允许缺失 |
| system role | system | developer |
| max token field | max_output_tokens | alternative field |
| cache retention | none/short/long | unsupported 时降级 |

不要按 URL 在 parser 深处分支。URL 只可用于构造默认 profile,显式配置必须能覆盖。

## Zerone 的完成定义

- [ ] 已证明不是现有 adapter + compat 能解决;
- [ ] 请求转换是纯函数并有 shape tests;
- [ ] streaming scratch 不可持久化;
- [ ] 多 tool call 可按 index/id 正确交错;
- [ ] start 后必有唯一 done/error/aborted;
- [ ] EOF/unknown terminal 不静默成功;
- [ ] length 工具参数绝不执行;
- [ ] request retry 唯一、可取消、有最大 delay;
- [ ] status/body/raw stop reason 保真且受限;
- [ ] payload/日志完成 secret 脱敏;
- [ ] model capability/compat 数据驱动;
- [ ] parser、terminal、retry、wire 五层测试通过;
- [ ] Runtime、ToolRegistry、TUI 没有新增该 Provider 的名字分支。

Pi 在 Provider 方向最值得学习的不是支持的服务数量,而是把“某次调用到底发生了什么”
表达完整:请求怎样构造、流在哪个状态结束、失败是否可重试、哪些临时数据能落盘、换
模型后哪些历史还能回放。做到这些,新增 Provider 才不会降低整个 harness 的可靠性。

# 07 · 实战:添加一个新 Provider

三种内置接口已覆盖绝大多数服务(任何 OpenAI 兼容服务直接
`api = "chat"` + 改 `base_url` 就能用,**不需要**新写适配器)。
真正需要动这一层的场景:接一个自有协议的 API(如 Gemini 原生
generateContent)、接公司内部网关、或想给某家做深度定制
(缓存、thinking 等)。

## 全景:一个适配器只做两次翻译

```
统一模型 ChatMessage/Block ──编码──► 该 API 的请求体(JSON)
该 API 的 SSE 流 ──解码──► ProviderEvent 增量 + 最终 ChatMessage
```

写之前先用 curl 把该 API 的裸流抓下来看一遍,把"事件类型 → 处理"
列成表,再动手。三个内置适配器就是三份可抄的答案:
`anthropic.rs` 最简单(推荐做骨架),`openai_chat.rs` 展示怎么防方言,
`openai_responses.rs` 展示怎么处理厂商私有状态(reasoning 回传)。

## 步骤清单

### 1. 配置侧(config.rs)

```rust
// ApiKind 加变体 + parse() 加分支
pub enum ApiKind { Messages, Chat, Responses, Gemini }
"gemini" => Ok(ApiKind::Gemini),
// 需要的话给 ApiKind::default_key_env() 加默认环境变量名
```

### 2. 适配器骨架(src/provider/gemini.rs)

照抄这个结构(与三个内置适配器完全一致):

```rust
pub struct GeminiProvider { settings: ProviderSettings, agent: ureq::Agent }

impl GeminiProvider {
    pub fn new(settings: ProviderSettings) -> Self {
        GeminiProvider { settings, agent: http_agent() }   // 超时/代理已配好
    }

    /// 编码:统一模型 → 请求体。为它写"形状回归测试"(见第 4 步)。
    fn build_body(&self, prompt: &PromptContext, tools: &[ToolSpec]) -> Value {
        // 遍历 prompt.messages 的每个 Block,按该 API 的概念落位:
        //   Text / ToolUse / ToolResult / Thinking(不认识的丢弃)
        // + system 落位 + 工具声明翻译 + stream 开关
    }
}

impl Provider for GeminiProvider {
    fn label(&self) -> String { format!("{} / {}", self.settings.name, self.settings.model) }
    fn model(&self) -> &str { &self.settings.model }
    fn set_model(&mut self, model: String) { self.settings.model = model; }

    fn stream_turn(&self, prompt, tools, on_event, cancel)
        -> Result<Option<TurnOutput>, ProviderError>
    {
        let reader = post_sse(&self.agent, &url, &headers, &self.build_body(prompt, tools))?;
        let mut sse = SseReader::new(reader);
        loop {
            if cancel.load(Ordering::Relaxed) { return Ok(None); }   // ← 必须
            let Some(ev) = sse.next_event().map_err(...)? else { break };
            // match 事件类型:
            //   文本增量   → 攒 buffer + on_event(TextDelta)
            //   工具调用   → 攒参数碎片;首次见到 name 时 on_event(ToolCallBegun)
            //   用量/结束  → 记录,break
            //   错误       → return Err(ProviderError::fatal(...))
            //   其他       → 忽略(向前兼容)
        }
        // 组装 Vec<Block> → Ok(Some(TurnOutput { message, usage, stop }))
    }
}
```

可直接复用的共用件(`provider/mod.rs`):`http_agent()`(超时+代理)、
`post_sse()`(错误分类、Retry-After、错误体解析)、`SseReader`、
`parse_args()`(参数碎片→JSON,空→`{}`,坏→保留原文让模型自愈)、
`args_to_string()/args_to_object()`、`url_join()`。

### 3. 注册(一行)

```rust
// provider/mod.rs::build_provider
ApiKind::Gemini => Box::new(gemini::GeminiProvider::new(settings)),
```

Agent Loop、TUI、config 流程零改动——这就是需求里
"新增 Provider 不需要修改 Agent Loop"的兑现处。

### 4. 测试(两层,都不碰真实网络)

**形状回归测试**(适配器文件内,抄 `anthropic.rs::tests`):
构造一段含 ToolUse/ToolResult 的历史,断言 `build_body` 产出的 JSON
每个关键字段——这是你未来重构时的安全网。

**导线级测试**(`tests/wire.rs`,抄任意一个既有用例):
把该 API 的真实 SSE 流(curl 抓的)简化成两段脚本——第一段让"模型"
调 `read_file`,第二段收尾——mock 服务器会替你校验:
请求体格式对不对、工具真的执行了、结果正确回填了。

### 5. 配置样例 + 文档

`config.example.toml` 加一段;若该家有私有状态(类似 Responses 的
reasoning),在 04 文档的对照表里补一列。

## 常见错误对照表(每一条都对应真实事故)

| 症状 | 病因 |
|---|---|
| 第二轮请求 400 "roles must alternate" | 忘了合并相邻同角色消息(Anthropic 系) |
| 400 "tool call id not found" / 结果配不上 | ToolUse.id 与 ToolResult.tool_use_id 没对上;或该服务不发 id 而你没造一个 |
| 参数解析 panic 或空参数 | 工具参数是**字符串化 JSON** 而你当对象读了;或空字符串没按 `{}` 处理 |
| 工具声明被拒 | 包装层级错了(Chat 有 `function` 包层,Responses 平铺) |
| 流"正常"结束但少内容 | 只认 `[DONE]`/EOF,没处理该家的终止事件;或把未知事件当错误断流了 |
| 用量恒为 0 | 忘了显式请求(Chat 的 `stream_options`),或读错事件位置 |
| 取消无效 | 流循环里没检查 `cancel` |
| 偶发 UI 卡死 | 在 `on_event` 回调里做了阻塞的事(回调必须只转发) |

## 检查表

- [ ] `build_body` 覆盖全部 Block 变体(不认识的显式丢弃,别 panic)
- [ ] 相邻消息/角色约束满足该 API 要求
- [ ] 流循环每次迭代检查 `cancel`,取消返回 `Ok(None)`
- [ ] 未知事件忽略;脏 JSON 行跳过;流掐断时收编半成品
- [ ] `ProviderError.retryable` 只给 408/429/5xx/网络错误
- [ ] 用量、stop_reason 正确映射
- [ ] 形状回归测试 + wire 测试通过
- [ ] `/provider` 热切换后,带工具往返的历史能被它正确编码(最容易漏)

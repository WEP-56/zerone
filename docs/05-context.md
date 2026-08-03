# 05 · Context 系统:模型每轮"看到什么"

对应源码:`src/context/mod.rs`(契约)、`instructions.rs`、
`workspace_info.rs`、`conversation.rs`,以及 `runtime.rs` 的装配处。

## 为什么这是独立的一层

Agent 的行为上限由工具决定,行为质量却几乎全由上下文决定。
把"组装上下文"从 Loop 里抽出来,是因为它是**变化最频繁**的东西:
今天想加项目结构、明天想加 TODO 列表、后天想做历史压缩——
这些都不该动 Loop 一根手指。

## 契约

```rust
pub struct PromptContext {
    pub system_sections: Vec<String>,   // 拼成最终 system 文本
    pub messages: Vec<ChatMessage>,     // 对话消息
}

pub trait ContextProvider: Send {
    fn name(&self) -> &'static str;
    fn contribute(&self, prompt: &mut PromptContext, ws: &Workspace);
}
```

每轮调模型前,Runtime 依次调用注册的 provider(`Agent::build_prompt`):

```rust
for c in &self.extra_context { c.contribute(&mut prompt, &self.workspace); }
self.conversation.contribute(&mut prompt, &self.workspace);   // 历史永远最后
```

顺序即优先级:先注入的片段出现在 system 更靠前的位置。
system 最终落到哪,是各 API 适配器的事(Messages 的 `system` 字段 /
Chat 的 system 消息 / Responses 的 `instructions`)——context 层不关心。

## 三个默认实现

| 实现 | 贡献什么 | 备注 |
|---|---|---|
| `Instructions` | 行为准则(默认内置英文提示词,`config.toml` 的 `system_prompt` 可整体替换) | 提示词是 harness 的调参面板,鼓励改 |
| `WorkspaceInfo` | 环境三行:工作目录、OS、run_command 用的 shell | 这就是 Workspace Context 的最小形态 |
| `Conversation` | 全部对话历史 | 唯一贡献 messages 的;Runtime 具体持有它(要写入) |

`Conversation` 的双重身份值得注意:它既被 Runtime 直接持有
(push/clear 需要 `&mut`),又实现 ContextProvider(供统一组装)。
这样"历史进入上下文"和"环境进入上下文"走同一条路,
而写入路径保持简单——不需要 `Rc<RefCell>` 之类的把戏。

## 预留扩展位(需求里点名的三个,各自怎么做)

### Planning Context
1. 写一个 `todo` 工具(模型可增删改条目,状态存在 struct 里);
2. 该 struct 同时实现 ContextProvider,把当前计划渲染成一段
   system 片段("当前计划:1.✅… 2.▢…");
3. 在 `Agent::new` 的 `extra_context` 列表里插入(源码中有标注的插入点)。

工具与上下文共享状态即可,Loop/Provider 零改动。这正是 Claude Code
TODO 机制的骨架。

### Workspace Map
启动时(或每轮)扫描项目结构,输出精简目录树片段。
实现在 `WorkspaceInfo` 里扩展即可——`contribute` 拿得到 `&Workspace`,
文件访问原语都是现成的。注意控制体积(深度/条数上限,参考 `list_dir`)。

### Memory(跨会话)
一个读 `MEMORY.md` 之类文件的 ContextProvider(注入)+ 一个写它的工具
(沉淀)。模式与 Planning 完全相同:工具负责写,context 负责读。

## 上下文压缩(Compression)的正确挂载点

就一行:`Conversation::contribute` 里那句全量克隆(源码有标注)。
把它换成:

```text
若估算 token 超阈值:
    保留最近 N 条消息原样;
    更早的消息折叠成一条摘要(可以另调一次便宜模型生成);
    摘要作为一条 User 文本消息放在最前。
```

注意两个不变量:折叠不能拆散 ToolUse/ToolResult 配对(要么整对保留,
要么整对进摘要);摘要本身要标明"这是压缩产物"。
Runtime、Provider、TUI 对此完全无感——这就是把历史藏在
ContextProvider 后面的回报。

## 扩展指引(动手清单)

- 改默认提示词感受行为变化:`context/instructions.rs` 的 `DEFAULT`;
- 给 `WorkspaceInfo` 加"当前 git 分支"一行(调 `git rev-parse`,
  注意失败要静默);
- 实现上面的 PlanningContext——它是三个预留位里收益最直观的;
- 做一个 `/context` 调试命令:把 `build_prompt()` 的结果 dump 出来看,
  你会对"模型到底看到了什么"祛魅。

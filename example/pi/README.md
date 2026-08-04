# Pi harness 教学切片

这个目录是为 Zerone 准备的一个**有意不完整、经过筛选的 Pi 源码参考副本**。

它只保留了在讲解一个小型 coding-agent（代码智能体）框架时有用的 Pi 机制。它**不是一个可直接构建的完整 Pi 仓库**，其中不包含扩展系统、TUI、发布工具、模型目录、OAuth 流程，也没有完整的应用外壳。

源码来源以及具体的裁剪策略请查看 `SNAPSHOT.md`。

---

# 阅读顺序

## 1. Agent 循环与取消机制

从以下文件开始：

* `packages/agent/src/agent-loop.ts`
* `packages/agent/src/types.ts`
* `packages/agent/test/agent-loop.test.ts`
 
循环（Loop）负责：

* 一轮对话（turn）的执行顺序
* Provider 流式输出
* 工具准备与执行
* Observation 消息处理
* 中断（abort）检查

以下内容**不属于这个文件的职责**：

* Provider 选择
* 持久化
* UI 状态管理

---

## 2. Runtime 与前端边界

阅读：

* `packages/agent/src/types.ts`（`AgentEvent`）
* `packages/agent/src/agent.ts`（`processEvents`、订阅机制、队列、abort）
* `packages/agent/test/agent.test.ts`

Pi 会：

* 产生 runtime event（运行时事件）
* 允许调用方订阅这些事件

Zerone 的 `AgentCommand` 是 Zerone 自己定义的输入边界。

在这个裁剪版本中，Pi **没有使用类似的 command union（命令联合类型）设计**。

---

## 3. 三种 Provider API 形态

先阅读公共契约和分发逻辑：

* `packages/ai/src/types.ts`

  * `KnownApi`
  * `ProviderStreams`
  * `ApiOptionsMap`

* `packages/ai/src/models.ts`

  * `createProvider`

* `packages/ai/src/api/lazy.ts`

然后对比三个适配器：

* `packages/ai/src/api/anthropic-messages.ts`
* `packages/ai/src/api/openai-completions.ts`
* `packages/ai/src/api/openai-responses.ts`
* `packages/ai/src/api/openai-responses-shared.ts`

三个适配器最终都暴露**相同的流式接口契约**。

`model.api` 会在进入 Agent Loop 之前选择对应的 adapter（适配器）。

---

## 4. Workspace 与工具归属

阅读：

* `packages/agent/src/harness/types.ts`

  * `ExecutionEnv`
  * 类型化错误
  * `AgentHarnessTool`

* `packages/agent/src/harness/env/nodejs.ts`

* `packages/agent/src/harness/tools/index.ts`

* `packages/agent/src/harness/tools/{read,write,edit,bash}.ts`

* `packages/agent/test/harness/tools.test.ts`

执行环境（Execution Environment）负责：

* 文件系统能力
* 进程执行能力

工具依赖这个接口，而不是直接访问 Agent Loop 内部状态。

---

## 5. Session 所有权

阅读：

* `packages/agent/src/harness/session/repository.ts`
* `packages/agent/src/harness/session/session.ts`
* `packages/agent/src/harness/session/memory-repo.ts`
* `packages/agent/src/harness/session/jsonl-repo.ts`
* `packages/agent/test/harness/{session,repo}.test.ts`

职责划分：

* `Session`

  负责：

  * 对话记录（transcript）
  * 树结构语义（tree semantics）

* `SessionStorage`

  负责：

  * 字节存储（实际数据保存）

* Repository

  负责：

  * 创建 session
  * 打开 session
  * 列出 session
  * 删除 session
  * fork session（分叉）

---

## 6. 错误与重试策略

阅读：

* `packages/agent/src/harness/types.ts`

  * `FileError`
  * `ExecutionError`
  * `SessionError`

* `packages/ai/src/utils/error-body.ts`

* `packages/ai/src/utils/provider-retry.ts`

* `packages/ai/src/utils/retry.ts`

以及：

* `packages/ai/test` 下对应的专项测试

重点理解三个层面的区别：

1. **Adapter 边界上的预期错误**

   （typed expected failures）

2. **Provider 请求重试**

   （provider-request retry）

3. **更高层级的 assistant turn 重试**

   （assistant-turn retry）

---

# 有意缺失的部分

以下内容被刻意移除：

* extensions（扩展）

* plugins（插件）

* skills

* prompt-template 加载机制

* TUI 和交互式应用模式

* compaction（上下文压缩）

* branch summarization（分支总结）

* Provider 身份认证

* 模型发现机制

* 除以下 API 外的其他 Provider：

  * Anthropic Messages
  * OpenAI Completions
  * OpenAI Responses

* 图片生成

* OAuth

* telemetry（遥测）

* evals（评测）

* server/client packages

以及：

* monorepo 构建系统
* CI
* 发布流程
* release 工具链

---

当需要了解上述列表之外的行为时，请查看上游完整仓库。

---


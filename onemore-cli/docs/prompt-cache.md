# Prompt Cache Design

## Decision

Onemore treats prompt caching as a provider-side optimization. The client does
not store or replay model KV tensors. Its responsibility is to produce a long,
deterministic prompt prefix, send provider-specific cache controls only when
they are supported, and record the reported cache usage.

The primary goal is to lower input-token cost and prefill latency without
changing model-visible semantics. A cache miss is acceptable; a cache-oriented
rewrite that changes the conversation or tool protocol is not.

## Model

Prompt caches reuse the attention KV state of an exact token prefix. They are
not semantic caches. A change early in the prompt prevents reuse of every
following token.

```text
stable prefix
  system instructions
  stable workspace policy and tool declarations
  append-only conversation history

changing suffix
  newest user input
  newest tool results
  steering and follow-up input
```

The prompt must be stable in content and ordering. Do not put timestamps,
random identifiers, usage counters, volatile git state, or dynamically ordered
tool declarations in the stable prefix.

## Current Baseline

The current session model is already favorable for prefix reuse:

- Session facts are append-only and project into model messages in order.
- A normal turn appends new user, assistant, and tool-result messages at the
  tail.
- Built-in tools are registered in a deterministic order.
- The system prompt and workspace context are assembled before conversation
  history.

Several intentional operations create a cache boundary:

- Switching provider or model.
- Changing the system prompt or a tool schema.
- Manual compaction, which replaces the model view with a new summary.
- Context-budget shortening that changes an older tool result.

Those operations must remain correct even when they cause a cache miss.

## Provider Policy

Provider support is not inferred from the endpoint name.

- OpenAI Responses: automatic caching is available on eligible models. Some
  newer models additionally support a cache key and explicit breakpoints.
- Anthropic Messages: explicit cache-control blocks are provider/model
  dependent.
- DeepSeek Responses: context caching is automatic and reports cached input
  tokens, but prompt cache keys and retention controls are not supported.
- DeepSeek Anthropic compatibility: `cache_control` is ignored, so an
  Anthropic-format request must not be treated as an explicit cache write.

Every cache parameter must be guarded by provider capabilities. Unsupported
parameters must not be emitted merely because a compatible provider silently
ignores them.

## Measurements

The first implementation step is observability, not cache-control fields.
Provider usage should preserve these values when they are reported:

```rust
pub struct CacheUsage {
    pub read_tokens: u64,
    pub write_tokens: u64,
}
```

`Usage` should retain normal input and output tokens plus optional cache usage.
The UI and session facts can then report:

- input tokens;
- cached input tokens;
- cache-write tokens;
- cache-read ratio (`read_tokens / input_tokens`);
- effective input cost when the configured model price is known.

Do not persist complete request bodies only to measure caching. Persist usage
and a non-secret prompt fingerprint instead.

## Prompt Fingerprint

Before adding provider controls, build a deterministic fingerprint from the
provider-rendered semantic prompt:

```text
provider family
model
system prompt version
tool schema digest
stable workspace-context version
projected message prefix digest
```

This is diagnostic data, not a substitute for provider matching. It should
identify why two consecutive requests cannot share a prefix without exposing
user content. A new turn should normally extend the previous fingerprint rather
than rewrite it.

## Cache Keys And Breakpoints

When a provider supports them, cache keys identify a prompt family, not a turn.
They must not contain a message sequence number, timestamp, random UUID, or
session ID.

An appropriate key shape is:

```text
onemore:v1:<provider>:<model>:<workspace-policy>:<system>:<toolset>
```

Explicit breakpoints belong after content that is both large and expected to be
reused. They are only useful when the provider capability says they are valid.
The cache-write cost must be amortized over later reads; do not mark a large,
one-off tool result as a write candidate.

## Delivery Plan

Current implementation status:

- cache reads and writes are parsed, accumulated, persisted, and exposed to the
  CLI/TUI for OpenAI Responses, DeepSeek Responses, and Anthropic Messages;
- provider profiles gate private request fields; DeepSeek never receives an
  OpenAI cache key or encrypted-reasoning request;
- tool declarations are sorted by name, prompt fingerprints are persisted with
  assistant facts, and OpenAI requests use a stable prompt-family cache key;
- explicit OpenAI breakpoints and Anthropic cache writes remain disabled until
  model-level capabilities and write-cost policy are configured.

1. Parse and persist provider cache usage, with fixture tests for every
   supported provider profile.
2. Add deterministic prompt fingerprints and notices explaining prefix changes.
3. Keep the existing prompt layout stable; remove accidental dynamic fields and
   make tool declaration order explicit.
4. Add provider capability-gated OpenAI cache keys and breakpoints where the
   configured model supports them.
5. Add capability-gated Anthropic cache controls.
6. Use real usage data to decide whether explicit writes lower cost for a model
   and workload.

## Acceptance Criteria

- A provider that does not report cache usage behaves exactly as before.
- An unsupported cache field is never sent.
- Identical consecutive prompt prefixes have identical local fingerprints.
- A cache miss cannot alter messages, tool calls, permissions, or session facts.
- Wire fixtures verify cache-usage parsing and provider-specific request shape.

## References

- [OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching)
- [DeepSeek Responses API compatibility](https://api-docs.deepseek.com/zh-cn/guides/responses_api)
- [DeepSeek Anthropic API compatibility](https://api-docs.deepseek.com/zh-cn/guides/anthropic_api)

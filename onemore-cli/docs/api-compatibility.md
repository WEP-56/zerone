# API Compatibility And Chat Completions Removal

## Decision

Remove the OpenAI Chat Completions adapter. Onemore will support two provider
families:

- OpenAI Responses-style APIs;
- Anthropic Messages-style APIs.

Chat Completions remains useful in the wider ecosystem, but keeping a third
first-party protocol multiplies request encoding, stream parsing, tool-pairing,
usage parsing, context normalization, compatibility testing, and cache policy.
It is not required for the intended learning project.

This is a breaking configuration change. A profile with `api = "chat"` is
rejected as an unknown API type. No configuration migration path is provided:
the project is not released yet, and callers must choose `messages` or
`responses` explicitly.

## Compatibility Principle

Responses and Messages are provider protocols, not universal standards with
full semantic conformance. A provider can accept the same JSON shape while
ignoring fields, remapping model names, omitting stream events, or assigning
different meaning to state, reasoning, caching, and tools.

Onemore therefore has three layers:

```text
Session and Runtime
  provider-neutral messages, tool calls, results, permissions, and events

Provider family adapter
  Responses or Messages request/stream conversion

Provider profile capabilities
  model and vendor-specific supported behavior
```

The Runtime must not branch on provider names. Adapters own wire details;
capabilities decide which optional behavior is enabled.

## Baseline Families

### Responses

Responses uses typed input and output items. A `message`, `reasoning`,
`function_call`, and `function_call_output` are separate items. Tool results
are correlated by `call_id`.

OpenAI Responses may support encrypted reasoning replay, stateful chaining,
conversations, cache keys, explicit cache breakpoints, and built-in tools. Each
of these is optional from Onemore's point of view.

### Messages

Messages uses alternating user/assistant messages with typed content blocks.
Tool calls are `tool_use`; tool results are `tool_result`. System instructions,
thinking, cache controls, images, documents, and server-side tools are all
capability-gated extensions.

## Initial Provider Profiles

The first capability matrix covers OpenAI, Anthropic, and DeepSeek. It should
be data, not a collection of name checks spread through adapters.

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
}
```

The exact representation may evolve, but every feature must have a declared
default of unsupported.

| Profile | Family | Important constraints |
|---|---|---|
| OpenAI | Responses | Supports the canonical Responses item model; optional features remain model-dependent. |
| Anthropic | Messages | Supports the canonical Messages block model; advanced blocks are model-dependent. |
| DeepSeek Responses | Responses | Stateless; no `previous_response_id`, conversation, cache key, retention, encrypted reasoning, images, or files. Context cache is automatic. |
| DeepSeek Anthropic | Messages | Anthropic-shaped compatibility endpoint; `cache_control`, `anthropic-version`, and `anthropic-beta` are ignored; several multimodal and MCP blocks are unsupported. |

DeepSeek compatibility documentation explicitly states that unsupported fields
may be silently ignored. Onemore must not rely on a remote error to detect an
unsupported feature.

## Known Responses Gap

The current Responses adapter handles
`response.reasoning_summary_text.delta`. DeepSeek Responses documents
`response.reasoning_text.delta` and `response.reasoning_text.done` instead.

The adapter must normalize both event forms when the profile advertises them.
The final reasoning item must also be decoded according to the profile: OpenAI
can require an encrypted raw item for a later turn, while DeepSeek reports
plain reasoning and does not support encrypted replay.

Do not solve this by accepting arbitrary foreign raw items. Raw reasoning may
only be replayed to the same provider profile that produced it.

## Chat Completions Removal Scope

The removal changes all public and test-facing references to `ApiKind::Chat`:

- remove `src/provider/openai_chat.rs` and its module export;
- remove the `Chat` enum variant and parser branch;
- remove Chat request/body/stream tests and the Chat wire fixture;
- reject `api = "chat"` in configuration as an unsupported API type;
- remove the `openai-chat` example profile and documentation references;
- update package and README verification counts.

Existing users should create a `responses` profile for a provider that supports
Responses. A provider that only exposes Chat Completions is intentionally out
of scope after this change.

## Delivery Plan

1. Delete Chat Completions and make configuration rejection explicit.
2. Preserve and extend Responses and Messages wire fixtures before adding new
   capability fields.
3. Introduce a profile capability object with conservative defaults.
4. Implement the OpenAI, Anthropic, DeepSeek Responses, and DeepSeek Messages
   profiles.
5. Normalize capability-gated reasoning streams and usage details.
6. Add cache controls only after the prompt-cache measurements exist.
7. Add another vendor only with a documented profile, a request fixture, a
   normal stream fixture, and a failure/unsupported-feature fixture.

## Acceptance Criteria

- No Chat Completions code path or configuration value remains.
- Responses and Messages full tool round trips continue to pass wire tests.
- Provider-specific optional fields are emitted only when advertised.
- Unsupported fields are rejected or omitted locally; they are never trusted to
  be harmless because a provider might silently ignore them.
- DeepSeek Responses reasoning text events and terminal events have fixtures.
- Provider switching never replays vendor-private reasoning to a different
  provider profile.

## References

- [OpenAI Responses migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)
- [Anthropic Messages API](https://docs.anthropic.com/en/api/messages)
- [DeepSeek Responses API compatibility](https://api-docs.deepseek.com/zh-cn/guides/responses_api)
- [DeepSeek Anthropic API compatibility](https://api-docs.deepseek.com/zh-cn/guides/anthropic_api)

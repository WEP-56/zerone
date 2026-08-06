# Onemore Open-Source Research Handoff

## Purpose

The open-source coding-agent research pass is complete. Its primary result and source of truth is:

```text
E:\harness from scratch\onemore-cli\docs\open-source-agent-research.md
```

Read that document before designing advanced features. It contains the evidence-backed comparison, invariants, failure cases, proposed Onemore types/events/facts/tests, cache implications, and staged delivery recommendation. Do not re-run the broad Grok Build/Codex survey unless a specific claim needs revalidation against a newer upstream snapshot.

Recommended implementation order from the research:

1. ~~`update_plan` and long-running task discipline~~ (implemented 2026-08-05, user-verified 2026-08-06);
2. Skills (next session);
3. MCP stdio integration;
4. durable background processes, then subagents.

## Current Onemore State

Repository: `E:\harness from scratch\onemore-cli`

Current version: `0.4.0`

Implemented foundations:

- Anthropic Messages and OpenAI Responses provider families;
- explicit OpenAI, Anthropic, DeepSeek Responses, and DeepSeek Messages profiles;
- capability-gated private request fields;
- DeepSeek Responses reasoning stream compatibility;
- prompt-cache usage parsing, accumulation, persistence, and CLI/TUI display;
- stable OpenAI `prompt_cache_key` and SHA-256 prompt fingerprints;
- append-only session facts, compaction, context budgeting, permissions, hooks, retries, cancellation, steering/follow-up, controlled tool concurrency, and resource locks.
- structured `update_plan`, strict revision reducer, committed plan events, bounded long-task reminders, cancellation repair, compaction projection, and CLI/TUI restore.

Intentionally absent:

- Skills;
- MCP client/server integration;
- background processes as durable tasks;
- subagents.

## Verification Status

User acceptance testing on 2026-08-06 confirmed:

- standard and custom reasoning effort lists are selectable in the TUI and are sent correctly by the configured provider;
- `update_plan` works through the full plan lifecycle and survives the intended runtime/UI path;
- long-running task reminders behave as intended without preventing completion.

The next session should begin the Skills implementation. Keep the existing plan reducer, fact/event boundaries, and prompt-cache constraints intact while adding skill discovery and loading.

Relevant Onemore files:

```text
src/runtime.rs              agent loop, ActiveRun, queues, tool batches
src/plan.rs                 plan types, invariants, reducer, compaction projection
src/session.rs              facts and model projection
src/storage.rs              append-only SQLite persistence
src/tools/mod.rs            tool contracts, registry, validation
src/tools/update_plan.rs    complete-snapshot update_plan tool and effect
src/context/mod.rs          system context composition
src/provider/mod.rs         provider capabilities and prompt identity
src/event.rs                Runtime/frontend event boundary
src/tui/mod.rs              event rendering and interactive UI
docs/prompt-cache-cn.md      cache design and current implementation status
docs/api-compatibility-cn.md provider compatibility design
```

## Cache Test Observation

The user is currently exercising the packaged Onemore through a 2api layer with DeepSeek Responses.

Observed examples:

- initial task/tool-reading phase: about `1.3K` cached tokens over `3,961` input tokens;
- a following similar request: about `1.3K` cached over `1,337` input tokens;
- high-frequency coding phase: about `47.6K` cached over `633` uncached/input tokens as displayed by the proxy.

The working product threshold is approximately 80% cache reuse in ordinary long sessions. Continue observing real traffic; do not add explicit cache writes or model-specific cache tuning during this research task.

## Reference Snapshots

Root: `E:\harness from scratch\example`

### Grok Build

```text
Path:   E:\harness from scratch\example\grok-build
Source: https://github.com/xai-org/grok-build
Commit: ed6d543643628663873c5de28298e022ed634238
```

### OpenAI Codex

```text
Path:   E:\harness from scratch\example\codex
Source: https://github.com/openai/codex
Commit: ed2f985a26eee9a59cde0fdefd20f69b45bc25f5
```

Both are shallow source snapshots. Their `.git` directories and Codex CI/editor configuration were moved out of the source roots into `example/.pruned/`; `example/.gitignore` ignores both reference trees and the pruned metadata. Source, tests, docs, manifests, and licenses remain available.

Do not treat either repository as code to copy wholesale. Extract invariants, state machines, boundaries, and tests. Verify license and dependency implications before proposing any direct reuse.

## Suggested Entry Points

### Todo And Long Tasks

Implemented in Onemore as of 2026-08-05. Keep the references and questions below as regression/design evidence; new work should not replace the strict reducer with prompt-only validation or UI-only state.

Grok Build:

```text
crates/codegen/xai-grok-tools/src/implementations/grok_build/todo/mod.rs
crates/codegen/xai-grok-shell/src/tools/todo.rs
crates/codegen/xai-grok-agent/src/system_reminder.rs
crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn_end.rs
crates/common/xai-grok-compaction/src/reminder.rs
```

Codex:

```text
codex-rs/protocol/src/plan_tool.rs
codex-rs/core/src/tools/handlers/plan.rs
codex-rs/core/src/tools/handlers/plan_spec.rs
codex-rs/core/src/tools/spec_plan.rs
codex-rs/protocol/src/prompts/base_instructions/default.md
codex-rs/app-server/tests/suite/v2/plan_item.rs
```

Questions:

- Is the plan runtime state, a persisted fact, or both?
- What invariants constrain pending/in-progress/completed items?
- How does the harness prevent stale in-progress items at turn end?
- How are plan updates emitted to the frontend without polluting model history?
- What survives compaction and session restore?

### Skills (Next Session)

Grok Build:

```text
crates/codegen/xai-grok-agent/src/prompt/skills.rs
crates/codegen/xai-grok-tools/src/implementations/skills/
crates/codegen/xai-grok-tools/src/types/skill_discovery_tracker/
crates/codegen/xai-grok-tools/src/reminders/skill_discovery.rs
crates/codegen/xai-grok-shell/src/extensions/skills.rs
crates/codegen/xai-grok-pager/docs/user-guide/08-skills.md
```

Codex:

```text
codex-rs/ext/skills/src/
codex-rs/core-skills/
codex-rs/core/src/skills.rs
codex-rs/app-server/src/skills_watcher.rs
codex-rs/core/tests/suite/skills.rs
codex-rs/core/tests/suite/skill_approval.rs
```

Questions:

- How are skills discovered, scoped, selected, and loaded lazily?
- Which metadata is placed in the stable prompt prefix?
- How are malformed or conflicting skills handled?
- How do skill tools pass through permission and capability checks?
- How does discovery avoid invalidating prompt caches every turn?

### MCP

Grok Build:

```text
crates/codegen/xai-grok-mcp/src/
crates/common/xai-computer-hub-mcp-adapter/src/
crates/codegen/xai-grok-shell/src/session/mcp_dispatcher.rs
crates/codegen/xai-grok-shell/src/session/managed_mcp.rs
crates/codegen/xai-grok-config-types/src/mcp.rs
crates/codegen/xai-grok-shell/tests/test_mcp_integration.rs
```

Codex:

```text
codex-rs/codex-mcp/src/connection_manager.rs
codex-rs/codex-mcp/src/connection_manager/
codex-rs/codex-mcp/src/catalog.rs
codex-rs/codex-mcp/src/tools.rs
codex-rs/config/src/mcp_types.rs
codex-rs/core/tests/suite/mcp_tool_cache.rs
codex-rs/core/tests/suite/mcp_tool_exposure.rs
```

Questions:

- How are initialize, `tools/list`, `tools/call`, cancellation, timeout, and shutdown modeled?
- How are remote schemas converted into local tool contracts?
- How are namespaced names, collisions, refresh, and server failure handled?
- Are remote tool schemas sorted and cached deterministically?
- Where do permission checks and result truncation occur?

### Subagents And Background Tasks

Grok Build:

```text
crates/common/xai-tool-types/src/task.rs
crates/codegen/xai-grok-tools/src/implementations/grok_build/task/
crates/codegen/xai-grok-tools/src/implementations/grok_build/task_output/
crates/codegen/xai-grok-tools/src/implementations/grok_build/kill_task/
crates/codegen/xai-grok-shell/src/terminal/background_task.rs
crates/common/xai-grok-compaction/src/reminder.rs
```

Codex:

```text
codex-rs/core/src/agent/control.rs
codex-rs/core/src/agent/control/spawn.rs
codex-rs/core/src/agent/control/legacy.rs
codex-rs/core/src/agent/agent_resolver.rs
codex-rs/core/src/codex_delegate.rs
codex-rs/core/src/agent/control_tests.rs
```

Questions:

- What owns the child lifecycle and concurrency slot?
- How are parent context, instructions, usage hints, and compaction state forked?
- How are send/wait/cancel/close represented and persisted?
- How are workspace write conflicts and tool permissions isolated?
- How are child results returned without importing the entire child transcript?
- How are token usage, maximum depth, maximum children, and failure propagation bounded?

## Completed Research Deliverable

The completed result is `onemore-cli/docs/open-source-agent-research.md`. It includes:

1. a compact architecture map for each reference project;
2. a behavior and data-model comparison for Todo, Skills, MCP, and Subagents;
3. invariants and failure cases worth adopting;
4. mechanisms that are too complex or product-specific for Onemore;
5. a staged Onemore design, beginning with Todo/long-task support;
6. proposed types, Session Facts, Agent events, tool contracts, and tests;
7. cache implications for every feature that changes instructions or tool schemas.

Treat this document as the baseline for follow-up design. The research pass itself did not copy upstream implementation code; any later direct reuse still requires dependency and license review.

## Recommended First Commands

```powershell
Get-Content "E:\harness from scratch\onemore-cli\docs\handoff.md"
Get-Content "E:\harness from scratch\onemore-cli\docs\open-source-agent-research.md"
rg -n "todo_write|update_plan" "E:\harness from scratch\example\grok-build\crates" "E:\harness from scratch\example\codex\codex-rs"
rg -n "skill|MCP|spawn_agent|subagent" "E:\harness from scratch\example\grok-build\crates" "E:\harness from scratch\example\codex\codex-rs\core\src"
```

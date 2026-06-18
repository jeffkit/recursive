---
type: Architecture
title: The Eight Invariants
description: The eight inviolable rules every change to Recursive must respect. Drawn from .dev/AGENTS.md. Violations cause rollback in the self-improve loop.
tags: [invariants, architecture, rules, self-improve]
timestamp: 2026-06-18T10:00:00Z
---

# The Eight Invariants

These rules are enforced by the self-improve loop. A change that violates any
invariant will be rolled back. Read `.dev/AGENTS.md` for the full text.

## Invariant #1 — Agent Loop Stays Small

> New capabilities go in **tools**, not in `AgentRuntime::run` (previously `Agent::run`).
> The main loop is a pure dispatch loop. If/else branching inside it is a red flag.

Impact: [Agent Loop](agent-loop.md), [Tools Overview](tools/index.md)

## Invariant #2 — Error Variants Live in error.rs

> New error types go in `src/error.rs`. **Never `unwrap()` or `expect()` in product code.**
> Tests may use `unwrap()`.

## Invariant #3 — Sandbox via resolve_within

> All filesystem and shell tools MUST pass user-supplied paths through
> `tools::resolve_within(workspace, path)`. Paths that escape the workspace
> return `Error::PathOutsideSandbox`, they do not panic.

Impact: [Filesystem Tools](tools/filesystem.md), [Shell Tool](tools/shell.md)

## Invariant #4 — New Tool → New File

> A new tool gets a new file under `src/tools/<name>.rs`. It is registered
> in `src/tools/mod.rs` and `build_standard_tools`. No tool logic goes
> directly into `runtime.rs`, `kernel.rs`, or `agent/`.

## Invariant #5 — No unwrap() in Product Code

> (See Invariant #2.) Specifically: never use `unwrap()` or `expect()` on
> `Result` or `Option` in any non-test code path.

## Invariant #6 — New Provider → New File + Trait

> A new LLM provider gets a new file under `src/llm/<name>.rs` that
> implements `ChatProvider`. No provider logic in the agent or runtime.

Impact: [Providers Overview](providers/index.md)

## Invariant #7 — Finish Reasons Are Data, Not Errors

> `AgentRuntime::run` returns `Ok(AgentOutcome { finish: FinishReason })` for
> **all** termination modes, including `BudgetExceeded`, `Stuck`, and
> `TranscriptLimit`. The transcript is **always saved** before returning.
>
> **Never** introduce a new `Error::Xxx` variant that short-circuits the
> transcript save. The self-improve auto-resume gate depends on a saved
> transcript existing.

Impact: [Agent Loop](agent-loop.md), [Sessions](sessions.md)

## Invariant #8 — Tool-Call ↔ Tool-Result Pairing

> Every `Role::Tool` message MUST stay **immediately after** the
> `Role::Assistant` message whose `tool_calls` array lists its `id`.
>
> Any operation that mutates the transcript — **compaction, trimming,
> splicing, resume replay** — MUST preserve this pairing.
>
> Orphaned tool results cause HTTP 400 from OpenAI / DeepSeek / Anthropic.
>
> Regression test: `compaction_keeps_tool_calls_paired_with_results`

Impact: [Agent Loop](agent-loop.md), [Sessions](sessions.md)

---

## Quick Reference

| # | Rule | Key files |
|---|------|-----------|
| 1 | Loop stays small — tools, not branches | `src/runtime.rs`, `src/kernel.rs` |
| 2 | Errors in error.rs, no unwrap | `src/error.rs` |
| 3 | Sandbox via resolve_within | `src/tools/dispatch.rs` |
| 4 | New tool → new file | `src/tools/` |
| 5 | No unwrap in product code | (everywhere) |
| 6 | New provider → new file | `src/llm/` |
| 7 | Finish reasons are data | `src/agent/types.rs`, `src/runtime.rs` |
| 8 | Tool-call ↔ result pairing | `src/compact.rs`, `src/session/` |

## Related Concepts

- [Overview](overview.md) — component map
- [Agent Loop](agent-loop.md) — Invariants 1, 7, 8
- [Filesystem Tools](tools/filesystem.md) — Invariant 3
- [Sessions](sessions.md) — Invariants 7, 8

---
type: Architecture
title: The Ten Invariants
description: The ten inviolable rules every change to Recursive must respect. Drawn from .dev/AGENTS.md. Violations cause rollback in the self-improve loop.
tags: [invariants, architecture, rules, self-improve]
timestamp: 2026-06-18T10:00:00Z
---

# The Ten Invariants

These rules are enforced by the self-improve loop. A change that violates any
invariant will be rolled back. Read `.dev/AGENTS.md` for the full text.

The (number → title) mapping below is kept in sync with the numbered list in
`.dev/AGENTS.md` by the guard test `tests/invariants/invariant_registry.rs`.
If you renumber the invariants, update both documents (and the test) together.

## Invariant #1 — Agent Loop Stays Small

> New capabilities go in **tools**, not in the agent loop. The main loop is a
> pure dispatch loop. If/else branching inside `run_inner` is a red flag.

Impact: [Agent Loop](agent-loop.md), [Tools Overview](tools/index.md)

## Invariant #2 — Orthogonality

> Tools must not depend on LLM internals; providers must not depend on tools.

Impact: [Tools Overview](tools/index.md), [Providers Overview](providers/index.md)

## Invariant #3 — Sandbox

> All filesystem and shell tools MUST pass user-supplied paths through
> `tools::resolve_within(workspace, path)`. Paths that escape the workspace
> return `Error::PathOutsideSandbox`, they do not panic.

Impact: [Filesystem Tools](tools/filesystem.md), [Shell Tool](tools/shell.md)

## Invariant #4 — Tests Are Non-Negotiable

> Every new public function / tool / provider gets unit tests in the same
> file (`#[cfg(test)] mod tests`).

## Invariant #5 — No `unwrap()` / `expect()` in Non-Test Code

> Never use `unwrap()` or `expect()` on `Result` or `Option` in any non-test
> code path. Return `Result` instead. Enforced by `clippy::unwrap_used` deny.
> New error variants go in `src/error.rs`.

## Invariant #6 — No New Dependencies Without Justification

> State the reason in the journal entry. Prefer std + what's already in
> `Cargo.toml`.

## Invariant #7 — Finish Reasons Are Data, Not Errors

> `AgentRuntime::run` returns `Ok(RuntimeOutcome { finish_reason })` for
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

## Invariant #9 — New Tool → New File

> A new tool gets a new file under `src/tools/<name>.rs`. It is registered
> in `src/tools/mod.rs` and the standard tool builder. No tool logic goes
> directly into `runtime.rs`, `kernel.rs`, or `agent/`.

Impact: [Tools Overview](tools/index.md)

## Invariant #10 — New Provider → New File + Trait

> A new LLM provider gets a new file under `src/llm/<name>.rs` that
> implements `ChatProvider`. No provider logic in the agent, runtime,
> or kernel.

Impact: [Providers Overview](providers/index.md)

---

## Quick Reference

| # | Rule | Key files |
|---|------|-----------|
| 1 | Loop stays small — tools, not branches | `src/run_core.rs`, `src/kernel.rs` |
| 2 | Orthogonality | `src/tools/`, `src/llm/` |
| 3 | Sandbox via resolve_within | `src/tools/dispatch.rs` |
| 4 | Tests are non-negotiable | `tests/invariants/test_coverage.rs` |
| 5 | No unwrap in product code | `src/error.rs` (variants) |
| 6 | No new dependencies without justification | `tests/invariants/dep_justification.rs` |
| 7 | Finish reasons are data | `src/agent/types.rs`, `src/runtime.rs` |
| 8 | Tool-call ↔ result pairing | `src/compact.rs`, `src/session/` |
| 9 | New tool → new file | `src/tools/` |
| 10 | New provider → new file | `src/llm/` |

## Related Concepts

- [Overview](overview.md) — component map
- [Agent Loop](agent-loop.md) — Invariants 1, 7, 8
- [Filesystem Tools](tools/filesystem.md) — Invariant 3
- [Sessions](sessions.md) — Invariants 7, 8

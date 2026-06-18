---
type: Architecture
title: Agent Loop — AgentRuntime + Kernel
description: The ReAct execution loop, turn structure, finish reasons, stuck detection, compaction, and budget management.
tags: [agent-loop, kernel, runtime, finish-reason]
timestamp: 2026-06-18T10:00:00Z
---

# Agent Loop

Recursive's execution is split across two layers:

- **AgentRuntime** (`src/runtime.rs`) — stateful wrapper; owns the transcript,
  session, compaction, and budget counter across turns.
- **AgentKernel** (`src/kernel.rs`) — stateless single-turn executor; knows
  nothing about transcripts or sessions. Receives a `TurnContext`, returns a
  `TurnOutcome`.

## FinishReason (src/agent/types.rs)

Every run ends with one of these — they are **data, not errors** (Invariant #7):

| Variant | Meaning |
|---------|---------|
| `NoMoreToolCalls` | Model emitted a final response with no tool calls — normal completion |
| `BudgetExceeded` | Agent hit `max_turns` step limit |
| `Stuck { repeated_call, repeats }` | Same failing tool call repeated N times |
| `TranscriptLimit { chars, limit }` | Transcript too large to compact further |
| `ProviderStop(reason)` | LLM provider returned an explicit stop reason |
| `Cancelled` | SIGINT / SIGTERM shutdown signal received |

**Never** introduce a new error variant that short-circuits transcript saving —
see [Invariant #7](/architecture/invariants.md).

## Stuck Detection

Configured via `AgentConfig`:
- `stuck_window` — default 10 (look-back window of tool calls)
- `stuck_error_rate` — default 0.8 (fraction of errors to trigger Stuck)

When triggered, `FinishReason::Stuck` is returned and the transcript is saved
so `self-improve.sh` can inspect what went wrong.

## Compaction

When the transcript grows large, `AgentRuntime` calls `Compactor` to:
1. Ask the LLM to summarize the conversation into `summary`, `kept_facts`, `next_steps`.
2. Replace old messages with a compacted stub.
3. **Tool-call ↔ tool-result pairing must be preserved** (Invariant #8).

`Compactor` lives in `src/compact.rs`. It uses a structured JSON output schema
so the LLM returns machine-readable fields.

## Tool Dispatch Safety

All file and shell tool calls pass through `resolve_within` (Invariant #3) to
enforce that paths stay inside the workspace. Any `..` escape returns an error
rather than panicking.

## Key Constraint: Agent Loop Must Stay Small

> **Invariant #1**: New capabilities go in tools, not in `Agent::run` / `AgentRuntime::run`.
> The main loop is a pure dispatch loop — if/else branching inside it is a red flag.

## Related Concepts

- [Overview](overview.md) — component map
- [Invariants](invariants.md) — all eight invariants
- [Layer 0 Injection](layer0-injection.md) — how system prompt is built before each run
- [Sessions](sessions.md) — how transcripts are persisted
- [Providers Overview](providers/index.md) — what ChatProvider::complete returns

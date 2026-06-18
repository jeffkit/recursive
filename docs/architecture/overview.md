---
type: Architecture
title: Recursive — System Overview
description: High-level architecture, component map, and data flow for the Recursive self-improving coding agent.
tags: [architecture, overview, core]
timestamp: 2026-06-18T10:00:00Z
---

# Recursive — System Overview

Recursive is a self-improving Rust coding agent. Its runtime executes a
ReAct (Reason + Act) loop: the LLM reasons, calls tools, observes results,
and repeats until a [finish reason](/architecture/agent-loop.md) is reached.

## Component Map

```
┌─────────────────────────────────────────────────────────────────┐
│                        Entry Points                             │
│  CLI (src/cli/)   TUI (src/tui/)   HTTP (src/http/)             │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│                  AgentRuntime  (src/runtime.rs)                 │
│  • Owns transcript  • Manages sessions  • Drives Kernel turns   │
│  • Handles compaction, stuck detection, budget counting         │
└────────────────────┬────────────────────────────────────────────┘
                     │ TurnContext
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              AgentKernel  (src/kernel.rs)                       │
│  Stateless single-turn executor: calls ChatProvider, dispatches │
│  tool calls, appends new messages, returns TurnOutcome          │
└────────────────────┬────────────────────────────────────────────┘
                     │
          ┌──────────┴──────────┐
          ▼                     ▼
┌──────────────────┐   ┌────────────────────────────────────────┐
│  ChatProvider    │   │  ToolRegistry  (src/tools/registry.rs) │
│  (src/llm/)      │   │  ~40 tools registered at startup        │
│  OpenAI-compat   │   │  Sandbox-enforced via resolve_within    │
│  Anthropic       │   └────────────────────────────────────────┘
└──────────────────┘
```

## Data Flow Per Turn

1. **AgentRuntime** prepares `TurnContext` from its transcript + config.
2. **AgentKernel** calls `ChatProvider::complete(messages, tools)`.
3. Provider returns a `Completion` with optional `tool_calls`.
4. Kernel dispatches each tool call via `ToolRegistry::invoke`.
5. Tool results appended as `Role::Tool` messages (must immediately follow
   the `Role::Assistant` message — see [Invariant #8](/architecture/invariants.md)).
6. Kernel loops until the model emits no more tool calls (`NoMoreToolCalls`)
   or a termination condition fires.
7. `TurnOutcome` returned to `AgentRuntime`, which merges new messages into
   transcript and persists to session store.

## Key Source Files

| Component | Location |
|-----------|----------|
| Agent loop types | `src/agent/types.rs` |
| Kernel | `src/kernel.rs` |
| AgentRuntime | `src/runtime.rs` |
| Config + system prompt assembly | `src/config.rs` |
| LLM traits + providers | `src/llm/` |
| Tool registry + all tools | `src/tools/` |
| Skills system | `src/skills.rs` |
| Session persistence | `src/session/` |
| Memory paths | `src/paths.rs` |
| Error types | `src/error.rs` |

## Related Concepts

- [Agent Loop](agent-loop.md) — detailed loop mechanics and finish reasons
- [Layer 0 Injection](layer0-injection.md) — how system prompt is assembled
- [Tools Overview](tools/index.md) — full tool catalog
- [Providers Overview](providers/index.md) — LLM backend options
- [Memory Overview](memory/index.md) — four-layer memory system
- [Invariants](invariants.md) — rules that must not be broken

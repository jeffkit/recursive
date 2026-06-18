---
type: Architecture
title: Multi-Agent Tools — AgentTool, send_message, teams
description: Tools for spawning sub-agents, message passing between agents, and team orchestration. Enabled when RECURSIVE_SUBAGENT_ENABLED=1.
tags: [tools, multi-agent, sub-agent, orchestration]
timestamp: 2026-06-18T10:00:00Z
---

# Multi-Agent Tools

## AgentTool (Sub-agent)

- **Rust struct**: `AgentTool`
- **Source**: `src/tools/agent.rs`
- **Enabled**: `RECURSIVE_SUBAGENT_ENABLED=1`
- **Purpose**: Dispatch a focused research/scan task to a fresh agent loop with restricted tools.

Key args: `task` (goal description), `tools` (allowed tool list), `model` (optional override).

Uses `SharedMemoryRead` / `SharedMemoryWrite` for optional memory sharing between parent and sub-agent.

## send_message / ListWorkers

- **Source**: `src/tools/send_message.rs`
- **Purpose**: Send a message to another named worker agent; list active workers.
- **Backed by**: `WorkerRegistry` + `WorkerMailbox` — an in-process message bus.

## Team Tools

- **Source**: `src/tools/team_create.rs`, `src/tools/team_delete.rs`
- **Purpose**: Create/delete a named team of agents that share a mailbox.

## Task Management

See [Task Tools](task-tools.md) for the full task lifecycle API
(`task_create`, `task_get`, `task_list`, etc.).

## A2A (Agent-to-Agent Protocol)

See [A2A Tools](a2a-tools.md) for cross-process agent communication via the
Google A2A protocol.

## Design Constraint

> New capabilities go in tools, not in the agent loop.
> Multi-agent features are implemented as tools that spawn new `AgentRuntime`
> instances — the parent loop never branches on agent type.
> (Invariant #1 — see [Invariants](../invariants.md))

## Related Concepts

- [Task Tools](task-tools.md) — task lifecycle
- [Agent Loop](../agent-loop.md) — how sub-agents run the same loop
- [Tools Overview](index.md)

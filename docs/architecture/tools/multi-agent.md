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
- **Purpose**: Spawn one or more specialist sub-agents (workers), each with its own agent loop,
  restricted tool set, and isolated transcript.

Key args:
- `mode` — `single` (one worker) / `parallel` (concurrent) / `sequential` (chained).
- `manifest` — map of `worker_id → { system_prompt, allowed_tools? }`. Each entry defines a
  specialist. `allowed_tools` omitted/empty = read-only set (Read, Grep, Glob, WebFetch,
  SearchFiles). An entry may instead (or additionally) set `definition` to reference a built-in
  role from `.recursive/agents/*.md` (e.g. `explore`, `plan`, `verification`,
  `general-purpose`).
- `prompt` — the self-contained task description delivered to every worker.
- `max_steps` — per-worker step cap (default 30, max 100).

Workers may optionally share memory via `SharedMemoryRead` / `SharedMemoryWrite` (registered on
the worker sub-registry when a pool is attached).

### Background workers & cross-turn continuation

With `background: true` (and `mode: "single"`), the worker is spawned into a long-lived tokio
task that drains an mpsc channel of turn prompts against a single retained `AgentRuntime`. The
`agent` tool returns a `task_id` immediately (instead of blocking until the worker finishes).

- **send_message(task_id=...)** pushes a new turn prompt onto the worker's channel; the worker
  runs it on the same runtime, preserving transcript/context (cross-turn continuation).
- **task_output(task_id, block=true)** waits event-driven (`tokio::sync::Notify`) rather than
  busy-polling, waking when the task reaches a terminal state.
- **task_stop(task_id)** aborts the worker's JoinHandle (real cancellation, not just a status
  flag), provided the handle was attached via `set_handle` at spawn time.

The continuation handle (`WorkerHandle`) is stored in a shared `WorkerTable`
(`Arc<RwLock<HashMap<worker_id, Arc<WorkerHandle>>>>`), wired through `register_subagent_if_enabled`
so the `agent` tool (spawner) and `send_message` tool (continuer) share one table per coordinator.

## send_message / ListWorkers

- **Source**: `src/tools/send_message.rs`
- **Purpose**: Send a message to another named worker agent; list active workers. For background
  workers, addressing by `task_id` drives a new turn (cross-turn continuation).
- **Backed by**: `WorkerRegistry` + `WorkerMailbox` (legacy, same-turn injection) and the
  `WorkerTable` of background `WorkerHandle`s (cross-turn continuation).

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

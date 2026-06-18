---
type: Architecture
title: Task Tools — task_create, task_get, task_list, task_update, task_stop, task_output
description: Structured task lifecycle management for multi-agent coordination. Tasks are units of work delegated to sub-agents.
tags: [tools, tasks, multi-agent]
timestamp: 2026-06-18T10:00:00Z
---

# Task Tools

| Tool | Source | Description |
|------|--------|-------------|
| `task_create` | `src/tools/task_create.rs` | Create a new task and assign to an agent |
| `task_get` | `src/tools/task_get.rs` | Get task status and metadata |
| `task_list` | `src/tools/task_list.rs` | List all tasks (optionally filtered by status) |
| `task_update` | `src/tools/task_update.rs` | Update task metadata or notes |
| `task_stop` | `src/tools/task_stop.rs` | Cancel a running task |
| `task_output` | `src/tools/task_output.rs` | Retrieve task output/result |

## Task Lifecycle

```
Created → Running → Completed
                 ↘ Failed
                 ↘ Cancelled (task_stop)
```

## Relationship to Multi-Agent

Tasks are the coordination primitive for [Multi-Agent Tools](multi-agent.md).
An orchestrator creates tasks and monitors them; worker agents pick up tasks
and update them.

## Related Concepts

- [Multi-Agent Tools](multi-agent.md) — AgentTool and message passing
- [A2A Tools](a2a-tools.md) — cross-process task checking
- [Tools Overview](index.md)

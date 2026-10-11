---
type: Architecture
title: A2A Tools — a2a_call, a2a_card, a2a_task_check
description: Agent-to-Agent (A2A) protocol tools for cross-process communication with remote agents that expose a standard A2A HTTP endpoint.
tags: [tools, a2a, multi-agent]
timestamp: 2026-06-18T10:00:00Z
---

# A2A Tools

Source: `src/tools/a2a.rs`

| Tool | Struct | Description |
|------|--------|-------------|
| `a2a_call` | `A2aCallTool` | Send a task to a remote A2A agent endpoint |
| `a2a_card` | `A2aCardTool` | Fetch an agent's capability card (what it can do) |
| `a2a_task_check` | `A2aTaskCheckTool` | Poll the status of a previously submitted A2A task |

## What Is A2A?

Google's Agent-to-Agent (A2A) protocol defines a standard HTTP interface for
agents to communicate across processes and machines. Any agent that exposes
the A2A endpoint can be called with `a2a_call`.

## Relationship to In-Process Multi-Agent

- [Multi-Agent Tools](multi-agent.md) handle **in-process** sub-agents (same binary).
- A2A tools handle **cross-process** remote agents.

## SSRF guard

All three tools share one HTTP client factory, and that client validates both the
endpoint URL and the address it resolves to before connecting (issue #169):
literal loopback / private / link-local hosts are rejected up front, and any DNS
answer containing a private address is refused, so a hostname that resolves into
the internal network (or rebinds to it) never gets a socket. Redirects are not
followed.

Deployments that must reach agents on internal addresses can opt out with
`RECURSIVE_SSRF_ALLOW_PRIVATE_DNS=1` (default off) — see
[Web Tools](web-tools.md).

## Related Concepts

- [Multi-Agent Tools](multi-agent.md)
- [Task Tools](task-tools.md)
- [Tools Overview](index.md)

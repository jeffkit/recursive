# Recursive ROADMAP v3 — From Platform to Product

> **Replaces**: ROADMAP v2 (v0.2 release ready, Phase 5-8 complete)
> **Status**: Active — approved via HITL discussion 2026-05-26
> **Version baseline**: v0.3.0 (published)

## Context

Recursive v0.3.0 delivered a complete platform: MCP server mode, config
file, CLI UX overhaul, feature flags, 433+ tests, crates.io published.

v0.4+ goal: **make it a product** — TUI, loop mode, HTTP API, multi-agent.

---

## Phase 10 — Loop Mode (Priority: Critical)

The agent needs to sustain itself across turns without user input.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 10.1 | AgentRunner — cross-turn wrapper | S | ✅ g87 |
| 10.2 | schedule_wakeup tool + WakeupSlot | S | 🔄 g88 (in progress) |
| 10.3 | Background shell complete → auto-trigger | S | 🔴 g89 |
| 10.4 | `recursive loop <goal>` CLI subcommand | S | 🔴 g90 |

**Total**: ~1-2 batches

---

## Phase 11 — TUI (Priority: High)

Interactive terminal UI, decoupled from agent core (orthogonal).
Reference: `~/Downloads/fake-cc` (Claude Code's Ink/React TUI).
Implementation: `ratatui` (Rust) in a separate crate `recursive-tui`.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 11.1 | TUI crate scaffold + basic REPL display | M | 🔴 |
| 11.2 | Streaming output + tool call indicators | M | 🔴 |
| 11.3 | Multi-turn conversation view | S | 🔴 |
| 11.4 | Logo + splash screen | S | 🔴 |
| 11.5 | Plan mode UI (approve/reject) | S | 🔴 |

**Total**: ~2-3 batches

---

## Phase 12 — HTTP API (Priority: High)

REST server exposing agent capabilities for external integration.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 12.1 | axum server scaffold + /tools endpoint | S | 🔴 |
| 12.2 | POST /run — one-shot execution | S | 🔴 |
| 12.3 | Sessions: create, message, list | M | 🔴 |
| 12.4 | SSE event streaming (GET /sessions/:id/events) | M | 🔴 |
| 12.5 | OpenAPI spec generation | S | 🔴 |
| 12.6 | Python SDK (thin client) | S | 🔴 |

**Total**: ~2-3 batches

---

## Phase 13 — Multi-Agent Framework (Priority: Medium)

Main agent dynamically designs a team of specialists for complex tasks.
Reference: fake-cc's multi-agent coordination.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 13.1 | Agent pool + role definitions | M | 🔴 |
| 13.2 | Shared project memory across agents | M | 🔴 |
| 13.3 | Inter-agent messaging bus | M | 🔴 |
| 13.4 | Dynamic team composition (main decides roles) | L | 🔴 |
| 13.5 | Pipeline mode (A → B → C) | S | 🔴 |

**Total**: ~3-4 batches

---

## Execution Order

```
Batch 30 (in progress): Phase 10 — g87 ✅, g88 🔄
Batch 31: Phase 10 finish — g89, g90
Batch 32+: Phase 11 (TUI) start
Batch 34+: Phase 12 (HTTP API) start
Batch 36+: Phase 13 (Multi-Agent) start
```

---

## Design Principles (carried from v1+v2)

1. Agent loop stays small — new capabilities are tools/providers/observers
2. Orthogonality — tools don't depend on LLM internals; TUI doesn't import core
3. Every feature gets real usage validation
4. No new dependencies without justification
5. TUI communicates via API, not direct library calls
6. Multi-agent shares memory, not transcript state

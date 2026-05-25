# Recursive ROADMAP v2 — From Kernel to Platform

> **Replaces**: original ROADMAP Phase 1-4 (100% complete as of batch 18)
> **Status**: Active — approved items from HITL discussion 2026-05-25

## Context

Recursive v0.1 delivered a complete agent kernel: ReAct loop, 10+ tools,
MCP client, Skills, Streaming, Compaction, Hooks, Transport abstraction,
Sub-agent, Memory, Session management, 286 tests.

v0.2 goal: **make it usable by others** — not just a self-improvement demo.

---

## Phase 5 — Skill System v2 (Priority: Critical)

The current skill system is "load markdown on demand". A usable skill
system needs progressive disclosure, reference docs, and executable scripts.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 5.1 | Skill refs/ — reference documents accessible via tool | S | 🔴 |
| 5.2 | Skill scripts/ — executable scripts the agent can invoke | S | 🔴 |
| 5.3 | Trigger-based progressive disclosure | M | 🔴 |
| 5.4 | Skill parameters (frontmatter args) | S | 🔴 |
| 5.5 | Skill composition (depends_on) | S | 🔴 |
| 5.6 | Injection modes: always / trigger / manual | S | 🔴 |

**Total**: ~2 batches (6 goals, mostly S)

---

## Phase 6 — MCP Maturity (Priority: High)

MCP is the ecosystem connector. Currently stdio-only with mock tests.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 6.1 | MCP HTTP+SSE transport (Streamable HTTP) | M | 🔴 |
| 6.2 | MCP real server integration test (filesystem server) | S | 🔴 |
| 6.3 | MCP Server mode (expose Recursive as an MCP server) | L | 🔴 |
| 6.4 | MCP resource/prompt support (beyond tools) | M | 🔴 |

**Total**: ~2 batches

---

## Phase 7 — Ship It (Priority: High)

Make the crate publishable and usable by external developers.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 7.1 | API stabilization + breaking change cleanup | M | 🔴 |
| 7.2 | docs.rs documentation (user-facing, not internal) | S | 🔴 |
| 7.3 | examples/ directory (5 runnable examples) | S | 🔴 |
| 7.4 | Feature flags (mcp, web_fetch, anthropic optional) | M | 🔴 |
| 7.5 | crates.io publish + CI release workflow | S | 🔴 |
| 7.6 | Error type refinement (structured errors for library) | S | 🔴 |

**Total**: ~2 batches

---

## Phase 8 — Capability (Priority: Medium)

Make the agent handle harder tasks.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 8.1 | Parallel tool execution | M | 🔴 |
| 8.2 | Tool Transport: SSH adapter | M | 🔴 |
| 8.3 | Background task execution (fire-and-forget + poll) | M | 🔴 |
| 8.4 | Multi-turn Planning (plan → confirm → execute) | L | 🔴 |

**Total**: ~2 batches

---

## Phase 9 — Ecosystem (Priority: Low, future)

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 9.1 | HTTP API / REST wrapper | M | 🔴 |
| 9.2 | Multi-Agent orchestration | L | 🔴 |
| 9.3 | Goal Queue + Loop Mode | M | ⏸️ pending discussion |
| 9.4 | Self-Orchestration (replace self-improve.sh) | L | ⏸️ pending discussion |

---

## Execution Order

```
Batch 19 (current): Validation — dogfood hooks, integration tests
Batch 20-21: Phase 5 (Skill v2) — 6 goals, 4-wide
Batch 22-23: Phase 6 (MCP) — 4 goals, 2-wide (complex)
Batch 24-25: Phase 7 (Ship) — 6 goals, 4-wide
Batch 26-27: Phase 8 (Capability) — 4 goals
Batch 28+: Phase 9 (Ecosystem) — as needed
```

**Estimated total: 8-10 more batches to v0.2 release.**

---

## Design Principles (carried from v1)

1. Agent loop stays small — new capabilities are tools/providers/observers
2. Orthogonality — tools don't depend on LLM internals
3. Every feature gets real usage validation (VALIDATION.md)
4. No new dependencies without justification

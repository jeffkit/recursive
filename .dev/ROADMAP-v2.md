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
| 5.1 | Skill refs/ — reference documents accessible via tool | S | ✅ g59 |
| 5.2 | Skill scripts/ — executable scripts the agent can invoke | S | ✅ g60 |
| 5.3 | Trigger-based progressive disclosure | M | 🔴 |
| 5.4 | Skill parameters (frontmatter args) | S | ✅ g61 |
| 5.5 | Skill composition (depends_on) | S | 🔴 |
| 5.6 | Injection modes: always / trigger / manual | S | ✅ g62 |

**Total**: ~2 batches (6 goals, mostly S)

---

## Phase 6 — MCP Maturity (Priority: High)

MCP is the ecosystem connector. Currently stdio-only with mock tests.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 6.1 | MCP HTTP+SSE transport (Streamable HTTP) | M | ✅ g65 |
| 6.2 | MCP real server integration test (filesystem server) | S | ✅ g66 |
| 6.3 | MCP Server mode (expose Recursive as an MCP server) | L | ⏸️ deferred (agent failed 2x) |
| 6.4 | MCP resource/prompt support (beyond tools) | M | ✅ g67 |

**Total**: ~2 batches

---

## Phase 7 — Ship It (Priority: High)

Make the crate publishable and usable by external developers.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 7.1 | API stabilization + breaking change cleanup | M | ✅ g69 (no changes needed) |
| 7.2 | docs.rs documentation (user-facing, not internal) | S | ✅ g73 |
| 7.3 | examples/ directory (5 runnable examples) | S | ✅ g72 |
| 7.4 | Feature flags (mcp, web_fetch, anthropic optional) | M | ✅ g70 |
| 7.5 | crates.io publish + CI release workflow | S | ✅ g75 |
| 7.6 | Error type refinement (structured errors for library) | S | ✅ g71 |

**Total**: ~2 batches

---

## Phase 8 — Capability (Priority: Medium)

Make the agent handle harder tasks.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 8.1 | Parallel tool execution | M | ✅ g74 |
| 8.2 | Tool Transport: SSH adapter | M | ✅ g76 |
| 8.3 | Background task execution (fire-and-forget + poll) | M | ✅ g77 |
| 8.4 | Multi-turn Planning (plan → confirm → execute) | L | ⏸️ deferred (budget exceeded) |

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
Batch 19 (done): Validation — dogfood hooks, integration tests
Batch 20 (done): Phase 5 partial — g59 refs, g60 scripts, g61 params, g62 injection modes
Batch 21 (done): Phase 5 finish + Phase 6.1 MCP HTTP+SSE
Batch 22 (done): Phase 6.2 integration test, 6.4 resources/prompts (6.3 server mode deferred)
Batch 23 (done): Phase 7 partial — g69 API audit, g70 feature flags, g71 errors, g72 examples
Batch 24 (done): Phase 7 finish + Phase 8 start — g73 docs, g74 parallel, g75 CI, g76 SSH
Batch 25 (done): Phase 8 — g77 background tasks ✅, g78 planning deferred
Batch 26+: Phase 9 (Ecosystem) — as needed
```

**v0.2 RELEASE READY.** Deferred items (6.3, 8.4) are stretch goals for v0.3.

---

## Design Principles (carried from v1)

1. Agent loop stays small — new capabilities are tools/providers/observers
2. Orthogonality — tools don't depend on LLM internals
3. Every feature gets real usage validation (VALIDATION.md)
4. No new dependencies without justification

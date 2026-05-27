# Recursive ROADMAP v4 — From Product to Ecosystem

> **Replaces**: ROADMAP v3 (v0.5 release, Phases 10-13 complete)
> **Status**: Draft — pending user review
> **Version baseline**: v0.5.0

## Context

Recursive v0.5.0 delivered a complete product stack:
- **Core**: ReAct loop, 10+ tools, MCP client/server, Skills, Streaming, Compaction, Hooks
- **Loop Mode**: Self-scheduling autonomous agent runs
- **HTTP API**: REST server with sessions, SSE streaming, OpenAPI spec
- **TUI**: Interactive terminal UI with plan mode
- **Multi-Agent**: Pool, shared memory, messaging bus, pipeline, team orchestrator
- **SDK**: Python client

v0.6+ goal: **make it an ecosystem** — plugins, persistence, observability, and production hardening.

---

## Phase 14 — Persistence & State (Priority: Critical)

Currently everything is in-memory. Production use requires durable state.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 14.1 | Session persistence (SQLite) | M | 🔴 |
| 14.2 | Memory persistence (SQLite) | S | 🔴 |
| 14.3 | Transcript export/import (JSON) | S | 🔴 |
| 14.4 | Agent checkpoint/resume | M | 🔴 |

**Total**: ~2 batches

---

## Phase 15 — Observability & Monitoring (Priority: High)

Understanding what agents do in production.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 15.1 | Structured logging (tracing spans per step) | S | 🔴 |
| 15.2 | Metrics endpoint (Prometheus-compatible) | M | 🔴 |
| 15.3 | Cost tracking dashboard (per-session, per-role) | M | 🔴 |
| 15.4 | Replay viewer (web-based transcript replay) | M | 🔴 |

**Total**: ~2 batches

---

## Phase 16 — Plugin System (Priority: High)

Allow third-party tool/provider extensions without forking.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 16.1 | Plugin trait + dynamic loading (dylib/.so) | L | 🔴 |
| 16.2 | Plugin manifest (TOML) + discovery | S | 🔴 |
| 16.3 | Plugin registry (list/install from URL) | M | 🔴 |
| 16.4 | Sandboxed plugin execution (WASM runtime) | L | 🔴 |

**Total**: ~3 batches

---

## Phase 17 — Production Hardening (Priority: High)

Make it safe and reliable for real deployments.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 17.1 | Rate limiting (per-session, per-API-key) | S | 🔴 |
| 17.2 | Authentication (API keys + JWT) | M | 🔴 |
| 17.3 | Tool permission system (role-based allow/deny) | M | 🔴 |
| 17.4 | Graceful shutdown + in-flight request draining | S | 🔴 |
| 17.5 | Docker packaging + health probes | S | 🔴 |

**Total**: ~2 batches

---

## Phase 18 — Advanced Agent Patterns (Priority: Medium)

Higher-level agent capabilities building on the multi-agent framework.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 18.1 | Self-reflection (agent reviews own output) | M | 🔴 |
| 18.2 | Tool learning (agent creates new tools from experience) | L | 🔴 |
| 18.3 | Hierarchical planning (recursive plan decomposition) | M | 🔴 |
| 18.4 | Consensus protocol (multiple agents vote on decisions) | M | 🔴 |
| 18.5 | Long-running goals (checkpoint + resume across restarts) | M | 🔴 |

**Total**: ~3-4 batches

---

## Phase 19 — Ecosystem & Distribution (Priority: Medium)

Make Recursive accessible to a wider audience.

| ID | Feature | Effort | Status |
|----|---------|--------|--------|
| 19.1 | TypeScript/Node.js SDK | M | 🔴 |
| 19.2 | CLI installer (curl | sh) | S | 🔴 |
| 19.3 | Homebrew formula | S | 🔴 |
| 19.4 | VS Code extension (agent in sidebar) | L | 🔴 |
| 19.5 | Documentation site (mdbook or similar) | M | 🔴 |

**Total**: ~3 batches

---

## Execution Order (Revised)

```
Batch 35: Phase 14 (Persistence + Memory) — JSONL sessions + 4-layer memory
           Goals: 107 (JSONL writer), 108 (wire agent), 109 (CLI commands)
                  110 (Layer 0 context), 111 (Layer 1 scratchpad),
                  112 (Layer 2 semantic), 113 (Layer 3 episodic)
Batch 36: Phase 15 (Observability) — logging, metrics, cost tracking
Batch 37: Phase 17 (Hardening) — auth, rate limiting, Docker
Batch 38: Phase 16.1-16.2 (Plugins) — trait + manifest
Batch 39: Phase 18 (Agent Patterns) — reflection, tool learning
Batch 40+: Phase 19 (Ecosystem) — SDKs, installers, docs site
```

---

## Design Principles (carried forward + new)

1. Agent loop stays small — new capabilities are tools/providers/observers
2. Orthogonality — tools don't depend on LLM internals; TUI doesn't import core
3. Every feature gets real usage validation
4. No new dependencies without justification
5. TUI communicates via API, not direct library calls
6. Multi-agent shares memory, not transcript state
7. **NEW**: Plugins are isolated — they cannot corrupt core state
8. **NEW**: Persistence is opt-in — in-memory remains the zero-config default
9. **NEW**: Authentication is middleware — core logic is auth-unaware
10. **NEW**: Every feature must work with `cargo test` (no external services required for tests)

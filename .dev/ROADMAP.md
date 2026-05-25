# Recursive Feature Roadmap

Based on competitive analysis of Claude Code, Codex, OpenHands, Hermes Agent,
Qwen Code, Trae Agent, and Open SWE.

## Current State

Recursive is a ~2K LOC Rust kernel with five concepts: `Message`,
`LlmProvider`, `Tool`+`ToolRegistry`, `Agent` loop, and `StepEvent` stream.
It already has:

- ReAct loop with step budget, stuck detection, transcript trimming
- OpenAI-compatible HTTP provider with retry/backoff
- 6 tools: `read_file`, `write_file`, `list_dir`, `run_shell`, `apply_patch`,
  `search_files`
- Path sandboxing (`resolve_within`), shell timeout
- Event stream for observers, transcript persistence, token usage tracking,
  cost estimation
- Builder pattern API, embeddable as a library

**Core differentiator**: minimal, orthogonal, embeddable kernel in Rust. No
product in this survey is both a library and a CLI at this small a footprint.
The roadmap must preserve this.

## Design Principle

Every feature below is proposed as either a **new `Tool`**, a **new
`LlmProvider` adapter**, or a **new observer of `StepEvent`** — never as a
branch inside `agent.rs`. This is the invariant that keeps the kernel tiny.

---

## Phase 1 — Kernel Essentials (Near Term)

These close the gap between "interesting demo" and "reliable workhorse for
real tasks". Every competitor has them.

### 1.1 Context Compaction

**Why**: The current `maybe_trim_transcript` replaces old tool outputs with a
placeholder — a lossy, character-level heuristic. Every major competitor
(Claude Code, Codex, Hermes) uses LLM-driven summarisation to compress history
while preserving key decisions and findings. Without this, Recursive fails on
any task exceeding one context window.

**What**: Add a `compact` method to `Agent` that:

1. Detects when prompt token count approaches a configurable fraction of the
   model's context limit (e.g. 80%).
2. Calls the LLM with the older portion of the transcript and a meta-prompt:
   "Summarise the conversation so far, preserving key decisions, file paths
   modified, and test results."
3. Replaces the older messages with a single `Role::System` or `Role::User`
   summary message.
4. Emits a `StepEvent::Compacted { removed, summary_chars }` event.

**Scope**: ~200 LOC in `agent.rs` + a new `StepEvent` variant. No new
dependencies.

### 1.2 Project Context File (AGENTS.md / RECURSIVE.md)

**Why**: Claude Code has `CLAUDE.md`, Codex has `AGENTS.md`, OpenHands has
`.openhands/microagents/repo.md`, Hermes reads `MEMORY.md`. These give the
agent project-specific knowledge at zero runtime cost (loaded once per
session). Without it, users must paste project conventions into every prompt.

**What**:

1. At agent start, walk from the workspace root upward looking for
   `RECURSIVE.md`, `AGENTS.md`, or `CLAUDE.md` (in that precedence).
2. Prepend the file content to the system prompt (after the built-in prompt,
   before the user goal).
3. Config env var `RECURSIVE_PROJECT_DOC_MAX_BYTES` (default 32 KiB) caps the
   injection size.

**Scope**: ~50 LOC in `config.rs` or a new `context.rs`. Zero new
dependencies.

### 1.3 Streaming Completions

**Why**: All competitors stream token-by-token. Current
`OpenAiProvider::complete` waits for the full response, which feels
unresponsive on large outputs and prevents early cancellation.

**What**: Add an optional `stream` method to `LlmProvider` with a default
implementation that falls back to `complete`. The `OpenAiProvider` adapter
implements true SSE streaming, yielding partial tokens through a channel. The
agent loop can forward these as `StepEvent::PartialToken { text, step }`
events.

**Scope**: ~150 LOC in `llm/openai.rs`, new `StepEvent` variant, opt-in for
CLI rendering.

### 1.4 Count Lines / Token Estimator Tool

**Why**: Agents need to estimate whether a file fits in context before reading
it. Claude Code, Codex, and Hermes all have this.

**What**: A `count_lines` tool that returns line count and approximate token
count (chars/4 heuristic) for one or more paths. Cheap to implement, prevents
the agent from blindly reading 50K-line files.

**Scope**: ~60 LOC as `src/tools/count.rs`.

---

## Phase 2 — Ecosystem Connectors (Short Term)

These connect Recursive to the standard infrastructure that serious users
expect.

### 2.1 MCP Client (Model Context Protocol)

**Why**: MCP is the de facto standard for connecting agents to external
services. Claude Code, Codex, OpenHands, Hermes, Qwen Code, Trae Agent all
support it. Without MCP, Recursive is limited to its built-in tools.

**What**: Implement a minimal MCP stdio client that:

1. Reads an `.mcp.json` or `mcp_servers` config to discover MCP servers.
2. Spawns each server as a child process, exchanges JSON-RPC over stdio.
3. Registers each server's tools as `Tool` instances in the `ToolRegistry` —
   the agent loop requires zero changes.

**Scope**: ~400 LOC as `src/mcp/` module. Depends on `tokio::process` (already
in deps) + JSON-RPC framing.

### 2.2 Web Fetch Tool

**Why**: Every competitor has web access. For documentation lookup, error
investigation, and API reference, the agent needs to reach the web.

**What**: A `web_fetch` tool that takes a URL, fetches the page, and returns
the text content (HTML stripped to markdown or plain text). Configurable
timeout and max body size.

**Scope**: ~100 LOC as `src/tools/web.rs`. Uses `reqwest` (already in deps).

### 2.3 Multi-Provider Support

**Why**: Users want to switch between OpenAI, Anthropic, Gemini, DeepSeek,
local Ollama — ideally with a config change. The current `OpenAiProvider`
handles any OpenAI-compatible API (which covers most), but Anthropic's Messages
API is structurally different.

**What**: Add an `AnthropicProvider` adapter in `src/llm/anthropic.rs`
implementing `LlmProvider`. Same trait, different wire format. Optionally, a
`ProviderRouter` that picks based on model name prefix (`claude-*` → Anthropic,
everything else → OpenAI-compatible).

**Scope**: ~300 LOC for the adapter. The agent loop is untouched.

---

## Phase 3 — Agent Intelligence (Medium Term)

These make the agent substantially smarter.

### 3.1 Sub-Agent / Delegation

**Why**: Claude Code, Codex, OpenHands, and Hermes all support spawning child
agents for parallel or specialised work. Keeps the main context clean.

**What**: A `delegate` tool that:

1. Spawns a new `Agent` instance (same `LlmProvider`, scoped `ToolRegistry`,
   fresh transcript).
2. Runs a sub-goal to completion.
3. Returns only the final summary to the parent transcript (not the full
   sub-agent history).
4. Emits `StepEvent::SubAgentStarted` / `SubAgentFinished` events.

This is where Recursive's library nature shines — sub-agents are just
`Agent::builder().build()?.run()` calls.

**Scope**: ~150 LOC as `src/tools/delegate.rs`.

### 3.2 Persistent Memory (MEMORY.md)

**Why**: Hermes Agent's strongest differentiator is its learning loop. Claude
Code has auto-memory. Without any persistence, the agent re-discovers project
conventions every session.

**What**:

1. A `memory_read` tool that reads `~/.recursive/memory/{project_hash}.md`.
2. A `memory_write` tool that appends structured entries (key-value or
   freeform).
3. At agent start, inject the memory file content as additional system context
   (similar to 1.2).
4. The agent decides what to remember — no automatic extraction initially.

**Scope**: ~120 LOC as `src/tools/memory.rs` + startup injection logic.

### 3.3 Skill / Command System

**Why**: Claude Code, Codex, and Hermes all support markdown-based skill files
that inject domain knowledge on demand. This extends the agent's capabilities
without editing the kernel.

**What**:

1. A `.recursive/skills/` directory (user-level) and project-level
   `.recursive/skills/`.
2. Each skill is a `SKILL.md` with optional YAML frontmatter (`name`,
   `description`, `trigger`).
3. A `load_skill` tool that reads a skill file and injects its content into the
   current context.
4. The agent's system prompt includes a skill index (names + descriptions) so
   it can self-select relevant skills.

**Scope**: ~200 LOC as `src/skills.rs` + a `load_skill` tool.

### 3.4 Permission / Approval Hooks

**Why**: Claude Code, Codex, and Hermes all support approval workflows —
auto-approve safe reads, prompt for writes/shell. Important for unattended or
CI use.

**What**: Add a `ToolPolicy` enum (`Allow`, `AskUser`, `Deny`) and a
`PolicyFn` callback on `AgentBuilder`. Before each tool invocation, the agent
checks the policy. In CLI mode, `AskUser` prints the tool call and waits for
`y/n`. In library mode, the caller provides the callback.

**Scope**: ~100 LOC in `agent.rs` + builder extension. No new files needed.

---

## Phase 4 — Production Readiness (Longer Term)

### 4.1 Docker Sandbox Backend

Alternative to path-based sandboxing: run all tool execution inside a Docker
container. Implement as a `DockerToolRegistry` wrapper that proxies `execute()`
calls into a container via `docker exec`.

### 4.2 Session Management

Resume interrupted runs. Save agent state (transcript + step count + metadata)
to `~/.recursive/sessions/`. CLI gets `--continue` and `--resume` flags.

### 4.3 Structured Output / JSON Mode

Request JSON-mode from the LLM for tool calls on models that support it,
reducing parse failures. Add `response_format` support to the
`OpenAiProvider`.

### 4.4 Hooks / Lifecycle Events

Pre/post tool-call hooks (e.g. auto-format after file edits, auto-lint after
writes). Configurable via a `hooks.toml` or programmatic API.

### 4.5 OpenTelemetry / Observability

Export `StepEvent` stream as OpenTelemetry spans for production monitoring. The
event stream architecture makes this a pure observer — no kernel changes.

---

## Dev Loop — Cost & Provider Observability

These are **meta-goals for the self-improve orchestrator**, not shipping-product
features. They improve how we measure and compare provider runs (MiniMax,
DeepSeek Flash/Pro, GLM). Pick them when the loop needs better cost signal —
especially after DeepSeek V4 model-ID migration and Flash→Pro fallback.

### D.1 External Pricing Table

**Why**: Goal 06 hard-coded per-model USD rates in `pricing_for()` inside
`src/llm/mod.rs`. That was fine for plumbing, but prices drift (V4 Flash/Pro,
cache tiers) and every update requires a product commit. The price table is
**orchestrator configuration**, not kernel logic.

**What**:

1. Add `.dev/pricing.yaml` (model name → input/output per-million USD; optional
   cache-hit rate).
2. CLI / `observe.sh` loads it at runtime; unknown models print usage without
   `cost:` (same as today).
3. Slim `pricing_for()` in the library: either remove the hardcoded table or
   keep a tiny fallback for tests only.

**Scope**: `.dev/pricing.yaml`, `src/main.rs` (or a `.dev/scripts/` helper),
`.dev/scripts/observe.sh`, `.dev/OPERATIONS.md`. Touches `src/llm/mod.rs` only
to delete or delegate the match table.

**Design principle check**: dev-infra / observer — not a branch in `agent.rs`.

### D.2 Cache-Aware Cost Estimation

**Why**: Goal 21 added `cache_hit_tokens` / `cache_miss_tokens` visibility, but
`ModelPricing::cost_usd()` still bills all prompt tokens at the miss rate.
DeepSeek runs often show 95%+ cache hit; the printed `cost:` line over-estimates
by ~10× vs actual billing.

**What**:

1. Extend cost calculation: `cache_hit` at discounted input rate, `cache_miss`
   (or `prompt_tokens - cache_hit`) at full input rate.
2. Read cache-hit pricing from `.dev/pricing.yaml` (pairs with D.1).
3. Regression tests for mixed hit/miss usage.

**Scope**: ~50 LOC in cost helper + tests. Depends on D.1 or inline YAML rates.

**Design principle check**: observer / CLI summary — kernel already exposes
`TokenUsage`; no agent-loop change.

### D.3 DeepSeek V4 Model-ID Cleanup (optional follow-up)

**Why**: `self-improve.sh` now uses `deepseek-v4-flash` / `deepseek-v4-pro`;
legacy `deepseek-chat` alias retires 2026-07-24. Any remaining docs, journals,
and hardcoded model strings should align before the API cutoff.

**What**: grep for `deepseek-chat`, update OPERATIONS/INDEX references, drop
deprecated alias once all runs use explicit V4 IDs.

**Scope**: dev-infra chore; no product behaviour change.

---

## Priority Matrix

Status legend: **✅ landed** | **🟡 in-batch-N** | **🔴 not started** | **⏸️ deferred**

| ID    | Feature              | Effort | Impact   | Orthogonality       | Status         |
|-------|----------------------|--------|----------|---------------------|----------------|
| 1.1   | Context Compaction   | M      | Critical | ✅ agent.rs only     | ✅ landed `e63eb63` (g31 deepseek, batch-12) |
| 1.2   | Project Context File | S      | High     | ✅ config/context    | ✅ landed `2dbe297` (g36 minimax, batch-13) |
| 1.3   | Streaming            | M      | High     | ✅ LlmProvider trait | ✅ landed `92d257e` (g32 deepseek, batch-12; startup-panic regression fixed in `c5b2b8d`) |
| 1.4   | estimate_tokens Tool | S      | Medium   | ✅ new Tool          | ✅ landed `0357e1f` (g39 minimax, batch-14; **Phase 1 complete**) |
| 2.1   | MCP Client           | L      | Critical | ✅ new Tool source   | ✅ landed `8792131` (g35 deepseek, batch-13; the headline) |
| 2.2   | Web Fetch            | S      | High     | ✅ new Tool          | ✅ landed `13df912` (g37 minimax, batch-13) |
| 2.3   | Anthropic Provider   | M      | High     | ✅ new LlmProvider   | ✅ landed `44cec95` (g34 minimax, batch-12; MiniMax + DeepSeek both expose Anthropic-compatible endpoints — free testing surface) |
| 3.1   | Sub-Agent            | M      | High     | ✅ new Tool          | ✅ landed `bd01835` (g40 deepseek, batch-14; recursive primitive, default-off via `RECURSIVE_SUBAGENT_ENABLED`) |
| 3.2   | Persistent Memory    | S      | Medium   | ✅ new Tool+startup  | ✅ landed `15249ef` (g38 deepseek, batch-13) |
| 3.3   | Skill System         | M      | Medium   | ✅ new Tool+index    | ✅ landed `efef2cc` (g33 minimax→manual, batch-12; auto-resume infra bug surfaced — see Phase 0 follow-ups) |
| 3.4   | Permission Hooks     | S      | High     | ✅ builder callback  | ✅ landed `31dc682` (g43 deepseek, batch-15; **Phase 3 complete**) |
| 4.1   | Docker Sandbox       | L      | Medium   | ✅ wrapper           | 🔴 not started |
| 4.2   | Session Management   | M      | Medium   | ✅ persistence layer | ✅ landed `eb5bffe` (g45 deepseek, batch-15) |
| 4.3   | Structured Output    | S      | Medium   | ✅ LlmProvider       | ✅ landed `477e689` (g41 deepseek, batch-14); first consumer wired `20b0164` (g46 deepseek, batch-15 — Compactor uses structured JSON) |
| 4.4   | Hooks                | M      | Medium   | ✅ observer pattern  | ✅ landed `52c0433` (g48 deepseek, batch-16) |
| 4.5   | OpenTelemetry        | S      | Low      | ✅ observer pattern  | ✅ landed `2df8fc4` (g42 minimax, batch-14; spans only, no exporter) |
| D.1   | External Pricing Table | S    | Medium   | ✅ dev-infra / CLI   | 🔴 not started — move rates out of `mod.rs` → `.dev/pricing.yaml` |
| D.2   | Cache-Aware Cost     | S      | High     | ✅ cost helper only  | ✅ landed `2dac43d` (g49 minimax, batch-16) |
| D.3   | DeepSeek V4 ID Cleanup | S    | Low      | ✅ dev-infra chore   | 🔴 not started — post Flash/Pro fallback; before 2026-07-24 alias retirement |

S = small (~1 day), M = medium (~2-3 days), L = large (~1 week)

## Phase 0 — Prep work (already landed, not in original roadmap)

Goals 04-30 (kernel tightening) all completed pre-roadmap. They are
the foundation Phase 1+ builds on. Highlights:

- 04 TokenUsage tracking
- 05 apply_patch unified-diff tolerance
- 06 cost estimation (hardcoded `pricing_for` in `src/llm/mod.rs` — see **D.1/D.2** for externalising + cache-aware follow-up)
- 07-09 transcript persistence + replay (head / tail / resume / diff)
- 10-13, 16, 26-29 tool refinements (shell cwd/timeout/env/stdin,
  search regex/case, read_file range, kill count_lines, ...)
- 14 JSON event output
- 15, 23 retry policy + shell timeout env vars
- 18 default system prompt dogfood
- 19 transcript budget trim (precursor to 1.1 Context Compaction)
- 20-22 apply_patch nicer errors + dry-run mode
- 24 per-step latency tracking
- 25 apply_patch dry-run
- 30 OpenAI errors include model name

Total ~140 tests, ~$3.50 cumulative LLM spend.
**Reference baseline before Phase 1 begins: commit `4c4cb48` on main.**

## Goal-file Convention

Every new goal file under `.dev/goals/` **MUST** include a header block:

```
**Roadmap**: <id> — <feature> | dev-infra | chore

**Design principle check**:
  Implemented as: [new Tool | new LlmProvider | new StepEvent observer |
                   system prompt source]
  ❌ Does NOT branch inside agent.rs main loop
```

This is the contract that keeps the kernel orthogonal as features pile
on. Goals lacking this header will not be launched by the orchestrator.

---

## What NOT to Build

To preserve Recursive's identity as a minimal embeddable kernel:

- **No built-in IDE integration** — that's a consumer of the library, not part
  of it.
- **No messaging gateway** (Telegram/Slack/Discord) — same: build it on top.
- **No web UI** — provide the event stream, let consumers render.
- **No plugin marketplace** — skills as files are sufficient.
- **No user-modeling / dialectic profiling** — Hermes's territory, over-complex
  for a kernel.

The kernel exposes primitives. Products are built on top.

# Proposal: Agent Run Kernel vs Multi-Application Scenarios

> **Status**: Draft — pending discussion
> **Created**: 2026-05-27
> **Context**: v0.5.0 shipped HTTP Server, TUI, Multi-Agent. Need to validate whether the Agent Run core architecture properly supports diverse application scenarios.

## Problem Statement

Recursive's Agent (ReAct loop) is currently consumed by 4 different callers:

```
                    ┌─── CLI (main.rs) ─── one-shot / loop mode
                    │
Agent::run()  ──────┼─── HTTP Server (http.rs) ─── long-lived sessions, SSE
                    │
                    ├─── TUI (recursive-tui) ─── interactive multi-turn
                    │
                    └─── Multi-Agent (multi.rs) ─── orchestrated sub-agents
```

Each scenario has fundamentally different lifecycle, concurrency, and streaming requirements. We need to ensure the core abstraction doesn't become a "God object" that tries to serve everyone poorly.

---

## Current Architecture (as of v0.5.0)

```rust
// Core agent — owns the ReAct loop
pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    system_prompt: String,
    max_steps: usize,
    transcript: Vec<Message>,  // mutable state lives HERE
    // ...
}

// Runner — wraps Agent for multi-turn with transcript preservation
pub struct Runner {
    agent: Agent,
    // ...
}

// HTTP — owns sessions as HashMap<String, SessionState>
pub struct AppState {
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    // ...
}
```

### Observations

1. **Agent owns mutable transcript** — This couples lifetime of conversation state to the Agent struct itself. Different callers manage this differently (Runner preserves across turns, HTTP clones into SessionState, Multi-Agent uses fresh agents per stage).

2. **No unified streaming interface** — CLI uses `EventSender` (channel), HTTP uses SSE broadcast, TUI polls events. Three different patterns for the same underlying need.

3. **Session identity is caller-defined** — CLI has no session concept, HTTP uses UUID strings, TUI is implicitly single-session, Multi-Agent uses role IDs.

4. **Memory access varies** — CLI injects AGENTS.md at startup, Multi-Agent injects shared memory per-turn, HTTP has no memory integration yet.

---

## Questions to Resolve

### Q1: Should Agent own the transcript?

**Current**: `Agent` holds `transcript: Vec<Message>` and mutates it during `run()`.

**Alternative**: Agent is stateless; caller passes transcript in, gets transcript + outcome out.

```rust
// Option A: Current (stateful Agent)
let outcome = agent.run(goal, &event_tx).await;
// transcript lives inside agent, caller uses agent.set_transcript() / runner

// Option B: Stateless Agent (functional style)
let outcome = agent.run(transcript, goal, &event_tx).await;
// returns AgentOutcome { transcript, final_message, ... }
// caller owns transcript lifecycle entirely
```

**Trade-offs**:
- Option A: Simpler for single-session CLI. Awkward for HTTP (need to extract/restore transcript per request).
- Option B: Cleaner separation. Agent becomes a pure "step executor". All callers manage state uniformly.

### Q2: Should there be a Session abstraction in core?

Currently, "session" only exists in HTTP. But all callers need:
- Transcript persistence
- Memory scope
- Identity (for logging, cost tracking)
- Resume capability

Proposal: Extract a `Session` struct into core:

```rust
pub struct Session {
    pub id: String,
    pub transcript: Vec<Message>,
    pub memory: MemoryScope,
    pub metadata: SessionMetadata,  // created_at, model, cost, etc.
}

impl Session {
    pub fn append(&mut self, msg: Message) { ... }
    pub fn persist(&self, writer: &dyn SessionWriter) { ... }
    pub fn load(reader: &dyn SessionReader, id: &str) -> Result<Self> { ... }
}
```

### Q3: Unified Event/Streaming interface?

All callers want real-time events from the agent loop. Current fragmentation:

| Caller | Mechanism | Event types |
|--------|-----------|-------------|
| CLI | `mpsc::Sender<Event>` | StepStart, ToolCall, ToolResult, Done |
| HTTP | `broadcast::Sender<SseEvent>` | Same + session lifecycle |
| TUI | Polls from `mpsc::Receiver` | Same |
| Multi | Internal, not exposed | Pipeline progress |

Proposal: Single `EventStream` trait:

```rust
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

// Implementations:
// - ChannelSink (CLI/TUI)
// - BroadcastSink (HTTP SSE)
// - NullSink (tests, background)
// - CompositeSink (multi-agent forwards to parent)
```

### Q4: Agent lifecycle — ephemeral vs long-lived?

| Scenario | Lifecycle | Transcript size | Concurrency |
|----------|-----------|-----------------|-------------|
| CLI one-shot | Create → run → drop | Small (one goal) | Single |
| CLI loop | Create → run → sleep → run → ... | Growing | Single |
| HTTP session | Create → run → idle → run → ... | Growing, needs persistence | Many concurrent sessions |
| TUI | Create → multi-turn → exit | Growing | Single + background tasks |
| Multi-Agent | Create N agents → orchestrate → drop | Per-stage, fresh | Parallel within pipeline |

Key insight: **Agent should be cheap to create** and **Session should be the long-lived entity**.

### Q5: Multi-Agent memory isolation boundary?

Current `multi.rs` uses `SharedMemory` (in-memory HashMap). Questions:
- Does each sub-agent get its own session?
- If a sub-agent writes to memory, when do others see it?
- Should the orchestrator's transcript include sub-agent transcripts?

---

## Proposed Target Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Application Layer                         │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌──────────────────┐  │
│  │   CLI   │  │  HTTP   │  │   TUI   │  │  Multi-Agent     │  │
│  └────┬────┘  └────┬────┘  └────┬────┘  └────────┬─────────┘  │
│       │             │            │                 │             │
├───────┼─────────────┼────────────┼─────────────────┼─────────────┤
│       │        Session Layer     │                 │             │
│       │    ┌─────────────────────────────────┐     │             │
│       └────┤  Session (id, transcript, meta) ├─────┘             │
│            │  + MemoryScope                  │                   │
│            │  + EventSink                    │                   │
│            │  + Persistence (JSONL writer)   │                   │
│            └──────────────┬──────────────────┘                   │
│                           │                                      │
├───────────────────────────┼──────────────────────────────────────┤
│                      Agent Layer                                  │
│            ┌──────────────┴──────────────────┐                   │
│            │  Agent (stateless ReAct engine)  │                   │
│            │  fn run(&self, session, goal)    │                   │
│            │  → AgentOutcome                  │                   │
│            └──────────────┬──────────────────┘                   │
│                           │                                      │
├───────────────────────────┼──────────────────────────────────────┤
│                    Infrastructure Layer                           │
│  ┌────────────┐  ┌───────┴──────┐  ┌─────────────┐             │
│  │ LLM Provider│  │ Tool Registry │  │ Memory Store │             │
│  └────────────┘  └──────────────┘  └─────────────┘             │
└─────────────────────────────────────────────────────────────────┘
```

### Key Changes from Current

1. **Agent becomes stateless** — no transcript ownership. Pure function: `(session_state, goal) → outcome`.
2. **Session is a first-class core concept** — not just an HTTP thing.
3. **EventSink is a trait** — callers inject their preferred streaming mechanism.
4. **Memory is scoped per-session** — with inheritance from workspace/global.
5. **Persistence is a Session concern** — JSONL writer attached to Session, not to Agent.

---

## Migration Path

```
Step 1: Extract Session struct into src/session.rs (expand current file)
Step 2: Make Agent::run() accept &mut Session instead of internal transcript
Step 3: Unify EventSink trait, adapt CLI/HTTP/TUI
Step 4: Move persistence (JSONL) into Session
Step 5: Wire Memory into Session scope
Step 6: Refactor Multi-Agent to use Session per sub-agent
```

---

## Open Questions for Discussion

1. Should `Session` own the `EventSink`, or should it be passed separately to `Agent::run()`?
2. For Multi-Agent: one parent Session that nests child Sessions? Or flat?
3. TUI's plan mode — does it need a separate "draft session" before committing?
4. Should Agent be `Clone`-able (allow spawning identical agents for parallel work)?
5. HTTP server restart: what's the contract for session resumption? (warm cache vs cold reload)

---

## References

- Current: `src/agent.rs`, `src/runner.rs`, `src/session.rs`, `src/http.rs`, `crates/recursive-tui/src/main.rs`, `src/multi.rs`
- Claude Code: Conversation stored as JSONL per session, indexed by project
- Related ROADMAP items: Phase 14 (Persistence), Phase 15 (Observability)

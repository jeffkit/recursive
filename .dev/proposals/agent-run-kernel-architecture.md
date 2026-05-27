# Proposal: Three-Layer Agent Architecture — Kernel / Wrapper / Interface

> **Status**: Draft v2 — incorporating discussion 2026-05-27
> **Created**: 2026-05-27
> **Context**: v0.5.0 shipped HTTP Server, TUI, Multi-Agent. Session persistence (JSONL) and 4-layer memory already implemented. Need to refactor the core to properly separate concerns across application scenarios.

---

## Design Philosophy

The core insight: **Agent Run is a single-turn execution kernel.** It receives a prepared context, executes one cycle of (think → tool calls → think → ... → final output), and returns. It does not know about sessions, multi-turn conversations, background processes, timers, or persistence.

Everything else — session management, compaction, scheduling, monitoring, background jobs — belongs to a **Wrapper** layer that orchestrates the Kernel across turns and manages long-lived state.

Users interact through an **Interface** layer (HTTP, TUI, CLI) that adapts the Wrapper's capabilities to a specific interaction protocol.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Interface Layer                                   │
│                                                                       │
│  ┌──────────┐     ┌──────────┐     ┌──────────┐                    │
│  │   CLI    │     │   HTTP   │     │   TUI    │                    │
│  │(one-shot)│     │(REST+SSE)│     │(terminal)│                    │
│  └────┬─────┘     └────┬─────┘     └────┬─────┘                    │
│       │                 │                 │                           │
│  Responsibility: interaction protocol adaptation                     │
│  - Parse user input into messages                                    │
│  - Render agent output in appropriate format                         │
│  - Handle transport concerns (HTTP routing, SSE, terminal I/O)       │
│  - Authentication, rate limiting (HTTP-specific)                     │
└───────┼─────────────────┼─────────────────┼─────────────────────────┘
        │                 │                 │
        ▼                 ▼                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     Wrapper Layer (Runtime Container)                 │
│                                                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │  Session                                                     │    │
│  │  - id, transcript, metadata                                  │    │
│  │  - multi-turn conversation management                        │    │
│  │  - context preparation (compaction, windowing)               │    │
│  │  - persistence (JSONL append-only)                           │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                       │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐      │
│  │ Background   │  │  Scheduler   │  │   Memory Scope       │      │
│  │ Job Manager  │  │  (Loop/Cron) │  │   (4-layer system)   │      │
│  └──────────────┘  └──────────────┘  └──────────────────────┘      │
│                                                                       │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐      │
│  │ Event Bus    │  │ Cost Tracker │  │   Hook Registry      │      │
│  │ (observe)    │  │ (accounting) │  │   (lifecycle hooks)  │      │
│  └──────────────┘  └──────────────┘  └──────────────────────┘      │
│                                                                       │
│  Responsibility: stateful runtime management                         │
│  - Own and accumulate transcript across turns                        │
│  - Prepare context for each Kernel invocation (inject memory,        │
│    apply compaction, enforce budget)                                  │
│  - Manage background shell processes spawned by tools                │
│  - Schedule future Kernel invocations (loop mode, timers)            │
│  - Persist messages to JSONL, track costs, emit events               │
│  - Lifecycle hooks (SessionStart, PreTurn, PostTurn, SessionEnd)     │
└───────────────────────────────┬─────────────────────────────────────┘
                                │
                                │ calls per turn
                                ▼
┌─────────────────────────────────────────────────────────────────────┐
│                     Kernel Layer (Agent Run)                          │
│                                                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │  fn run(&self, ctx: TurnContext) -> TurnOutcome              │    │
│  │                                                               │    │
│  │  Loop:                                                        │    │
│  │    1. Send context to LLM                                     │    │
│  │    2. Parse response (text / tool calls)                      │    │
│  │    3. If tool calls → execute → append results → goto 1      │    │
│  │    4. If final text → return outcome                          │    │
│  │    5. If budget exceeded → return with reason                 │    │
│  │                                                               │    │
│  │  Anti-stuck detection (consecutive identical failing calls)   │    │
│  │  Intra-turn budget enforcement (step limit, context growth)   │    │
│  │  Tool result trimming (local optimization within budget)      │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                                                                       │
│  Dependencies (injected, not owned):                                 │
│  - LLM Provider (Arc<dyn LlmProvider>)                              │
│  - Tool Registry (ToolRegistry)                                      │
│  - Event Sink (dyn EventSink — for real-time observation)           │
│                                                                       │
│  Responsibility: single-turn ReAct execution                         │
│  - Execute ONE turn: context in → outcome out                        │
│  - No knowledge of sessions, persistence, multi-turn                 │
│  - No knowledge of background jobs or timers                         │
│  - Stateless: can be shared, cloned, called concurrently             │
└─────────────────────────────────────────────────────────────────────┘
        │
        ▼ uses
┌─────────────────────────────────────────────────────────────────────┐
│                     Infrastructure Layer                              │
│  ┌────────────┐  ┌──────────────┐  ┌─────────────┐                 │
│  │LLM Provider│  │Tool Registry │  │  EventSink  │                 │
│  │(OpenAI,    │  │(read_file,   │  │(channel,    │                 │
│  │ DeepSeek,  │  │ shell, etc.) │  │ broadcast,  │                 │
│  │ Anthropic) │  │              │  │ null, log)  │                 │
│  └────────────┘  └──────────────┘  └─────────────┘                 │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Kernel Layer Detail

### Core Contract

```rust
/// The Agent Kernel — a stateless, single-turn ReAct executor.
///
/// Cheap to create, safe to clone, safe to share across threads.
/// Does not own transcript, session, or any cross-turn state.
#[derive(Clone)]
pub struct AgentKernel {
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    max_steps: usize,
    max_context_growth: Option<usize>,  // intra-turn budget
}

/// Everything the Kernel needs to execute one turn.
pub struct TurnContext {
    /// The prepared message history (system prompt + compacted transcript
    /// + new user message). Owned by caller, passed by value.
    pub messages: Vec<Message>,

    /// Where to emit real-time events (tool calls, progress, etc.)
    pub event_sink: Box<dyn EventSink>,

    /// Tool specs to advertise to the LLM (subset filtering possible)
    pub tool_specs: Vec<ToolSpec>,

    /// Whether to stream LLM responses token-by-token
    pub streaming: bool,

    /// Optional permission hook for tool-call gating
    pub permission_hook: Option<PermissionHook>,

    /// Planning mode (immediate execution vs plan-then-confirm)
    pub planning_mode: PlanningMode,
}

/// The result of a single turn execution.
pub struct TurnOutcome {
    /// All messages produced during this turn (assistant + tool results).
    /// The Wrapper appends these to its transcript.
    pub new_messages: Vec<Message>,

    /// The final assistant text (convenience — also in new_messages).
    pub final_text: Option<String>,

    /// Why the turn ended.
    pub finish_reason: FinishReason,

    /// Cumulative token usage for this turn.
    pub usage: TokenUsage,

    /// Total LLM latency in milliseconds.
    pub llm_latency_ms: u64,

    /// Number of steps (LLM calls) executed in this turn.
    pub steps: usize,
}

impl AgentKernel {
    /// Execute one turn. Pure function of inputs → outputs.
    /// The caller (Wrapper) owns all state.
    pub async fn run(&self, ctx: TurnContext) -> Result<TurnOutcome> {
        // ... ReAct loop implementation ...
    }
}
```

### What stays IN the Kernel

- ReAct loop (think → act → observe → repeat)
- Tool execution (dispatch to registry, handle results)
- Anti-stuck detection (consecutive identical failing calls)
- Intra-turn context budget (trim old tool results if THIS turn's messages grow too large)
- Step limit enforcement
- Permission hook evaluation (per-call)
- Plan mode buffering (buffer tool calls, emit plan, wait for confirm/reject)

### What moves OUT of the Kernel

| Concern | Current location | Moves to |
|---------|-----------------|----------|
| `transcript: Vec<Message>` | `Agent` struct field | Wrapper / Session |
| `on_message` callback | `Agent` struct field | Wrapper (persistence) |
| `events` channel | `Agent` struct field | Passed in via `TurnContext.event_sink` |
| `Compactor` (cross-turn) | `Agent` struct field | Wrapper (pre-turn) |
| `HookRegistry` (SessionStart/End) | `Agent` struct field | Wrapper (lifecycle) |
| Multi-turn transcript preservation | `Runner.turn()` + `set_transcript()` | Wrapper |
| Background job references | Passed through `Runner` | Wrapper |

---

## Wrapper Layer Detail

### Core Struct

```rust
/// The Runtime Container — manages an agent's lifecycle across turns.
///
/// Owns the session state, prepares context for each Kernel call,
/// manages background processes, scheduling, and observation.
pub struct AgentRuntime {
    // --- Identity & State ---
    session: Session,

    // --- Kernel (shared, stateless) ---
    kernel: AgentKernel,

    // --- Cross-turn Infrastructure ---
    compactor: Option<Compactor>,
    memory_scope: MemoryScope,
    cost_tracker: Option<CostTracker>,
    hook_registry: HookRegistry,

    // --- Background Resources ---
    bg_jobs: BackgroundJobManager,
    scheduler: Option<Scheduler>,  // loop mode, timers

    // --- Observation ---
    event_bus: EventBus,  // fans out to multiple sinks
}

/// Session state — the stateful core of the Wrapper.
pub struct Session {
    pub id: String,
    pub meta: SessionMeta,
    pub transcript: Vec<Message>,
    pub system_prompt: String,
    writer: Option<SessionWriter>,  // JSONL persistence (opt-in)
}

impl AgentRuntime {
    /// Execute one turn in the conversation.
    pub async fn turn(&mut self, user_message: &str) -> Result<TurnOutcome> {
        // 1. Lifecycle hook: PreTurn
        self.hook_registry.dispatch(HookEvent::PreTurn { message: user_message });

        // 2. Append user message to session transcript
        self.session.append(Message::user(user_message.to_string()));

        // 3. Prepare context for Kernel
        //    - Apply compaction if transcript exceeds threshold
        //    - Inject memory context
        //    - Build message list
        let context = self.prepare_turn_context().await?;

        // 4. Call Kernel
        let outcome = self.kernel.run(context).await?;

        // 5. Append new messages to session transcript
        for msg in &outcome.new_messages {
            self.session.append(msg.clone());
        }

        // 6. Persist (if session writer active)
        self.session.persist_new_messages(&outcome.new_messages)?;

        // 7. Track costs
        if let Some(ref mut tracker) = self.cost_tracker {
            tracker.record(&outcome.usage);
        }

        // 8. Lifecycle hook: PostTurn
        self.hook_registry.dispatch(HookEvent::PostTurn {
            finish_reason: &outcome.finish_reason,
            steps: outcome.steps,
        });

        Ok(outcome)
    }

    /// Prepare TurnContext by compacting transcript, injecting memory, etc.
    async fn prepare_turn_context(&mut self) -> Result<TurnContext> {
        // Apply compaction if needed
        if let Some(ref compactor) = self.compactor {
            if self.session.transcript_chars() > compactor.threshold() {
                let summary = compactor.compact(&self.session.transcript, &self.kernel.llm).await?;
                self.session.replace_with_summary(summary);
            }
        }

        // Build messages: system prompt + memory + transcript
        let mut messages = Vec::new();

        // System prompt with memory injection
        let system = self.build_system_prompt().await;
        messages.push(Message::system(system));

        // Transcript (already compacted if needed)
        messages.extend(self.session.transcript_without_system());

        Ok(TurnContext {
            messages,
            event_sink: self.event_bus.new_sink(),
            tool_specs: self.kernel.tools.specs(),
            streaming: self.session.meta.streaming,
            permission_hook: None,  // or from config
            planning_mode: PlanningMode::default(),
        })
    }
}
```

### Wrapper Responsibilities

| Responsibility | Detail |
|----------------|--------|
| **Multi-turn management** | Accumulate transcript, track turn count |
| **Context preparation** | Compaction, memory injection, system prompt assembly |
| **Persistence** | JSONL append via SessionWriter, meta updates |
| **Background jobs** | Track shell processes spawned by tools, cleanup on session end |
| **Scheduling** | Loop mode (periodic re-invocation), cron-like timers |
| **Cost tracking** | Per-turn token accounting, budget alerts |
| **Event dispatch** | Fan out Kernel events to multiple observers (UI, log, metrics) |
| **Lifecycle hooks** | SessionStart, PreTurn, PostTurn, SessionEnd |
| **Session resume** | Load from JSONL, rebuild transcript, continue |

### Background Jobs & Scheduling

The Wrapper owns these because they outlive any single turn:

```rust
impl AgentRuntime {
    /// A tool (e.g. run_background) registers a job here.
    /// The Wrapper monitors it and can inject status into the next turn's context.
    pub fn register_bg_job(&mut self, job: BackgroundJob) { ... }

    /// Loop mode: schedule the next turn after a delay.
    /// The Wrapper decides when to invoke the Kernel again.
    pub fn schedule_next_turn(&mut self, delay: Duration, goal: String) { ... }

    /// Cleanup: kill background jobs, flush persistence, fire SessionEnd hook.
    pub async fn shutdown(&mut self) { ... }
}
```

---

## Interface Layer Detail

The Interface layer is thin — it adapts a specific interaction protocol to the Wrapper API.

### CLI (one-shot)

```rust
// Simplest case: create runtime, run one turn, print result, exit.
let runtime = AgentRuntime::new(kernel, config);
let outcome = runtime.turn(&goal).await?;
println!("{}", outcome.final_text.unwrap_or_default());
```

### CLI (loop mode)

```rust
// Create runtime with scheduler enabled.
let mut runtime = AgentRuntime::new(kernel, config);
runtime.enable_scheduler();
runtime.turn(&initial_goal).await?;
// Scheduler triggers subsequent turns based on agent's schedule_wakeup calls.
// Runtime stays alive until shutdown signal or agent requests exit.
```

### HTTP Server

```rust
// Multiple concurrent runtimes, one per session.
// HTTP layer manages the session → runtime mapping.
struct HttpServer {
    runtimes: HashMap<String, AgentRuntime>,
    kernel: AgentKernel,  // shared, cloneable
}

// POST /sessions → create new AgentRuntime
// POST /sessions/:id/messages → runtime.turn(message)
// GET /sessions/:id/events → subscribe to runtime.event_bus
// DELETE /sessions/:id → runtime.shutdown()
```

### TUI

```rust
// Single runtime, interactive multi-turn.
// TUI directly drives the runtime and renders events in real-time.
let mut runtime = AgentRuntime::new(kernel, config);
loop {
    let input = tui.read_input().await;
    let event_rx = runtime.event_bus.subscribe();
    // Render events as they arrive (streaming)
    tokio::spawn(render_events(event_rx, &tui));
    let outcome = runtime.turn(&input).await?;
    tui.show_result(&outcome);
}
```

### Multi-Agent

```rust
// Orchestrator creates multiple runtimes with different tool subsets.
// Each sub-agent gets its own runtime (own session, own transcript).
// Shared memory is mediated through the MemoryScope (shared layer).
let researcher_rt = AgentRuntime::new(kernel.with_tools(research_tools), config);
let coder_rt = AgentRuntime::new(kernel.with_tools(coding_tools), config);

let research = researcher_rt.turn("Find relevant APIs").await?;
// Inject research results into coder's memory
coder_rt.memory_scope.inject("research_findings", &research.final_text);
let code = coder_rt.turn("Implement based on research").await?;
```

---

## EventSink Unification

All layers share a common event interface:

```rust
/// Trait for receiving real-time events from the Kernel.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// Unified event type (superset of current StepEvent + SseEvent).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    // --- Kernel-level events (emitted during a turn) ---
    StepStart { step: usize },
    ToolCall { call: ToolCall, step: usize },
    ToolResult { id: String, name: String, output: String, step: usize },
    AssistantText { text: String, step: usize },
    PartialToken { token: String },  // streaming
    Latency { step: usize, llm_ms: u64 },
    Usage { usage: TokenUsage },
    TurnFinished { reason: FinishReason, steps: usize },

    // --- Wrapper-level events (emitted across turns) ---
    Compacted { original_msgs: usize, summary_chars: usize },
    BgJobStarted { job_id: String, command: String },
    BgJobFinished { job_id: String, exit_code: i32 },
    ScheduledWakeup { delay_secs: u64 },
    CostUpdate { total_usd: f64 },
}

/// Concrete implementations:
pub struct ChannelSink(mpsc::UnboundedSender<AgentEvent>);   // CLI, TUI
pub struct BroadcastSink(broadcast::Sender<AgentEvent>);      // HTTP SSE
pub struct NullSink;                                           // tests, background
pub struct LogSink;                                            // structured logging
pub struct CompositeSink(Vec<Box<dyn EventSink>>);            // fan-out
```

The **EventBus** in the Wrapper fans out events to multiple sinks:
- Kernel emits via the sink passed in TurnContext
- Wrapper emits its own events (compaction, bg jobs, scheduling)
- Interface layer subscribes to the bus and renders appropriately

---

## Impact Analysis

### Current Dependency Graph (who uses what from Agent)

```
src/agent.rs exports:
  Agent, AgentBuilder, AgentOutcome, StepEvent, FinishReason,
  PermissionDecision, PermissionHook, OnMessageFn, PlanningMode

Consumers:
┌─────────────────────────────────────────────────────────────────────┐
│ main.rs (2310 LOC)                                                   │
│  - build_agent(): AgentBuilder → Agent                              │
│  - run_one_shot(): agent.run(goal) + SessionWriter via on_message   │
│  - run_repl(): AgentRunner.turn() in loop                           │
│  - loop mode: AgentRunner.run_event_loop()                          │
│  - stream_events*(): consumes StepEvent via mpsc channel            │
│  - exit_for_finish(): matches FinishReason variants                 │
│  - on_message: wires SessionWriter.append into Agent via callback   │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ runner.rs (582 LOC)                                                  │
│  - AgentRunner: holds Agent + BackgroundJobManager                  │
│  - turn(): agent.set_events(tx) → agent.run() → set_transcript()   │
│  - run_loop(): turn + WakeupSlot + sleep                            │
│  - run_event_loop(): turn + WakeupSlot + bg job injection           │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ http.rs (1232 LOC)                                                   │
│  - POST /run: Agent::builder() → agent.run(goal) → drop            │
│  - POST /sessions/:id/messages:                                     │
│    Agent::builder() → set_transcript(session.transcript.clone())    │
│    → agent.run(message) → store outcome.transcript back             │
│  - SSE: mpsc → broadcast forwarding (StepEvent → SseEvent map)     │
│  - SessionState: {id, transcript, system_prompt} in HashMap         │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ multi.rs (1215 LOC)                                                  │
│  - AgentPool.run_with_role(): Agent::builder() → agent.run(goal)   │
│  - Pipeline: sequential run_with_role calls                         │
│  - TeamOrchestrator: lead agent + delegate agents                   │
│  - SharedMemory injected into system_prompt before each run         │
│  - No events, no persistence, no multi-turn (fresh agent per call) │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ tools/sub_agent.rs (120 LOC)                                         │
│  - SubAgent: Agent::builder() → agent.run(goal) per invocation     │
│  - Fresh agent, restricted tool subset, depth-limited               │
│  - Inherits permission_hook from parent                             │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ hooks.rs (531 LOC)                                                   │
│  - HookEvent: SessionStart, PreToolCall, PostToolCall,              │
│    PreCompact, PostCompact, SessionEnd                              │
│  - Dispatched FROM inside Agent::run()                              │
│  - SessionEnd receives &AgentOutcome                                │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ compact.rs (420 LOC)                                                 │
│  - Compactor: stored in Agent struct, invoked mid-loop in run()    │
│  - compact(): takes &[Message] + LlmProvider → summarized messages │
│  - Threshold check inside Agent::run() before each LLM call        │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ cost.rs (601 LOC)                                                    │
│  - CostTracker.on_message_callback() → OnMessageFn                 │
│  - Currently unused (comment says use AgentOutcome.total_usage)     │
│  - Main cost tracking is post-run via outcome.total_usage           │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ crates/recursive-tui (separate crate)                                │
│  - Does NOT import Agent/Runner directly!                           │
│  - Communicates via HTTP API (/sessions endpoints)                  │
│  - Only uses "Agent" as a display string                            │
│  - Impact: ZERO (adapts automatically when HTTP layer changes)      │
└─────────────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────────────┐
│ lib.rs (re-exports)                                                  │
│  pub use agent::{Agent, AgentOutcome, FinishReason, StepEvent}      │
│  pub use agent::{OnMessageFn, PlanningMode}                         │
│  pub use agent::{PermissionDecision, PermissionHook}                │
│  pub use runner::AgentRunner                                        │
│  → Public API surface that external users depend on                 │
└─────────────────────────────────────────────────────────────────────┘
```

### Key Observations for Migration Strategy

1. **TUI is HTTP-only** — no direct agent dependency. This eliminates one migration target entirely.

2. **HTTP creates a fresh Agent per request** — both `/run` and `/sessions/:id/messages` build a new Agent, run it, drop it. The "session" is just transcript stored in a HashMap. Migration is straightforward: replace Agent creation with AgentRuntime usage.

3. **Multi-Agent creates fresh Agents per role** — no cross-turn state. Migration is similarly straightforward.

4. **SubAgent (tool) creates fresh Agents** — same pattern. Easiest to migrate.

5. **Runner is the only true multi-turn consumer** — it does the `set_transcript(outcome.transcript.clone())` dance. This is exactly what AgentRuntime replaces.

6. **main.rs is the most complex consumer** — it has three modes (one-shot, REPL, loop), wires SessionWriter via on_message, manages CostTracker, and drives event streaming. This is where most migration effort concentrates.

7. **Hooks currently fire FROM Agent::run()** — SessionStart/SessionEnd are really "TurnStart/TurnEnd" semantics. In the new model:
   - PreToolCall / PostToolCall → stay in Kernel (they're per-tool-call, within a turn)
   - SessionStart / SessionEnd → move to Wrapper (they're session lifecycle)
   - PreCompact / PostCompact → move to Wrapper (compaction is a Wrapper concern)

8. **Compactor is invoked mid-loop in Agent::run()** — currently it fires between steps when transcript exceeds threshold. In new model: Wrapper compacts BEFORE the turn. Kernel only does lightweight intra-turn trimming (already implemented as `maybe_trim_transcript()`).

9. **`on_message` callback is the bridge to persistence** — Wrapper eliminates this by owning both transcript and SessionWriter. Messages are persisted naturally as they're appended.

10. **Public library API (lib.rs re-exports)** — External crate users depend on `Agent`, `AgentBuilder`, `AgentOutcome`, `StepEvent`, etc. Need backward-compat re-exports or a clean major version bump.

### Risk-ordered Impact Matrix

| Module | Lines | Risk | Reason |
|--------|-------|------|--------|
| `agent.rs` | 2993 | 🔴 Critical | Core loop refactor, highest density of logic |
| `main.rs` | 2310 | 🟡 High | Most complex consumer, 3 execution modes |
| `http.rs` | 1232 | 🟡 High | Concurrent sessions, SSE streaming |
| `multi.rs` | 1215 | 🟢 Medium | Fresh agents per call, straightforward swap |
| `runner.rs` | 582 | 🟢 Medium | Being replaced, but needs working until Phase 4 |
| `hooks.rs` | 531 | 🟢 Medium | Event splitting (kernel hooks vs wrapper hooks) |
| `compact.rs` | 420 | 🟢 Low | Pure logic, just moves to a different caller |
| `cost.rs` | 601 | 🟢 Low | Already mostly post-hoc; on_message_callback unused |
| `sub_agent.rs` | 120 | 🟢 Low | Fresh agent pattern, trivial swap |
| TUI crate | ~700 | ⬜ None | HTTP-only, zero direct dependency |

---

## Migration Path — Detailed Goal Breakdown

### Design Principles for Goal Splitting

1. **Each goal touches ≤3 product files** (avoids merge conflicts in parallel runs)
2. **Each goal is independently testable** (cargo test green after each)
3. **New code before old code removal** (add new path, wire it, remove old)
4. **Public API compatibility during transition** (old types re-export to new)
5. **Goals within a phase can run in parallel if file-disjoint**

---

### Phase 1: New Abstractions (additive only — zero breakage)

These goals add new files/types. Existing code is untouched.

#### Goal A: `src/event.rs` — EventSink trait + AgentEvent

```
Files: NEW src/event.rs, MODIFY src/lib.rs (add pub mod + re-export)
Effort: S
Parallel: Yes (disjoint from B)
```

- Define `EventSink` trait (`fn emit(&self, event: AgentEvent)`)
- Define `AgentEvent` enum (superset of current StepEvent + wrapper events)
- Implement `ChannelSink` (wraps `mpsc::UnboundedSender<AgentEvent>`)
- Implement `NullSink` (for tests)
- Implement `BroadcastSink` (wraps `broadcast::Sender<AgentEvent>`)
- Add conversion: `impl From<StepEvent> for AgentEvent` (bridge)
- Tests: sink emits correctly, null sink doesn't panic, broadcast fan-out

#### Goal B: `src/kernel.rs` — TurnContext + TurnOutcome types

```
Files: NEW src/kernel.rs, MODIFY src/lib.rs (add pub mod + re-export)
Effort: S
Parallel: Yes (disjoint from A)
```

- Define `TurnContext` struct (messages, event_sink, tool_specs, streaming, planning_mode, permission_hook)
- Define `TurnOutcome` struct (new_messages, final_text, finish_reason, usage, latency_ms, steps, side_effects)
- Define `SideEffect` enum (BackgroundJob, ScheduleWakeup)
- Re-export `FinishReason` from agent.rs (no duplication)
- Tests: struct construction, serialization round-trip

---

### Phase 2: Kernel Extraction (refactor agent.rs internals)

The most delicate phase. The external API (`Agent::run(&mut self, goal)`) keeps working. Internally, logic is restructured.

#### Goal C: Extract `run_inner()` — the stateless core loop

```
Files: MODIFY src/agent.rs
Effort: M (this is the critical-path goal)
Parallel: No (must precede D, E)
```

- Add a private method:
  ```rust
  async fn run_inner(
      messages: Vec<Message>,
      llm: &dyn LlmProvider,
      tools: &ToolRegistry,
      event_sink: &dyn EventSink,
      max_steps: usize,
      streaming: bool,
      permission_hook: Option<&PermissionHook>,
      planning_mode: &PlanningMode,
  ) -> Result<TurnOutcome>
  ```
- Move the ReAct loop body into `run_inner()`
- `Agent::run()` becomes: prepare messages from self.transcript → call `run_inner()` → update self.transcript from outcome → return AgentOutcome
- **Zero behavior change** — all existing tests pass unmodified
- `run_inner` is NOT public yet (internal restructuring only)

#### Goal D: Expose `AgentKernel` — public stateless interface

```
Files: MODIFY src/kernel.rs (add impl), MODIFY src/agent.rs (pub fn), MODIFY src/lib.rs
Effort: S
Depends on: Goal C
```

- Add `pub struct AgentKernel` to `src/kernel.rs`:
  ```rust
  #[derive(Clone)]
  pub struct AgentKernel {
      pub(crate) llm: Arc<dyn LlmProvider>,
      pub(crate) tools: ToolRegistry,
      pub(crate) max_steps: usize,
  }
  ```
- `impl AgentKernel { pub async fn run(&self, ctx: TurnContext) -> Result<TurnOutcome> }` calls `Agent::run_inner()` (or directly contains the loop)
- `Agent::run()` now delegates to `AgentKernel::run()` internally
- Add `AgentKernelBuilder`
- Re-export from lib.rs
- Tests: build kernel, run with mock LLM, verify TurnOutcome

---

### Phase 3: Wrapper (AgentRuntime)

#### Goal E: `src/runtime.rs` — AgentRuntime core

```
Files: NEW src/runtime.rs, MODIFY src/lib.rs
Effort: M
Depends on: Goal D
Parallel: Yes (disjoint from F)
```

- Define `AgentRuntime` struct:
  - Holds `AgentKernel` + `Session` (from session.rs) + `Compactor` + `HookRegistry`
- Implement `turn(&mut self, message: &str) -> Result<TurnOutcome>`:
  - Append user message to session transcript
  - Prepare TurnContext (inject system prompt + transcript)
  - Call kernel.run(context)
  - Append new_messages to transcript
  - Persist via SessionWriter
  - Return outcome
- Implement `AgentRuntimeBuilder` (mirrors common AgentBuilder config)
- Tests: create runtime, run turn, verify transcript accumulates

#### Goal F: Move compaction into AgentRuntime

```
Files: MODIFY src/runtime.rs, MODIFY src/agent.rs (remove compactor field)
Effort: S
Depends on: Goal E
```

- `AgentRuntime::prepare_turn_context()` checks transcript size, calls Compactor
- Remove `Compactor` from `Agent` struct fields
- Keep `maybe_trim_transcript()` in kernel as intra-turn optimization only
- `AgentBuilder::compactor()` deprecated (still works but warns)
- Tests: compaction triggers from runtime, not from agent loop

#### Goal G: Move loop/scheduling into AgentRuntime

```
Files: MODIFY src/runtime.rs, deprecate runner.rs methods
Effort: S
Depends on: Goal E
```

- `AgentRuntime::run_loop()` — absorbs `Runner::run_loop()` logic
- `AgentRuntime::run_event_loop()` — absorbs `Runner::run_event_loop()` logic
- `AgentRuntime` holds `BackgroundJobManager` + `WakeupSlot`
- `Runner::run_loop()` marked `#[deprecated]`
- Tests: loop mode works through runtime

---

### Phase 4: Interface Adaptation

#### Goal H: CLI uses AgentRuntime

```
Files: MODIFY src/main.rs
Effort: M
Depends on: Goal G
```

- `run_one_shot()` creates AgentRuntime, calls `.turn()`, no more raw Agent
- `run_repl()` creates AgentRuntime, calls `.turn()` in loop (no more Runner)
- Loop mode uses `AgentRuntime::run_event_loop()`
- Remove `on_message` wiring (Session persistence is internal to runtime)
- Remove `build_agent()` helper (replaced by `AgentRuntimeBuilder`)
- All event streaming subscribes to runtime's EventBus

#### Goal I: HTTP uses AgentRuntime

```
Files: MODIFY src/http.rs
Effort: M
Depends on: Goal E
Parallel: Yes (disjoint from H)
```

- `AppState` holds `kernel: AgentKernel` (shared) + `runtimes: HashMap<String, AgentRuntime>`
- `POST /run`: create ephemeral runtime, call turn, return
- `POST /sessions/:id/messages`: get runtime from map, call turn
- SSE: subscribe to runtime's EventBus (no more mpsc→broadcast forwarding)
- Remove `SessionState` struct (replaced by AgentRuntime's Session)

#### Goal J: Multi-Agent uses AgentRuntime

```
Files: MODIFY src/multi.rs
Effort: S
Depends on: Goal E
Parallel: Yes (disjoint from H, I)
```

- `AgentPool::run_with_role()`: create ephemeral AgentRuntime per call
- SharedMemory injected via runtime's MemoryScope
- Pipeline uses sequence of runtime.turn() calls
- TeamOrchestrator creates one runtime per delegate

#### Goal K: SubAgent uses Kernel directly

```
Files: MODIFY src/tools/sub_agent.rs
Effort: S
Depends on: Goal D
Parallel: Yes
```

- SubAgent stores `AgentKernel` instead of re-building Agent
- Calls `kernel.run(TurnContext)` directly (sub-agents are single-turn)
- Permission hook passed through TurnContext

---

### Phase 5: Cleanup

#### Goal L: Remove Runner

```
Files: DELETE src/runner.rs, MODIFY src/lib.rs, MODIFY src/main.rs
Effort: S
Depends on: Goal H
```

- Remove `pub use runner::AgentRunner` from lib.rs
- Remove `runner.rs`
- If external consumers exist, re-export `AgentRuntime as AgentRunner` with deprecation warning

#### Goal M: Slim down Agent → pure kernel facade

```
Files: MODIFY src/agent.rs, MODIFY src/lib.rs
Effort: M
Depends on: Goals H, I, J, K (all consumers migrated)
```

- Remove from Agent: `transcript`, `events`, `compactor`, `on_message`, `hooks` (session-level ones)
- Keep in Agent: `llm`, `tools`, `max_steps`, `streaming`, `permission_hook`, `planning_mode`
- `Agent::run()` becomes: `fn run(&self, ctx: TurnContext) → TurnOutcome` (same as AgentKernel)
- OR: `Agent` becomes a type alias for `AgentKernel`
- `AgentBuilder` generates `AgentKernel` directly
- Keep `StepEvent` as alias for `AgentEvent` with `#[deprecated]` for backward compat

#### Goal N: Unify event types + final cleanup

```
Files: MODIFY src/agent.rs, src/event.rs, src/http.rs, src/lib.rs
Effort: S
Depends on: Goal M
```

- Remove `StepEvent` (replaced by `AgentEvent`)
- Remove `SseEvent` from http.rs (use `AgentEvent` directly over SSE)
- Remove `OnMessageFn` type (no longer needed)
- Update lib.rs public exports
- Final test sweep

---

### Execution Schedule (estimated batches)

```
Batch N+0 (Phase 1): Goals A + B in parallel
  - New files only, zero risk
  - Outcome: EventSink trait + TurnContext/TurnOutcome types exist

Batch N+1 (Phase 2): Goal C (solo — critical path, agent.rs only)
  - Internal refactor of the ReAct loop
  - Highest risk goal, deserves dedicated batch with review
  - Outcome: Agent::run() internally uses run_inner()

Batch N+2 (Phase 2+3): Goals D + E in parallel
  - D: expose AgentKernel (agent.rs + kernel.rs)
  - E: AgentRuntime basic turn() (runtime.rs, new file)
  - Outcome: Kernel + Runtime both usable

Batch N+3 (Phase 3): Goals F + G in parallel
  - F: compaction moves to runtime (runtime.rs + agent.rs)
  - G: loop/scheduling moves to runtime (runtime.rs)
  - Outcome: Runtime is feature-complete

Batch N+4 (Phase 4): Goals H + I + J + K (partially parallel)
  - H: CLI migration (main.rs) — SERIAL (largest change)
  - I: HTTP migration (http.rs) — parallel with J, K
  - J: Multi-Agent (multi.rs) — parallel with I, K
  - K: SubAgent (sub_agent.rs) — parallel with I, J
  - Outcome: All consumers use new APIs

Batch N+5 (Phase 5): Goals L + M + N
  - Cleanup and finalization
  - Outcome: Clean three-layer architecture
```

Total: **~6 batches**, estimated 14 goals.
Conservative timeline: 6 batches × ~2 hours/batch = 12 hours of agent time.

---

### File Conflict Matrix (for parallel scheduling)

| Goal | agent.rs | kernel.rs | event.rs | runtime.rs | main.rs | http.rs | multi.rs | runner.rs | lib.rs |
|------|----------|-----------|----------|------------|---------|---------|----------|-----------|--------|
| A    |          |           | **NEW**  |            |         |         |          |           | ✏️     |
| B    |          | **NEW**   |          |            |         |         |          |           | ✏️     |
| C    | ✏️       |           |          |            |         |         |          |           |        |
| D    | ✏️       | ✏️        |          |            |         |         |          |           | ✏️     |
| E    |          |           |          | **NEW**    |         |         |          |           | ✏️     |
| F    | ✏️       |           |          | ✏️         |         |         |          |           |        |
| G    |          |           |          | ✏️         |         |         |          |           |        |
| H    |          |           |          |            | ✏️      |         |          |           |        |
| I    |          |           |          |            |         | ✏️      |          |           |        |
| J    |          |           |          |            |         |         | ✏️       |           |        |
| K    |          |           |          |            |         |         |          |           |        |
| L    |          |           |          |            | ✏️      |         |          | ❌        | ✏️     |
| M    | ✏️       |           |          |            |         |         |          |           | ✏️     |
| N    | ✏️       |           | ✏️       |            |         | ✏️      |          |           | ✏️     |

✏️ = modifies, **NEW** = creates, ❌ = deletes

Safe parallel pairs (same batch): A+B, D+E, F+G, I+J+K

---

## Comparison: Before vs After

| Aspect | Before (v0.5.0) | After |
|--------|-----------------|-------|
| Agent state | Stateful (owns transcript, events, hooks) | Stateless kernel (pure executor) |
| Multi-turn | Runner wraps Agent, manually re-seeds transcript | AgentRuntime.turn() — natural |
| Persistence | on_message callback hack in main.rs | Session.persist_new_messages() |
| Events | 3 different types (StepEvent, SseEvent, none) | Unified AgentEvent + EventSink trait |
| Compaction | Inside Agent.run() (mid-turn + cross-turn mixed) | Wrapper: cross-turn; Kernel: intra-turn trim only |
| Background jobs | Threaded through Runner, awkward | AgentRuntime owns BackgroundJobManager |
| HTTP sessions | HashMap<String, SessionState> (in-memory only) | AgentRuntime per session (with persistence) |
| Loop mode | Complex scheduling logic in main.rs | AgentRuntime.scheduler |
| Multi-agent | Fresh Agent per stage, manual memory plumbing | AgentRuntime per sub-agent, shared MemoryScope |
| Cloneability | Agent not Clone (mutable state) | AgentKernel is Clone + Send + Sync |

---

## Open Design Questions

### Q1: Intra-turn context growth — observation, not enforcement

The Wrapper observes context growth in real-time via EventSink (each ToolResult
event carries the output). If the Wrapper detects excessive growth:

- **Between turns**: apply compaction before the next kernel invocation
- **Mid-turn (rare pathological case)**: signal the Kernel to stop via
  CancellationToken (already exists for shutdown signals)

The Kernel retains only the lightweight `maybe_trim_transcript()` logic — a
local optimization that truncates older tool results within the current turn
to keep the LLM's input window reasonable. This is NOT a "budget" — it's a
heuristic that already exists and doesn't need to change.

**No `max_context_growth` field in TurnContext.** The Kernel just does its job;
the Wrapper watches and reacts.

### Q2: Tool execution side effects (background jobs)

When the Kernel executes a `run_background` tool, the job outlives the turn.
How does the Wrapper know about it?

**Proposal**: Tools that create background resources return a structured
"side effect" in the TurnOutcome:

```rust
pub struct TurnOutcome {
    // ...
    /// Side effects that the Wrapper should adopt (bg jobs, scheduled tasks).
    pub side_effects: Vec<SideEffect>,
}

pub enum SideEffect {
    BackgroundJob { id: String, pid: u32, command: String },
    ScheduleWakeup { delay: Duration, prompt: String },
}
```

### Q3: Plan mode confirmation — who handles it?

Currently `confirm_plan()` / `reject_plan()` are on Agent. In the new model:
- Kernel pauses and returns `FinishReason::PlanProposed { calls: Vec<ToolCall> }`
- Wrapper presents the plan to the Interface layer
- Interface gets user confirmation
- Wrapper re-invokes Kernel with confirmation signal

This keeps the Kernel simple (no blocking I/O waiting for user input).

### Q4: Multi-Agent shared state

Each sub-agent has its own AgentRuntime (own session/transcript). Cross-agent
communication via:
- **Shared MemoryScope** (read/write key-value, visible to all sub-agents)
- **Event Bus fan-out** (parent orchestrator subscribes to all sub-agent events)
- **Explicit message passing** (orchestrator injects results from one agent into another's next turn context)

### Q5: Naming — Agent vs Kernel vs Runtime

| Current | Proposed | Rationale |
|---------|----------|-----------|
| `Agent` | `AgentKernel` | Emphasizes "execution engine", not "intelligent entity" |
| `Runner` | `AgentRuntime` | Emphasizes "runtime container", lifecycle management |
| n/a | `Session` | Already exists partially, promoted to first-class |
| `StepEvent` | `AgentEvent` | Unified across kernel + wrapper |

Alternative: keep `Agent` for the Kernel (it IS the agent, just stateless),
and call the Wrapper `AgentSession` or `AgentContext`. Open for discussion.

---

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Large refactor touching agent.rs (core loop) | Phase 1 is purely structural — no behavior change. Tests prove equivalence. |
| Runner is used by TUI crate (external) | Phase 4 provides a clean migration path. Runner can be kept as deprecated adapter during transition. |
| HTTP sessions need live migration | HTTP server currently has no persistent sessions anyway — restart = fresh. No migration needed. |
| Multi-agent pipeline breaks | Multi-agent already creates fresh agents per stage — adapting to Kernel is straightforward. |
| Performance (extra allocation per turn) | TurnContext messages are already being cloned in current code (Runner.turn does clone). No new copies. |

---

## References

- Current code: `src/agent.rs`, `src/runner.rs`, `src/session.rs`, `src/http.rs`, `src/multi.rs`
- Memory system: `src/tools/memory.rs`, `src/tools/facts.rs`, `src/tools/episodic_recall.rs`
- Related: `memory-and-session-persistence.md` (implemented)
- Related: `agent-evaluation-system.md` (pending)
- ROADMAP: Phase 14 (Persistence), Phase 15 (Observability), Phase 17 (Production Hardening)

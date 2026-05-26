# Goal 87 — AgentRunner: Cross-Turn Wrapper

**Roadmap**: Phase 9 — Loop Mode (part 1/4)

**Design principle check**:
- Implemented as: new module `src/runner.rs` that wraps `Agent`. No changes
  to agent.rs's main loop.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Currently the REPL in `main.rs` manually manages the agent across turns:
builds once, calls `agent.run()`, then `agent.set_transcript()`. This is
fine for user-driven interaction but cannot support event-driven loop mode
(timer wakeups, background shell completions) because the orchestration
logic is baked into the CLI's REPL function.

`AgentRunner` is the extraction of this "manage an agent across multiple
turns" concern into a reusable, embeddable struct. It becomes the
foundation for Loop Mode, HTTP API sessions, and TUI.

## Scope (do exactly this, no more)

### 1. `src/runner.rs` — new file

```rust
use crate::{Agent, AgentOutcome, Config, FinishReason, StepEvent};
use tokio::sync::mpsc;

/// Manages an Agent across multiple conversation turns.
/// Preserves transcript between turns, emits events, and tracks cumulative usage.
pub struct AgentRunner {
    agent: Agent,
    total_turns: usize,
    // Events channel for the current turn — recreated each turn
    events_tx: Option<mpsc::UnboundedSender<StepEvent>>,
}

impl AgentRunner {
    /// Create from a pre-built Agent.
    pub fn new(agent: Agent) -> Self {
        Self { agent, total_turns: 0, events_tx: None }
    }

    /// Run a single turn with the given goal. Returns the outcome.
    /// Transcript is preserved between turns automatically.
    pub async fn turn(&mut self, goal: impl Into<String>) -> crate::Result<AgentOutcome> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.agent.set_events(Some(tx));

        let outcome = self.agent.run(goal).await?;

        // Restore transcript for next turn
        self.agent.set_transcript(outcome.transcript.clone());
        self.agent.set_events(None);
        self.total_turns += 1;

        Ok(outcome)
    }

    /// Clear the conversation history (start fresh).
    pub fn clear(&mut self) {
        self.agent.set_transcript(Vec::new());
        self.total_turns = 0;
    }

    /// Get the events receiver for the current turn.
    /// Call this AFTER calling `turn()` — the receiver is created inside turn().
    /// Actually: expose a subscription model instead.
    pub fn subscribe_events(&self) -> mpsc::UnboundedReceiver<StepEvent> {
        // This needs a different design — see below
        todo!()
    }

    /// Number of turns completed so far.
    pub fn turns(&self) -> usize {
        self.total_turns
    }

    /// Access the underlying agent (e.g., to call confirm_plan).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }
}
```

**Important design decision**: Events. The caller needs to receive events
during a turn (for printing, streaming, etc.). Two options:

A) `turn()` returns `(AgentOutcome, Vec<StepEvent>)` — simple, buffered
B) `turn()` takes an `mpsc::UnboundedSender<StepEvent>` parameter — streaming

Choose B (streaming). Signature becomes:
```rust
pub async fn turn(
    &mut self,
    goal: impl Into<String>,
    events: Option<mpsc::UnboundedSender<StepEvent>>,
) -> crate::Result<AgentOutcome>
```

If events is None, no events are emitted. If Some, events stream in real-time.

### 2. `src/lib.rs` — add module and re-export

```rust
pub mod runner;
pub use runner::AgentRunner;
```

### 3. Tests

- Test: AgentRunner preserves transcript across turns (mock provider returns
  different content each turn; second turn's transcript has first turn's messages)
- Test: AgentRunner::clear() resets transcript
- Test: AgentRunner::turns() increments
- Test: events are forwarded correctly when provided
- Test: no events emitted when None is passed

### 4. Refactor REPL to use AgentRunner (optional, bonus)

If time allows, refactor `main.rs::repl()` to use `AgentRunner` instead of
raw `agent.set_transcript()` calls. This validates the API ergonomics.
Skip this if it would touch too many lines — it can be a follow-up goal.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `AgentRunner` is public in the crate root
- No changes to `src/agent.rs`'s `run()` method

## Notes for the agent

- Read `src/main.rs::repl()` to understand the current cross-turn pattern.
- Read `src/agent.rs` for `set_transcript()` and `set_events()` methods.
- The `AgentOutcome` struct is in `src/agent.rs` — it has `transcript`,
  `finish`, `total_usage`, `steps`, `total_llm_latency_ms`.
- Keep it simple: this is just a convenience wrapper, not a new abstraction
  layer. ~100-150 LOC max.
- Do NOT add new dependencies.

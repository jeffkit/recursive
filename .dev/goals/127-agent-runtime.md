# Goal 127 — AgentRuntime: the Wrapper layer

**Roadmap**: Kernel Architecture Refactor — Phase 3a (wrapper)

**Design principle check**:
- Implemented as: new `src/runtime.rs` module
- Pure addition — existing code NOT modified (except lib.rs re-export)
- Does NOT modify agent.rs, main.rs, http.rs, runner.rs, multi.rs

## Why

The three-layer architecture needs a Wrapper that manages an agent's lifecycle
across turns: transcript accumulation, context preparation, persistence, and
eventually compaction/scheduling. `AgentRuntime` is this Wrapper.

This goal creates the basic struct and a working `turn()` method that:
1. Appends the user message to the session transcript
2. Prepares a TurnContext (system prompt + transcript)
3. Calls AgentKernel::run()
4. Appends new messages back to the transcript
5. Returns the TurnOutcome

## Scope (do exactly this, no more)

### 1. Create `src/runtime.rs`

```rust
use crate::kernel::{AgentKernel, TurnContext, TurnOutcome};
use crate::agent::PlanningMode;
use crate::event::NullSink;
use crate::message::Message;
use crate::error::Result;

/// The Runtime Container — manages an agent's lifecycle across turns.
///
/// Owns the session state and prepares context for each Kernel call.
/// This is the "Wrapper" in the three-layer architecture.
pub struct AgentRuntime {
    kernel: AgentKernel,
    system_prompt: String,
    transcript: Vec<Message>,
    turn_count: usize,
}

impl AgentRuntime {
    /// Create a new runtime with a kernel and system prompt.
    pub fn new(kernel: AgentKernel, system_prompt: impl Into<String>) -> Self {
        Self {
            kernel,
            system_prompt: system_prompt.into(),
            transcript: Vec::new(),
            turn_count: 0,
        }
    }

    /// Execute one turn: user sends a message, agent processes and responds.
    pub async fn turn(&mut self, user_message: &str) -> Result<TurnOutcome> {
        // 1. Append user message to transcript
        self.transcript.push(Message::user(user_message.to_string()));

        // 2. Prepare TurnContext
        let mut messages = Vec::with_capacity(1 + self.transcript.len());
        messages.push(Message::system(self.system_prompt.clone()));
        messages.extend(self.transcript.clone());

        let ctx = TurnContext {
            messages,
            event_sink: Box::new(NullSink),
            tool_specs: self.kernel.tools().specs(),
            streaming: false,
            permission_hook: None,
            planning_mode: PlanningMode::default(),
        };

        // 3. Call kernel
        let outcome = self.kernel.run(ctx).await?;

        // 4. Append new messages to transcript
        for msg in &outcome.new_messages {
            self.transcript.push(msg.clone());
        }

        self.turn_count += 1;
        Ok(outcome)
    }

    /// Get the current transcript.
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    /// Get the number of completed turns.
    pub fn turn_count(&self) -> usize {
        self.turn_count
    }

    /// Clear the transcript and reset turn count.
    pub fn clear(&mut self) {
        self.transcript.clear();
        self.turn_count = 0;
    }

    /// Access the underlying kernel.
    pub fn kernel(&self) -> &AgentKernel {
        &self.kernel
    }
}
```

### 2. Wire into lib.rs

Add `pub mod runtime;` and re-export:
```rust
pub use runtime::AgentRuntime;
```

### 3. Tests

- `runtime_single_turn` — create runtime with MockProvider, call turn(), verify outcome has final_text and transcript grows
- `runtime_multi_turn` — call turn() twice, verify transcript accumulates both exchanges
- `runtime_clear` — call turn(), then clear(), verify transcript is empty and turn_count resets
- `runtime_includes_system_prompt` — verify the system prompt is prepended to messages sent to kernel

## Acceptance

- `cargo test` green (518+ tests, new tests added)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- New module `src/runtime.rs` exists
- `AgentRuntime::turn()` works with MockProvider
- `lib.rs` re-exports `AgentRuntime`
- **No changes** to agent.rs, main.rs, http.rs, runner.rs, multi.rs, kernel.rs

## Notes for the agent

- Read `src/kernel.rs` to understand `AgentKernel`, `TurnContext`, `TurnOutcome`.
- Read `src/event.rs` for `NullSink` (used as default event sink).
- Read `src/llm/mock.rs` for `MockProvider` — needed for tests.
- Read existing tests in `src/kernel.rs` for how to construct an `AgentKernel` in tests.
- `ToolRegistry` requires a workspace path. Use `tempfile::tempdir()` in tests.
- `TurnContext.tool_specs` should come from `self.kernel.tools().specs()`.
- Keep it simple: no compaction, no persistence, no event bus yet. Those come in later goals.
- **DO NOT touch any existing file except `src/lib.rs`** (adding `pub mod runtime;` + re-export).

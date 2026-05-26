# Goal 88 — schedule_wakeup Tool + Loop Runner

**Roadmap**: Phase 9 — Loop Mode (part 2/4)

**Design principle check**:
- Implemented as: new tool file `src/tools/schedule_wakeup.rs` +
  additions to `src/runner.rs`. No changes to agent.rs's main loop.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

For Loop Mode, the agent needs a way to say "wake me up in X seconds with
this context". This is how the loop sustains itself: each turn ends with
the agent scheduling its own next wakeup (or not, to end the loop).

Currently `src/runner.rs` has `AgentRunner` with `turn()` and `clear()`.
This goal EXTENDS it with loop support and CREATES a new tool.

## Scope (do exactly this, no more)

### 1. CREATE `src/tools/schedule_wakeup.rs` — brand new file

This file does NOT exist yet. You must create it from scratch.

```rust
//! schedule_wakeup tool — lets the agent request a timed re-invocation.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;
use crate::llm::ToolSpec;
use crate::error::Result;

/// Shared slot where the tool writes a wakeup request.
pub type WakeupSlot = Arc<Mutex<Option<WakeupRequest>>>;

/// A wakeup request placed by the schedule_wakeup tool during a turn.
#[derive(Debug, Clone)]
pub struct WakeupRequest {
    pub delay: std::time::Duration,
    pub reason: String,
    pub prompt: String,
}

pub struct ScheduleWakeup {
    slot: WakeupSlot,
}

impl ScheduleWakeup {
    pub fn new(slot: WakeupSlot) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl Tool for ScheduleWakeup {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_wakeup".into(),
            description: "Schedule the next loop iteration. The runner will \
                          sleep for delay_secs then re-invoke the agent with \
                          the given prompt.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "delay_secs": {
                        "type": "integer",
                        "description": "Seconds to sleep before next turn (1-3600)",
                        "minimum": 1,
                        "maximum": 3600
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why this wakeup is needed (shown in logs)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Goal/context for the next turn"
                    }
                },
                "required": ["delay_secs", "reason", "prompt"]
            }),
        }
    }

    async fn call(&self, args: Value) -> Result<String> {
        let delay_secs = args["delay_secs"].as_u64().unwrap_or(60).clamp(1, 3600);
        let reason = args["reason"].as_str().unwrap_or("").to_string();
        let prompt = args["prompt"].as_str().unwrap_or("").to_string();

        let request = WakeupRequest {
            delay: std::time::Duration::from_secs(delay_secs),
            reason: reason.clone(),
            prompt,
        };

        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(request);
        }

        Ok(format!("Wakeup scheduled: {reason} in {delay_secs}s"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_wakeup_request() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        let args = serde_json::json!({"delay_secs": 30, "reason": "check status", "prompt": "check if done"});
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("30s"));
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, std::time::Duration::from_secs(30));
        assert_eq!(req.prompt, "check if done");
    }

    #[tokio::test]
    async fn clamps_delay() {
        let slot: WakeupSlot = Arc::new(Mutex::new(None));
        let tool = ScheduleWakeup::new(slot.clone());
        let args = serde_json::json!({"delay_secs": 9999, "reason": "x", "prompt": "y"});
        tool.call(args).await.unwrap();
        let req = slot.lock().unwrap().take().unwrap();
        assert_eq!(req.delay, std::time::Duration::from_secs(3600));
    }
}
```

### 2. EXTEND `src/runner.rs` — add loop support

Add these to the existing `AgentRunner` in `src/runner.rs`:

```rust
use crate::tools::schedule_wakeup::{WakeupSlot, WakeupRequest};

// Add field to AgentRunner:
//   wakeup_slot: WakeupSlot,

impl AgentRunner {
    // New constructor that also creates the wakeup slot:
    pub fn with_wakeup_slot(agent: Agent, slot: WakeupSlot) -> Self { ... }

    /// Run a loop: execute turns until the agent stops calling schedule_wakeup.
    pub async fn run_loop(
        &mut self,
        initial_goal: impl Into<String>,
        events: Option<mpsc::UnboundedSender<StepEvent>>,
    ) -> Result<Vec<AgentOutcome>> { ... }

    /// Take the pending wakeup request (if any) placed during the last turn.
    pub fn take_pending_wakeup(&mut self) -> Option<WakeupRequest> { ... }
}
```

### 3. Register `schedule_wakeup` in `src/tools/mod.rs`

Add `pub mod schedule_wakeup;` and export `ScheduleWakeup`.
Add it to the default tool registration (gated behind a `WakeupSlot` parameter).

### 4. Tests (in addition to the inline ones)

- Test: AgentRunner::run_loop executes 3 turns (mock schedules 2 wakeups then stops)
- Test: run_loop returns all outcomes
- Test: run_loop ends immediately if first turn doesn't schedule

## Acceptance

- `cargo test` green (including new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `src/tools/schedule_wakeup.rs` exists as a new file
- `AgentRunner::run_loop` is public and tested
- No changes to `src/agent.rs`

## Notes for the agent

- **IMPORTANT**: `src/tools/schedule_wakeup.rs` does NOT exist. You must CREATE it.
- Read `src/runner.rs` — it already has `AgentRunner`. You are ADDING to it.
- Read `src/tools/mod.rs` for `pub mod` declarations and `Tool` trait usage.
- Read `src/tools/shell.rs` to see how `BackgroundJobManager` (an Arc<Mutex>)
  is passed to a tool — same pattern for `WakeupSlot`.
- The `WakeupSlot` is `Arc<Mutex<Option<WakeupRequest>>>`. Very simple.
- ~120-180 LOC of new code total.
- **DO NOT modify `src/tools/shell.rs`, `src/tools/mod.rs` beyond adding
  `pub mod schedule_wakeup;` and `pub use schedule_wakeup::*;`, or any
  other existing tool file.** The scope is: create one new file + extend runner.
- **DO NOT refactor ToolTransport, RunShell, or any transport mechanism.**
  This goal is ONLY about the schedule_wakeup tool and the AgentRunner loop.
- If you find yourself editing shell.rs, STOP. You are off-scope.

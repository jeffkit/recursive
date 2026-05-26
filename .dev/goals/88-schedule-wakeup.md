# Goal 88 — schedule_wakeup Tool + Timer Event Source

**Roadmap**: Phase 9 — Loop Mode (part 2/4)

**Design principle check**:
- Implemented as: new tool `src/tools/schedule_wakeup.rs` + event types in
  `src/runner.rs`. No changes to agent.rs's main loop.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

For Loop Mode, the agent needs a way to say "wake me up in X seconds with
this context". This is how the loop sustains itself: each turn ends with
the agent scheduling its own next wakeup (or not, to end the loop).

## Scope (do exactly this, no more)

### 1. `src/tools/schedule_wakeup.rs` — new tool

```rust
/// Tool: schedule_wakeup
///
/// Parameters:
///   delay_secs: u64 — seconds until next wakeup (min 1, max 3600)
///   reason: String — why (shown in logs)
///   prompt: String — the goal/context to inject on wakeup
///
/// Returns: "Wakeup scheduled: {reason} in {delay_secs}s"
///
/// Side effect: stores the wakeup request in a shared WakeupSlot
/// that the AgentRunner can read after the turn completes.
```

The tool itself just writes to a shared slot — it doesn't do any timer
scheduling. The AgentRunner reads the slot after `turn()` returns and
decides what to do.

### 2. `src/runner.rs` — add WakeupRequest and loop support

```rust
/// A wakeup request placed by the schedule_wakeup tool during a turn.
#[derive(Debug, Clone)]
pub struct WakeupRequest {
    pub delay: std::time::Duration,
    pub reason: String,
    pub prompt: String,
}

impl AgentRunner {
    /// Run a loop: execute turns until the agent stops scheduling wakeups
    /// or a stop condition is met.
    pub async fn run_loop(
        &mut self,
        initial_goal: impl Into<String>,
        events: Option<mpsc::UnboundedSender<StepEvent>>,
    ) -> crate::Result<Vec<AgentOutcome>> {
        let mut outcomes = Vec::new();
        let mut next_goal = initial_goal.into();

        loop {
            let outcome = self.turn(&next_goal, events.clone()).await?;
            let wakeup = self.take_pending_wakeup();
            outcomes.push(outcome);

            match wakeup {
                Some(req) => {
                    tokio::time::sleep(req.delay).await;
                    next_goal = req.prompt;
                }
                None => break, // No wakeup scheduled = loop ends
            }
        }
        Ok(outcomes)
    }

    /// Take the pending wakeup request (if any) placed during the last turn.
    pub fn take_pending_wakeup(&mut self) -> Option<WakeupRequest> {
        // Read from the shared slot
        todo!()
    }
}
```

### 3. Shared WakeupSlot mechanism

Use `Arc<Mutex<Option<WakeupRequest>>>` passed to the tool at registration
time. The tool writes; the runner reads after the turn.

### 4. Register the tool

In `src/tools/mod.rs`, add `schedule_wakeup` to the registry like other tools.

### 5. Tests

- Test: schedule_wakeup tool stores a WakeupRequest
- Test: AgentRunner::run_loop executes multiple turns based on wakeup
- Test: Loop ends when agent doesn't call schedule_wakeup
- Test: delay_secs is clamped (min 1, max 3600)
- Test: prompt is passed to next turn correctly

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- schedule_wakeup tool is registered and callable
- AgentRunner::run_loop works with mock provider

## Notes for the agent

- Read `src/tools/mod.rs` for how tools are registered.
- Read `src/tools/shell.rs` for how an existing tool accesses shared state
  (the `BackgroundJobManager` pattern — tool holds an Arc to shared state).
- Read goal-87's `AgentRunner` for the wrapper it builds on.
- The `WakeupSlot` = `Arc<Mutex<Option<WakeupRequest>>>`. Create it in
  AgentRunner, pass a clone to the tool at registration.
- Keep it simple: ~150 LOC across both files.
- Do NOT sleep in the tool itself — the tool just records the request.

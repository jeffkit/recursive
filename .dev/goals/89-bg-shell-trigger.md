# Goal 89 — Background Shell Complete Auto-Triggers Next Turn

**Roadmap**: Phase 10.3 — Loop Mode (part 3/4)

**Design principle check**:
- Implemented as: extension to `src/runner.rs`. Uses existing
  `BackgroundJobManager`. No changes to agent.rs's main loop.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

When an agent spawns a background shell job (via `run_background`), it
currently has to poll with `check_background`. In loop mode, the wrapper
should detect when a background job completes and automatically trigger
the next turn with the job's output as context.

## Scope (do exactly this, no more)

### 1. `src/runner.rs` — add `run_loop_with_bg` or extend `run_loop`

Add a method that, after each turn, checks both:
1. The WakeupSlot (existing from g88)
2. The BackgroundJobManager for completed jobs

If a background job completed, inject its output as the next turn's goal
(e.g., "Background job {id} completed with exit code {code}: {output}").

```rust
impl AgentRunner {
    /// Run a loop with background job awareness.
    /// Triggers on: wakeup schedule OR background job completion.
    pub async fn run_event_loop(
        &mut self,
        initial_goal: impl Into<String>,
        wakeup_slot: &WakeupSlot,
        events: Option<mpsc::UnboundedSender<StepEvent>>,
    ) -> Result<Vec<AgentOutcome>> {
        let mut outcomes = Vec::new();
        let mut next_goal = initial_goal.into();

        loop {
            let outcome = self.turn(&next_goal, events.clone()).await?;
            outcomes.push(outcome);

            // Priority 1: explicit wakeup
            let wakeup = wakeup_slot.lock().ok().and_then(|mut s| s.take());
            if let Some(req) = wakeup {
                tokio::time::sleep(req.delay).await;
                next_goal = req.prompt;
                continue;
            }

            // Priority 2: background job completed
            if let Some(ref mgr) = self.bg_manager {
                if let Ok(mgr) = mgr.try_lock() {
                    if let Some((id, output)) = mgr.take_completed() {
                        next_goal = format!(
                            "Background job '{}' completed:\n{}",
                            id, output
                        );
                        continue;
                    }
                }
            }

            // Nothing to do → loop ends
            break;
        }
        Ok(outcomes)
    }
}
```

### 2. `src/tools/run_background.rs` — add `take_completed` method

Add a method to `BackgroundJobManager` that returns the first completed
job (if any), removing it from the tracked set:

```rust
impl BackgroundJobManager {
    /// Remove and return the first completed job, if any.
    pub fn take_completed(&self) -> Option<(String, String)> {
        // Check each job, find one that's finished, remove and return
        // (job_id, output_string)
    }
}
```

### 3. Tests

- Test: run_event_loop triggers on wakeup (same as run_loop)
- Test: run_event_loop ends when neither wakeup nor bg job
- Test: BackgroundJobManager::take_completed returns finished job

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `AgentRunner::run_event_loop` is public
- No changes to `src/agent.rs`

## Notes for the agent

- Read `src/runner.rs` for existing `AgentRunner` and `run_loop`.
- Read `src/tools/run_background.rs` for `BackgroundJobManager`.
- The bg_manager field already exists on AgentRunner (added by prior merge).
- Keep it ~50-80 LOC of new code. This is a small extension.
- **DO NOT modify shell.rs or any tool other than run_background.rs.**

# Goal 125 — Extract run_inner(): stateless core loop

**Roadmap**: Kernel Architecture Refactor — Phase 2 (kernel extraction)

**Design principle check**:
- Implemented as: internal refactor of `src/agent.rs`
- The PUBLIC API does NOT change — `Agent::run(&mut self, goal)` still works identically
- Internally, the ReAct loop is extracted into a helper that accepts messages as input

## Why

The Agent currently owns its transcript (`Vec<Message>`) and mutates it during
`run()`. This makes it impossible to share the kernel across sessions or to
use it as a pure function. By extracting the loop logic into an internal
`run_inner()` that accepts prepared messages and returns new messages, we
create the foundation for the stateless `AgentKernel::run()` that the Wrapper
will call.

This is the critical-path goal in the kernel architecture refactor. It must
produce ZERO behavior changes — all 518 existing tests must pass unchanged.

## Scope (do exactly this, no more)

### 1. Add internal helper `run_inner()`

In `src/agent.rs`, add a new **private** function (NOT a method on Agent):

```rust
/// Execute the ReAct loop on a prepared message list.
///
/// This is the core execution logic extracted from `Agent::run()`.
/// It takes all inputs as parameters (no `&self`) and returns the
/// new messages produced during execution plus metadata.
///
/// NOTE: This function is private. The public API remains `Agent::run()`.
async fn run_inner(
    messages: Vec<Message>,
    llm: &dyn LlmProvider,
    tools: &ToolRegistry,
    max_steps: usize,
    max_transcript_chars: Option<usize>,
    streaming: bool,
    compactor: Option<&Compactor>,
    permission_hook: Option<&PermissionHook>,
    hooks: &HookRegistry,
    planning_mode: &PlanningMode,
    event_tx: Option<&mpsc::UnboundedSender<StepEvent>>,
) -> Result<RunInnerOutcome> {
    // ... the ReAct loop body moves here ...
}

/// Internal outcome type for run_inner.
struct RunInnerOutcome {
    /// The complete transcript (input messages + all new messages produced).
    transcript: Vec<Message>,
    /// The final assistant text.
    final_message: Option<String>,
    /// Why the loop ended.
    finish: FinishReason,
    /// Token usage accumulated during this run.
    usage: TokenUsage,
    /// Total LLM latency in ms.
    llm_latency_ms: u64,
    /// Number of steps executed.
    steps: usize,
}
```

### 2. Move the loop body from Agent::run() into run_inner()

The content of the `for step in 1..=self.max_steps { ... }` loop and the
surrounding setup/teardown moves into `run_inner()`. Inside `run_inner()`:
- Replace `self.llm` with `llm` parameter
- Replace `self.tools` with `tools` parameter
- Replace `self.transcript` with local `messages` vec
- Replace `self.emit(...)` with a local helper that uses the `event_tx` parameter
- Replace `self.push_message(...)` with direct `messages.push(...)` + event emit
- Replace `self.compactor` with `compactor` parameter
- Replace `self.hooks` with `hooks` parameter
- Replace `self.permission_hook` with `permission_hook` parameter

### 3. Agent::run() becomes a thin wrapper

```rust
pub async fn run(&mut self, goal: impl Into<String>) -> Result<AgentOutcome> {
    let goal = goal.into();

    // Prepare: push user message to transcript
    self.push_message(Message::user(goal.clone()));

    // Delegate to the stateless core
    let outcome = run_inner(
        std::mem::take(&mut self.transcript),
        self.llm.as_ref(),
        &self.tools,
        self.max_steps,
        self.max_transcript_chars,
        self.streaming,
        self.compactor.as_ref(),
        self.permission_hook.as_ref(),
        &self.hooks,
        &self.planning_mode,
        self.events.as_ref(),
    ).await?;

    // Restore transcript
    self.transcript = outcome.transcript;
    self.total_llm_latency_ms = outcome.llm_latency_ms;

    Ok(AgentOutcome {
        final_message: outcome.final_message,
        transcript: self.transcript.clone(),
        steps: outcome.steps,
        finish: outcome.finish,
        total_usage: outcome.usage,
        total_llm_latency_ms: outcome.llm_latency_ms,
    })
}
```

### 4. Handle plan mode

Plan mode state (`plan_buffer`, `plan_confirmed`) currently lives on `Agent`.
For now, keep it on Agent and pass it through as mutable state:
- Option A: Pass `&mut Option<Vec<ToolCall>>` as a parameter to `run_inner()`
- Option B: Keep plan handling in `Agent::run()` (call `run_inner()` in a loop
  where plan confirmation is checked between iterations)

Choose whichever is simpler. The key requirement is that plan mode still works
in tests.

### 5. No new tests required

All 518 existing tests must pass unchanged. The refactoring is purely internal.
If existing plan-mode tests pass, that validates the plan handling approach.

## Acceptance

- `cargo test` green — **exactly 518+ tests passing**
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `Agent::run()` public signature unchanged
- `Agent::run()` behavior unchanged (all existing tests prove this)
- New private `run_inner()` function exists and contains the core loop logic
- `Agent::run()` delegates to `run_inner()`
- No new public API added

## Notes for the agent

- **Read `src/agent.rs` COMPLETELY first** (all ~2993 lines). You must understand
  the full loop logic, including:
  - Anti-stuck detection (lines ~448-500)
  - Tool execution with parallelism (lines ~282-435)
  - Compaction trigger (lines ~460-490)
  - Plan mode buffering (search for `PlanningMode::PlanFirst`)
  - `on_message` callback invocation (search for `on_message`)
  - Transcript trimming (search for `maybe_trim_transcript`)

- The `on_message` callback is tricky: it fires on every message push. In
  `run_inner()`, you need to pass it as a parameter or include it in the
  emit-to-event-channel path. Simplest: pass `on_message: Option<&OnMessageFn>`.

- **Do NOT rename `Agent`** — that happens later (Goal M).
- **Do NOT change `AgentOutcome`** — that's the public return type.
- **Do NOT touch `src/kernel.rs` or `src/event.rs`** — those are for later goals.
- **Do NOT add `run_inner` to the public API** — it stays `pub(crate)` at most.

- The function signature of `run_inner()` doesn't need to be pretty. It will
  have many parameters. That's OK — in a later goal it will accept `TurnContext`
  instead. For now, just get the extraction correct.

- If `run_inner()` has too many parameters (>10), consider a private config struct:
  ```rust
  struct RunConfig<'a> {
      llm: &'a dyn LlmProvider,
      tools: &'a ToolRegistry,
      max_steps: usize,
      // ...
  }
  ```

- **Be careful with the `on_message` callback lifetime.** It's `Box<dyn Fn(...)>`.
  You may need to pass `Option<&dyn Fn(&Message)>` to `run_inner()`.

- **DO NOT modify any file other than `src/agent.rs`.**

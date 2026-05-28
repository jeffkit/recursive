# Goal 131 — CLI uses AgentRuntime (Goal H)

**Roadmap**: Kernel Architecture Refactor — Phase 4c (CLI migration)

**Design principle check**:
- `run_once()` creates `AgentRuntime`, calls `.run()`, no more raw `Agent`
- `repl()` creates `AgentRuntime`, calls `.run()` in loop (no more `AgentRunner`)
- Loop mode uses `AgentRuntime::run_loop()` / `run_event_loop()`
- Event streaming wired through runtime's `EventSink`
- Session persistence done via batch write after each turn (on_message removed)
- `build_agent()` replaced by `build_runtime()`

## Why

`src/main.rs` currently builds a raw `Agent` and wraps it in `AgentRunner` for
multi-turn and loop use cases. Now that `AgentRuntime` is feature-complete
(Goals E, F, G done), main.rs should be the first consumer to migrate.

Key benefits:
- Transcript accumulation is automatic (no manual `set_transcript()`)
- Loop mode is built-in (`run_loop()` / `run_event_loop()`)
- Single abstraction across all CLI modes

## Scope (do exactly this, no more)

### Step 1 — Wire EventSink into AgentRuntime::run()

Currently `runtime.rs` line ~145 has:
```rust
event_sink: Box::new(NullSink), // TODO: wire the stored EventSink in a future goal
```

And `kernel.rs` line ~203 has:
```rust
events: None, // TODO: bridge EventSink → mpsc in a future goal
```

**Fix both TODOs** so that events flow from kernel → runtime's EventSink:

In `runtime.rs`:
- Change `event_sink` field from `Box<dyn EventSink>` to `Arc<dyn EventSink>`.
- In `AgentRuntime::run()`:
  1. Create `let (step_tx, step_rx) = tokio::sync::mpsc::unbounded_channel::<StepEvent>();`
  2. Clone `Arc<dyn EventSink>` to share with a forwarder task.
  3. Spawn: `tokio::spawn(async move { while let Some(ev) = step_rx.recv().await { sink.emit(ev.into()).await; } })`
  4. Build `TurnContext` with the new field `step_events_tx: Some(step_tx)` (see Step 1b).
  5. After `self.kernel.run(ctx).await?`, drop the tx, await the forwarder task.
- Update `AgentRuntimeBuilder::event_sink()` to accept `Arc<dyn EventSink>`.

In `kernel.rs` — add `step_events_tx: Option<tokio::sync::mpsc::UnboundedSender<StepEvent>>` to `TurnContext`, and wire it to `RunCore.events`:
```rust
// In kernel.rs run():
let core = RunCore {
    ...
    events: ctx.step_events_tx,  // was: None
    ...
};
```
Add the new field to `TurnContext` struct.

In `runtime.rs run()`, set `ctx.step_events_tx = Some(step_tx)`.

### Step 2 — Add plan mode support to AgentRuntime

Currently `AgentRuntime` has no `confirm_plan()` / `reject_plan()`. The CLI's
`run_once()` needs these for `--plan-first` mode.

Add to `AgentRuntime`:
```rust
/// Pending plan tool calls buffered by the kernel (plan-first mode).
pending_plan_calls: Option<Vec<crate::llm::ToolCall>>,
/// Whether the user confirmed the pending plan.
plan_confirmed: bool,
```

In `AgentRuntime::run()`, after the kernel returns `FinishReason::PlanPending`:
- Store `turn_outcome.plan_calls` (if any) in `self.pending_plan_calls`.
- On the NEXT `run()` call, if `self.plan_confirmed == true`:
  - Set `plan_confirmed = true` in `RunCore` (via a new `TurnContext` field `plan_confirmed: bool`).
  - Inject `pending_plan_calls` as the pending tool calls (via `TurnContext.plan_buffer`).
  - Reset `self.plan_confirmed` and `self.pending_plan_calls`.

Add public methods:
```rust
pub fn confirm_plan(&mut self) { self.plan_confirmed = true; }
pub fn reject_plan(&mut self, reason: &str) { /* inject rejection messages into transcript */ }
```

**Note**: Look at how `agent.rs` implements `confirm_plan()` and `reject_plan()` for
reference on the rejection approach (inject tool error messages into transcript).

`TurnContext` needs two new fields:
```rust
pub plan_confirmed: bool,
pub plan_buffer: Option<Vec<crate::llm::ToolCall>>,
```
And `kernel.run()` passes them to `RunCore`:
```rust
plan_confirmed: ctx.plan_confirmed,
plan_buffer: ctx.plan_buffer,
```

### Step 3 — Replace `build_agent()` with `build_runtime()`

Replace the `build_agent()` function in `main.rs` with a new `build_runtime()`
that returns `AgentRuntime`:

```rust
async fn build_runtime(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<Message>,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    event_sink: Arc<dyn EventSink>,
) -> anyhow::Result<AgentRuntime> { ... }
```

Key changes vs `build_agent()`:
- Remove `on_message: Option<OnMessageFn>` parameter (no longer needed).
- Return `AgentRuntime` directly instead of `(Agent, rx)`.
- Use `AgentRuntimeBuilder` instead of `Agent::builder()`.
- Pass `event_sink` to the builder instead of wiring an mpsc channel.

### Step 4 — Migrate `run_once()`

Replace the body of `run_once()`:

```rust
// Event streaming
let (sink, rx) = ChannelSink::new();
let runtime = build_runtime(&config, ..., Arc::new(sink), ...).await?;

// Session persistence: track transcript length before run
let pre_len = runtime.transcript().len();

let printer = tokio::spawn(stream_events(rx));   // now reads AgentEvent
let outcome = loop {
    let o = runtime.run(goal.clone()).await?;
    if !matches!(o.finish_reason, FinishReason::PlanPending) { break o; }
    // Plan confirmation (same logic as current, but via runtime.confirm_plan())
    let plan_text = o.final_text.as_deref().unwrap_or("(no plan)");
    eprintln!("\n=== Proposed Plan ===\n{plan_text}");
    eprint!("Confirm plan? [Y/n] ");
    // ... read input ...
    if confirmed { runtime.confirm_plan(); } else { runtime.reject_plan("User rejected"); break o; }
};
drop(runtime);   // closes the sink → printer task exits
printer.await.ok();

// Write session: new messages since pre_len
if let Some(sw) = session_writer {
    for msg in &all_transcript[pre_len..] { sw.append(msg)?; }
    sw.finish(finish_status)?;
}
```

Remove `on_message` wiring. Remove `build_agent()` call. Remove `drop(agent)`.

### Step 5 — Migrate `run_resumed()`

Same as `run_once()` but pass `seed` to the builder via `.seed_transcript(seed)`.

### Step 6 — Migrate `repl()`

```rust
let (sink, rx) = ChannelSink::new();
// Drop rx — no events before first turn
drop(rx);

let mut runtime = build_runtime(&config, ..., Arc::new(sink), ...).await?;

loop {
    // Print prompt, read line...
    
    // Create per-turn sink+rx
    let (turn_sink, turn_rx) = ChannelSink::new();
    runtime.set_event_sink(Arc::new(turn_sink));
    
    let printer = tokio::spawn(stream_events_repl(turn_rx));
    match runtime.run(goal.to_string()).await { ... }
    runtime.set_event_sink(Arc::new(NullSink));
    drop(printer);
    // Transcript auto-accumulated — no set_transcript() needed
    // Session: write new messages since last turn
}
```

Add `AgentRuntime::set_event_sink(sink: Arc<dyn EventSink>)` to runtime.rs.

The `:clear` command calls `runtime.set_transcript(Vec::new())` (already exists).

### Step 7 — Migrate `run_loop()`

```rust
let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));
let wakeup_slot_clone = wakeup_slot.clone();

// Build tools with ScheduleWakeup
let mut tools = build_tools(&config).await;
tools.register_mut(Arc::new(ScheduleWakeup::new(wakeup_slot_clone)));

// Build runtime (no special event sink for loop mode — use NullSink or ChannelSink)
let (sink, rx) = ChannelSink::new();
let mut runtime = build_runtime_with_tools(&config, tools, ..., Arc::new(sink)).await?;
let printer = tokio::spawn(stream_events(rx));

let outcomes = runtime.run_loop(&goal, &wakeup_slot).await?;
// or run_event_loop if bg_manager needed

drop(runtime);
printer.await.ok();
```

Note: `build_runtime()` currently always calls `build_tools()` internally. Since
`run_loop()` needs to add `ScheduleWakeup` BEFORE building the runtime, you may
need a variant `build_runtime_with_tools()` that accepts a pre-built tool registry,
or refactor `build_runtime()` to accept an optional pre-built tools parameter.

### Step 8 — Update event streaming functions

The existing `stream_events()`, `stream_events_repl()`, `stream_events_json()`
read from `mpsc::UnboundedReceiver<StepEvent>`. Change them to
`mpsc::UnboundedReceiver<AgentEvent>`:

```rust
async fn stream_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AssistantText { text, step } => { ... }
            AgentEvent::ToolCall { name, arguments, step, .. } => { ... }
            AgentEvent::ToolResult { name, output, step, .. } => { ... }
            AgentEvent::TurnFinished { reason, steps } => { ... }  // was: StepEvent::Finished
            AgentEvent::Latency { step, llm_ms } => { ... }
            AgentEvent::Compacted { removed, kept, summary_chars, step } => { ... }
            AgentEvent::PlanProposed { plan_text, .. } => { ... }
            AgentEvent::PlanConfirmed => { ... }
            AgentEvent::PlanRejected { reason } => { ... }
            _ => {}
        }
    }
}
```

Note: `AgentEvent` uses `#[non_exhaustive]` so the `_ => {}` arm is required.

### Step 9 — Update imports in `main.rs`

Remove:
```rust
use recursive::OnMessageFn;
use recursive::{Agent, AgentRunner, ...};
```

Add:
```rust
use recursive::{AgentRuntime, AgentRuntimeBuilder, RuntimeOutcome};
use recursive::event::{AgentEvent, ChannelSink, NullSink};
```

Keep `FinishReason`, `PlanningMode`, `StepEvent` (if still needed), etc.

Remove `AgentRunner` from `lib.rs` exports only if all consumers are migrated
(check that runner.rs tests still compile). For now, just remove it from main.rs
imports — don't remove the export from lib.rs yet (Goal L handles that).

### Step 10 — Tests

Update `main.rs` tests:
- `build_agent_construction_smoke` → rename to `build_runtime_construction_smoke`,
  call `build_runtime()` instead of `build_agent()`.

Add:
- `runtime_confirm_plan_does_not_panic` — smoke test for the new `confirm_plan()` method.
- `runtime_set_event_sink_works` — confirm `set_event_sink()` method compiles.

## Acceptance

- `cargo test` green (all existing tests pass)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `build_agent()` function is removed from `main.rs`
- `AgentRunner` import is removed from `main.rs`
- `OnMessageFn` import is removed from `main.rs`
- All three CLI modes (run, repl, loop) use `AgentRuntime`
- `stream_events*` functions accept `AgentEvent` (not `StepEvent`)
- Event streaming works end-to-end (events emitted by kernel reach the CLI printer)

## Notes for the agent

### Reading order

1. `src/runtime.rs` — full file (AgentRuntime, AgentRuntimeBuilder, run(), run_loop())
2. `src/kernel.rs` — TurnContext struct + kernel.run() (lines 40-233)
3. `src/agent.rs` — RunCore struct, confirm_plan(), reject_plan() (search for these)
4. `src/event.rs` — AgentEvent enum, ChannelSink::new()
5. `src/main.rs` — build_agent(), run_once(), repl(), run_loop() (the targets)

### Key API facts

- `ChannelSink::new()` → `(ChannelSink, mpsc::UnboundedReceiver<AgentEvent>)`
- `AgentRuntimeBuilder::event_sink(sink: Arc<dyn EventSink>)` — after Step 1
- `AgentRuntime::set_transcript(Vec<Message>)` — already exists
- `AgentRuntime::transcript()` — already exists (returns `&[Message]`)
- `AgentRuntime::run_loop(goal, &wakeup_slot)` — already exists in runtime.rs
- `FinishReason::PlanPending` — same type used by both Agent and Runtime outcomes

### Session persistence approach

Session persistence via `on_message` is replaced with **batch write after each turn**:
```rust
let prev_len = runtime.transcript().len();  // snapshot before run()
let outcome = runtime.run(goal).await?;
// Write new messages
let transcript = runtime.transcript();
for msg in &transcript[prev_len..] {
    if let Ok(mut w) = session_writer.lock() { let _ = w.append(msg); }
}
```

Note: the first message in any run is the user message (appended by runtime.run()
before kernel executes). So `transcript[prev_len..]` includes the user message plus
all kernel-produced messages.

### Patch strategy

Prefer `apply_patch` over `write_file`. The changes span:
- `src/kernel.rs` (add fields to TurnContext + wire them in kernel.run())
- `src/runtime.rs` (wire EventSink, add plan methods, add set_event_sink)
- `src/main.rs` (biggest change — replace build_agent, update all run functions)

**Only modify these three files**: `src/kernel.rs`, `src/runtime.rs`, `src/main.rs`.
Do NOT touch `agent.rs`, `event.rs`, `runner.rs`, `lib.rs`, or any tool files.

### Pitfalls

1. `AgentEvent` is `#[non_exhaustive]` — always include `_ => {}` in match arms.
2. `Arc<dyn EventSink>` requires `EventSink: Send + Sync + 'static` — check that ChannelSink satisfies this.
3. The forwarder task spawned in runtime.run() must complete before the outcome
   is returned — otherwise events may be lost. Use `task.await.ok()` after dropping `step_tx`.
4. In `run_loop()`, a single `(sink, rx)` pair is created before the loop.
   The runtime reuses the same sink across all turns in the loop — this is correct.
5. `stream_events_json` serializes AgentEvent with serde — it already derives
   `Serialize`/`Deserialize`, so this works unchanged except for the type change.
6. The `run_resumed()` helper shares structure with `run_once()` — migrate it the same way.

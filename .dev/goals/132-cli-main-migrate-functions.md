# Goal 132 — Migrate main.rs run functions to AgentRuntime

**Roadmap**: Kernel Architecture Refactor — Phase 4c continuation

**Context**: Goal 131 completed Steps 1-2 (kernel.rs + runtime.rs). This goal
completes the CLI migration by updating the four run functions in main.rs plus
updating imports and event streaming helpers.

**What is already done** (do NOT redo these):
- `src/kernel.rs`: TurnContext has `step_events_tx`, `plan_confirmed`, `plan_buffer`
- `src/runtime.rs`: EventSink wired, `confirm_plan()`, `reject_plan()`, `set_event_sink()`
- `src/lib.rs`: exports `AgentRuntime`, `AgentRuntimeBuilder`, `RuntimeOutcome`
- `src/event.rs`: `ChannelSink::new()`, `CompositeSink`, `NullSink` all available

## Critical approach note — patch ambiguity workaround

`main.rs` has two nearly-identical functions (`run_once` and `run_resumed`) that
share the same boilerplate blocks. V4A `apply_patch` fails with "hunk matches 2
locations" on these duplicate patterns.

**Solution**: For any hunk that fails with "matches N locations", immediately
switch to **rewriting the entire function body** using `write_file`. The workflow:

1. `read_file` the function (give start/end lines)
2. Prepare the complete new function text
3. Call `write_file` on a NEW temporary file e.g. `src/main_patch_tmp.rs` with
   ONLY the new function body
4. Use `apply_patch` with the function signature as the unique anchor to splice
   in the new body

Actually, the cleanest approach: **rewrite main.rs entirely**.
- `read_file src/main.rs` (all 2310 lines — you may need several reads)  
- Prepare the complete updated file content
- `write_file src/main.rs` with the full updated content

**AGENTS.md** explicitly allows `write_file` for "whole-file rewrites when you
have read the entire current contents and intentionally want to replace them."
This qualifies.

## Scope

### 1. New imports to add

Replace in the `use recursive::{ ... }` block:
```rust
// REMOVE:
Agent, AgentRunner, FinishReason, PlanningMode, RetryPolicy, StepEvent, ToolRegistry,
OnMessageFn,

// ADD:
AgentRuntime, AgentRuntimeBuilder, FinishReason, PlanningMode, RetryPolicy, ToolRegistry,
RuntimeOutcome,
```

Also add to top-level use statements:
```rust
use recursive::event::{AgentEvent, ChannelSink, CompositeSink, EventSink, NullSink};
use std::sync::Arc;  // already present — keep
```

Remove: `use tokio::sync::mpsc;` (no longer needed if events go through EventSink).
Check: if `mpsc` is still used elsewhere in main.rs (the REPL uses it); keep it
if needed.

### 2. New helper struct — `SessionWriterSink`

Add before `run_resumed`:
```rust
/// EventSink that appends every new assistant message to a SessionWriter.
struct SessionWriterSink {
    writer: Arc<std::sync::Mutex<SessionWriter>>,
}

impl EventSink for SessionWriterSink {
    async fn emit(&self, event: AgentEvent) {
        if let AgentEvent::AssistantText { text, .. } = event {
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.append(&recursive::message::Message::assistant(text));
            }
        }
    }
}
```

Note: `EventSink` is an `async_trait`. Check if the crate already has
`async-trait` as a dependency (it does — check `Cargo.toml`).
If not, use a wrapper with `tokio::task::block_in_place` or remove the async
from the trait impl. Actually, looking at event.rs — `EventSink` uses
`async fn emit` directly with `#[async_trait]`. So `SessionWriterSink` needs
`#[async_trait::async_trait]` as well, and `async-trait` must be in Cargo.toml.

**Check first**: `grep async-trait Cargo.toml`. If not present, add it.

### 3. New `build_runtime()` function

Replace `build_agent()` with:
```rust
#[allow(clippy::too_many_arguments)]
async fn build_runtime(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    event_sink: Option<Arc<dyn EventSink>>,
) -> anyhow::Result<AgentRuntime> {
    let api_key = config.require_api_key()?;
    // ... same provider building logic as build_agent ...
    // ... same tools building logic ...
    // ... same system_prompt building logic ...
    
    let mut builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps);
    
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
    if let Ok(threshold) = std::env::var("RECURSIVE_COMPACT_THRESHOLD") {
        if let Ok(n) = threshold.parse::<usize>() {
            if n > 0 {
                builder = builder.compactor(recursive::Compactor::new(n));
            }
        }
    }
    if hook_timing {
        // hooks: hook_timing → HookRegistry with ToolTimingHook
        // Check how AgentRuntimeBuilder accepts hooks
        let mut hooks = recursive::hooks::HookRegistry::new();
        hooks.register(Arc::new(recursive::hooks::ToolTimingHook::new()));
        builder = builder.hooks(hooks);
    }
    builder = builder.streaming(stream);
    if plan_first {
        builder = builder.planning_mode(PlanningMode::PlanFirst);
    }
    if let Some(sink) = event_sink {
        builder = builder.event_sink(sink);
    }
    builder.build().map_err(Into::into)
}
```

Keep `build_agent()` temporarily (it's used by the test), OR update the test.

### 4. Updated event streaming helpers

Rename/replace `stream_events`, `stream_events_repl`, `stream_events_json` to
work with `AgentEvent` instead of `StepEvent`:

```rust
async fn stream_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AssistantText { text, step } => { ... }
            AgentEvent::ToolCall { name, arguments, step, .. } => { ... }
            AgentEvent::ToolResult { name, output, step, .. } => { ... }
            AgentEvent::TurnFinished { reason, steps } => { ... }
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

`AgentEvent` is `#[non_exhaustive]` so `_ => {}` is required.

### 5. Migrate `run_once()`

Pattern:
```rust
// Create ChannelSink for event streaming
let (channel_sink, event_rx) = ChannelSink::new();

// If session recording, wrap in CompositeSink
let event_sink: Arc<dyn EventSink> = if let Some(ref sw) = session_writer {
    Arc::new(CompositeSink::new(vec![
        Box::new(channel_sink) as Box<dyn EventSink>,
        Box::new(SessionWriterSink { writer: sw.clone() }) as Box<dyn EventSink>,
    ]))
} else {
    Arc::new(channel_sink)
};

let mut runtime = build_runtime(&config, ..., Some(event_sink)).await?;

// Spawn event printer
let printer = if json_mode {
    tokio::spawn(stream_events_json(event_rx))
} else {
    tokio::spawn(stream_events(event_rx))
};

// Run with plan confirmation loop if needed
let outcome = loop {
    let o = runtime.run(goal.clone()).await?;
    if !matches!(o.finish_reason, FinishReason::PlanPending) { break o; }
    let plan_text = o.final_text.as_deref().unwrap_or("(no plan)");
    eprintln!("\n=== Proposed Plan ===\n{plan_text}");
    eprint!("Confirm plan? [Y/n] ");
    use std::io::Write; let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    if input.is_empty() || input == "y" || input == "yes" {
        runtime.confirm_plan();
    } else {
        runtime.reject_plan("User rejected the plan");
        break o;
    }
};

drop(runtime);
printer.await.ok();

// Session finalize (writer was already recording via SessionWriterSink)
let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
    "success"
} else {
    "incomplete"
};
if let Some(sw) = session_writer {
    // finalize
}
```

Note: `AgentOutcome` fields → `RuntimeOutcome` fields:
- `outcome.finish` → `outcome.finish_reason`  
- `outcome.final_message` → `outcome.final_text`
- `outcome.total_llm_latency_ms` → `outcome.llm_latency_ms`
- `outcome.transcript` → use `runtime.transcript()` (but runtime was dropped!)

**Important**: capture transcript BEFORE dropping runtime:
```rust
let transcript = runtime.transcript().to_vec();
drop(runtime);
// ... then use transcript for save_transcript() if needed
```

### 6. Migrate `run_resumed()`

Same pattern as `run_once()` but pass `seed` via `build_runtime(... seed, ...)`.
Remove `on_message` wiring. The `session_writer` is already handled by `SessionWriterSink`.

### 7. Migrate `repl()`

```rust
// Create runtime once
let (channel_sink, _rx) = ChannelSink::new();
drop(_rx);  // no events before first turn
let mut runtime = build_runtime(&config, ..., Some(Arc::new(NullSink))).await?;

loop {
    // Read input...
    if goal == ":clear" {
        runtime.set_transcript(Vec::new());
        continue;
    }
    
    // Per-turn event sink
    let (turn_sink, turn_rx) = ChannelSink::new();
    runtime.set_event_sink(Arc::new(turn_sink));
    
    let printer = if json_mode {
        tokio::spawn(stream_events_json(turn_rx))
    } else {
        tokio::spawn(stream_events_repl(turn_rx))
    };
    
    match runtime.run(goal.to_string()).await {
        Ok(outcome) => {
            runtime.set_event_sink(Arc::new(NullSink));
            printer.await.ok();
            // print usage, finish note
            // Transcript auto-accumulated — no set_transcript() needed
            total_turns += 1;
        }
        Err(e) => {
            runtime.set_event_sink(Arc::new(NullSink));
            printer.await.ok();
            eprintln!("error: {e}");
        }
    }
}
```

### 8. Migrate `run_loop()`

```rust
let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));
let wakeup_slot_clone = wakeup_slot.clone();

let mut tools = build_tools(&config).await;
register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;
tools.register_mut(Arc::new(ScheduleWakeup::new(wakeup_slot_clone)));

// Build runtime with custom tools
// NOTE: build_runtime() calls build_tools() internally, which doesn't include ScheduleWakeup.
// We need a variant. Options:
// a) Add a build_runtime_with_tools() that accepts pre-built tools
// b) Or use AgentRuntimeBuilder directly inline (copy the provider/system_prompt logic)
// Option (b) is simpler for now.

let (channel_sink, event_rx) = ChannelSink::new();
// Build the provider (same as build_runtime does internally)...
let mut runtime = AgentRuntimeBuilder::new()
    .llm(provider)
    .tools(tools)
    .system_prompt(&config.system_prompt)
    .max_steps(config.max_steps)
    .event_sink(Arc::new(channel_sink))
    .build()?;

let printer = tokio::spawn(stream_events(event_rx));
let outcomes = runtime.run_loop(&goal, &wakeup_slot).await?;
drop(runtime);
printer.await.ok();
```

### 9. Update `main()` dispatch

In `match effective_cmd { ... }`:
- `Cmd::Run { goal }` → update to remove `shutdown` param if not needed, or keep
- `Cmd::Repl` → update signature call
- `Cmd::Loop { goal }` → update signature call
- `Cmd::Resume { session }` → keep calling `run_resumed()` (just updated internally)
- `Cmd::Replay { ..., resume_from: Some(n) }` → keep calling `run_resumed()`

### 10. Remove dead code

After migration:
- Remove `build_agent()` function (or remove the `on_message` param first, then remove)
- Remove `AgentRunner` import
- Remove `OnMessageFn` import  
- Remove `StepEvent` import (if no longer used)
- Remove `Agent` import

### 11. Update tests

The test `build_agent_construction_smoke` calls `build_agent()`. After removing
`build_agent()`, update it to call `build_runtime()` instead.

## Acceptance

- `cargo build` clean (no errors)
- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `main.rs` no longer imports `Agent`, `AgentRunner`, `OnMessageFn`, `StepEvent`
- `main.rs` has `build_runtime()` replacing `build_agent()`
- All four modes (run, repl, loop, resume) use `AgentRuntime`

## Notes for the agent

### Reading order

1. `src/main.rs` — read the ENTIRE file in multiple chunks to understand all
   functions before writing. Start with:
   - Lines 1-50 (imports)
   - Lines 950-1140 (build_agent)
   - Lines 1250-1450 (run_resumed)
   - Lines 1540-1740 (run_once)
   - Lines 1840-1960 (repl)
   - Lines 1430-1540 (run_loop)
   - Lines 1950-2050 (stream_events functions)

2. `src/runtime.rs` — check AgentRuntimeBuilder API (already known from prior read)

3. `Cargo.toml` — check if `async-trait` is a dependency

### CRITICAL: Use `write_file` for main.rs

Because `run_once` and `run_resumed` contain nearly identical blocks of code,
`apply_patch` will ALWAYS fail with "hunk matches 2 locations" on those blocks.

**DO NOT use `apply_patch` for any hunk that touches the body of `run_once` or
`run_resumed`.** Instead:

1. Read the full current content of `src/main.rs`
2. Prepare the COMPLETE updated file content (all 2300+ lines)
3. Call `write_file src/main.rs` with the complete new content

This is explicitly allowed by AGENTS.md: "write_file for whole-file rewrites
when you have read the entire current contents and intentionally want to replace them."

`apply_patch` is fine for small targeted changes (imports, adding a new struct,
adding a new function). Use it for those. Use `write_file` only for the
full-file rewrite at the end.

### async-trait

If `async-trait` is not in `Cargo.toml`, use one of:
```rust
// Option 1: Box::pin pattern (no external dep)
fn emit<'a>(&'a self, event: AgentEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output=()> + Send + 'a>> {
    Box::pin(async move { ... })
}
// Option 2: if async-trait IS in Cargo.toml
#[async_trait::async_trait]
impl EventSink for SessionWriterSink { ... }
```

### RuntimeOutcome field names vs AgentOutcome

| AgentOutcome field | RuntimeOutcome field |
|--------------------|---------------------|
| `.finish` | `.finish_reason` |
| `.final_message` | `.final_text` |
| `.total_usage` | `.total_usage` (same) |
| `.total_llm_latency_ms` | `.llm_latency_ms` |
| `.steps` | `.steps` (same) |
| `.transcript` | use `runtime.transcript()` |

Capture transcript before dropping runtime:
```rust
let transcript = runtime.transcript().to_vec();
drop(runtime);
// now use transcript for save_transcript()
```

### save_session() signature

`save_session()` takes `&recursive::AgentOutcome`. After migration, you can
either:
a) Create a minimal `AgentOutcome` from RuntimeOutcome fields (with empty transcript)
b) Or change `save_session()` to accept the individual fields directly

Option (b) is cleaner. Update `save_session()` signature to take the fields directly.

# Goal 316 — Gate plan-mode tools correctly in non-TUI CLI modes

## Why

`enter_plan_mode` and `exit_plan_mode` tools are registered with the agent
when `with_plan_mode_tools(true)` is called. These tools only work correctly
when there is a live human who can approve/reject the plan — they call
`PlanApprovalGate::wait_for_approval()` which **blocks** until someone calls
`gate.approve()` or `gate.reject()`.

Currently there are two non-TUI CLI modes that incorrectly pass
`interactive = true` to `build_runtime()`:

### 1. `run_once` (line ~1764 in `src/main.rs`)

`run_once` is the handler for `recursive run "goal"`. It passes
`true /* interactive CLI — plan mode tools enabled */` but its event handler
(`stream_events` in `src/cli/output.rs`) only **prints** a `PlanProposed`
event — it does NOT call `gate.approve()`. If the agent calls
`enter_plan_mode`, the session will **hang forever** waiting for approval that
never comes.

**Fix**: Pass `false` for `run_once`. It is a batch/headless run; there is no
human in the loop to approve plans.

### 2. REPL mode (line ~1902 in `src/main.rs`)

The REPL (`recursive repl`) also passes `interactive = true`, but
`stream_events_repl` in `src/cli/output.rs` ignores `PlanProposed` entirely.
Same hang risk.

**Fix options** (choose one):
  a. **Simplest**: Pass `false` for REPL — disable plan mode tools until a
     proper y/n prompt is implemented.
  b. **Better UX**: Implement plan approval in the REPL: when the agent fires
     a `PlanProposed` event, print the plan and ask `Approve plan? [y/n]: `.
     If `y`, call `gate.approve()`; if `n`, ask for a rejection reason and
     call `gate.reject(&reason)`.

**Recommended**: implement option (b) for the REPL since it already has a
human at the keyboard, and add a TODO comment for future reference.

## Scope

### Changes required

**`src/main.rs`**

1. In `run_once`, change the `interactive` argument from `true` to `false`:
   ```rust
   false, // headless batch run — no human to approve plans
   ```

2. In the REPL handler, either:
   - Change to `false` (simple fix), OR
   - Keep `true` but add a plan-approval loop in the REPL event thread.

**`src/cli/output.rs`** (if implementing REPL approval, option b):

Add a new async function `stream_events_repl_with_plan_approval` (or modify
`stream_events_repl`) that, on `AgentEvent::PlanProposed`, prints the plan
and reads stdin for `y/n`:

```rust
AgentEvent::PlanProposed { ref plan_text, session_id: _, .. } => {
    eprintln!("\n[plan] Agent proposed the following plan:\n{plan_text}\n");
    eprint!("Approve plan? [y/n]: ");
    // Read one char from stdin, flush, call gate.approve() or gate.reject()
}
```

But this requires access to the `PlanApprovalGate` in the event stream,
which means the event handler needs a reference to the gate. A simpler
approach is to write approval logic in the REPL main loop (not the event
stream) — see the REPL loop at lines ~1905–1970 in `src/main.rs`.

**Simplest correct fix (minimum scope)**:

Change BOTH `run_once` AND the REPL handler to pass `false`:

```rust
// run_once (line ~1764):
false, // headless batch run — no human to approve plans
       // (plan mode is available in TUI and HTTP API sessions)

// REPL (line ~1902):
false, // plan approval not yet wired into the REPL event loop;
       // use TUI or HTTP API for plan-mode sessions
```

Add a `// TODO(plan-mode-repl): implement y/n approval prompt` comment in
`stream_events_repl`.

## Tests

In `tests/integration.rs` or `tests/http.rs`, add:

```
run_once_does_not_expose_plan_mode_tools
```

Test: build a runtime with `with_plan_mode_tools(false)` and verify that
`registry.get("enter_plan_mode")` returns `None` and
`registry.get("exit_plan_mode")` returns `None`.

(This test may already be implied by `default_registry_has_no_plan_mode_tools`
in `src/tools/mod.rs` — check if a new test is needed or if the existing one
covers it.)

## Acceptance criteria

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` no diff
- `run_once` passes `false` for `interactive` to `build_runtime`
- REPL either passes `false` OR has working y/n approval prompt
- Neither `run_once` nor the REPL will hang if the agent attempts
  to call `enter_plan_mode`

## Notes for the agent

- The fix to `run_once` is 1 character: `true` → `false` at line ~1764
- The fix to REPL is also 1 character: `true` → `false` at line ~1902
- Both changes need a comment update explaining WHY `false` is used
- The TUI (`src/tui/runtime_builder.rs:75`) correctly uses `true` — do NOT
  change it
- WeChat daemon (line ~2221) already correctly uses `false` — it shows the
  right pattern
- `default_registry_has_no_plan_mode_tools` test in `src/tools/mod.rs`
  already verifies that `build_standard_tools()` does NOT include plan mode
  tools. The `with_plan_mode_tools(false)` call preserves this invariant.
- After this change, plan mode is only available in:
  - TUI (cursive) sessions
  - HTTP API sessions (via `send_session_message` which uses `interactive` 
    from its own context — check `src/http/handlers.rs`)

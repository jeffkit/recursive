# Goal 248 — Avoid full transcript clone on every kernel turn

**Roadmap**: Arch-review bugfixes (high severity / performance)

**Design principle check**:
- Implemented as: pass `Arc<Vec<Message>>` or slice ref instead of cloning
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`AgentRuntime::execute_kernel_turn()` clones the entire transcript on every
turn before passing it to the kernel. For long sessions this is a significant
allocation per turn (~1 MB/clone at 100 turns × 10 KB/turn). The kernel only
reads the transcript, so a shared reference is sufficient.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — pass transcript by reference in `execute_kernel_turn`

Read `execute_kernel_turn()` in `src/runtime.rs`. Find the line:

```rust
let messages = self.transcript.clone();
```

Check what the kernel's `run()` method expects for the `messages` field in
`TurnContext` (read `src/kernel/mod.rs` or wherever `TurnContext` is defined).

If `TurnContext.messages` is `Vec<Message>` (owned), change it to
`&[Message]` or `Arc<Vec<Message>>` — whichever requires fewer call-site
changes. Prefer `&[Message]` (a simple slice reference) if the kernel only
reads it. Update `TurnContext` definition and all its construction sites.

If changing `TurnContext` is too invasive (more than 5 call sites need
changes), a narrower fix is: wrap `self.transcript` in an `Arc<Vec<Message>>`
so the clone is a cheap pointer copy:

```rust
// In AgentRuntime, change:
transcript: Vec<Message>
// to:
transcript: Arc<Vec<Message>>
```

Then cloning for `TurnContext` is `Arc::clone(&self.transcript)` — O(1).
When appending a new message, use `Arc::make_mut(&mut self.transcript).push(msg)`.

Choose whichever approach touches fewer files. Read both before deciding.

### 2. Tests

Existing tests should continue to pass. No new tests needed beyond confirming
`cargo test --workspace` is green.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No full `Vec<Message>` clone of the transcript on every turn

## Notes for the agent

- Read `src/runtime.rs` `execute_kernel_turn` and the `TurnContext` struct
  definition before deciding which approach to take.
- Read `src/kernel/` to understand `TurnContext` usage.
- Prefer the `Arc<Vec<Message>>` approach if `TurnContext` has many
  construction sites — it minimises the blast radius.
- CRITICAL: Run `cargo test --workspace` (not just `cargo check` or
  `cargo test --lib`) to verify the full test suite passes before declaring
  done. Integration tests in `tests/` must also compile and pass.
- CRITICAL: After any struct change, search `grep -rn "TurnContext {" src/ tests/`
  to find ALL construction sites and update them.
- **DO NOT modify** `src/agent.rs`, `src/run_core.rs`, `src/llm/`, `src/config.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.

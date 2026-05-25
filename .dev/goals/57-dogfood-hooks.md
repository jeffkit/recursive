# Goal 57 — Dogfood Lifecycle Hooks in self-improve

**Roadmap**: validation — wire Hooks (g48) into self-improve loop

**Design principle check**:
- Implemented as: dev-infra change to `src/main.rs` (register a built-in
  `ToolTimingHook` when a flag is set). No new product modules.
- Does NOT modify agent loop logic.

## Why

Lifecycle Hooks (g48) landed with 11 tests but have never fired in a real
agent run. Wiring a simple hook into the CLI exercises the full dispatch
path under real LLM traffic — the same pattern that caught the Compactor
orphan-tool bug in batch 15.

## Scope (do exactly this, no more)

### 1. `src/hooks.rs` — add a built-in `ToolTimingHook`

A simple hook that:
- On `PostToolCall`: prints to stderr: `[hook] {name} took {duration_ms}ms`
- On all other events: `HookAction::Continue`

This is a **library-shipped** example hook, not just a test helper.

### 2. `src/main.rs` — wire it when `--hook-timing` is set

Add a CLI flag `--hook-timing` (or env `RECURSIVE_HOOK_TIMING=1`):
- When set, create a `HookRegistry`, register `ToolTimingHook`, pass to
  `AgentBuilder::hooks(registry)`

### 3. Add `AgentBuilder::hooks()` if not present

Check if the builder already accepts a `HookRegistry`. If not, add the
method (small plumbing — just stores it in the Agent struct and calls
`registry.dispatch()` at the lifecycle points already wired in g48).

### 4. Tests

- Test: `ToolTimingHook` emits correct format on `PostToolCall`
- Test: `--hook-timing` flag is recognized by CLI parser
- Integration: Agent with `ToolTimingHook` registered completes a run
  without panic (proves dispatch path works end-to-end)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- `recursive run --hook-timing "list files in src"` prints timing lines
  to stderr for each tool call
- No regressions

## Notes for the agent

- Read `src/hooks.rs` for `HookEvent`, `Hook` trait, `HookRegistry`.
- Read `src/agent.rs` for where hooks are dispatched (search for
  "hook" or "HookRegistry").
- The key validation: does `registry.dispatch(HookEvent::PostToolCall{...})`
  actually get called during a real run? If the agent.rs wiring from g48
  is incomplete, this goal will expose it.
- Keep the `ToolTimingHook` simple — it's a validation vehicle, not a
  feature. One line of stderr output per tool call is enough.

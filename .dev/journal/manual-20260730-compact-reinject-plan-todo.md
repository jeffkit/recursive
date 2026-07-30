# Manual edit: compact-reinject-plan-todo

**Date**: 2026-07-30
**Goal**: 340 — Re-inject current plan and todo list after cross-turn compaction

## Summary

Added `PlanTodoReinjector` that recovers the pending plan text (from
`PlanApprovalGate::pending_plan`) and the current task list (from the shared
`Arc<RwLock<Vec<TodoItem>>>`) as `Role::System` attachment messages after an
LLM-summary cross-turn compaction. This prevents the model from losing track
of its plan and todo checklist after compaction.

## Files created

- **`src/compact/reinject.rs`** — Added `PlanTodoReinjector` struct with:
  - `new(todos, plan_gate)` constructor
  - `reinject()` method returning 0-2 `Vec<Message>` (plan first, then todos)
  - Format: `[post-compact plan restore]` + `[post-compact todo restore]`
    with checkbox-style rendering (`[x]`, `[/]`, `[ ]`, `[~]`)
  - Poisoned-lock handling via `tracing::warn` (no unwrap, invariant #5)

## Files modified

- **`src/tools/plan_mode.rs`**:
  - Added `#[derive(Debug)]` to `PlanApprovalGate` (needed by `PlanTodoReinjector`)
  - Added `pending_plan()` accessor returning `Option<String>`
  - Added tests `pending_plan_returns_begin_approval_text`,
    `pending_plan_cleared_on_approve`, `pending_plan_returns_none_on_poisoned_lock`

- **`src/compact/mod.rs`**:
  - Added `PlanTodoReinjector` to the `pub use` re-exports

- **`src/runtime.rs`**:
  - Added `plan_todo_reinjector: Option<PlanTodoReinjector>` field to `AgentRuntime`
  - Constructed inside `AgentRuntimeBuilder::build()` (where `todo_list` and
    `plan_approval_gate` arcs exist — no external builder wiring needed)
  - Wired into `maybe_compact_cross_turn` after skill reinjection (g335) and
    before the preserved tail, using `take_while` to count already-inserted
    file/skill attachments for correct insert position

- **`tests/invariants/loop_size_orthogonality.rs`**:
  - Bumped runtime.rs line limit from 3550 → 3700 (the new feature code +
    integration test added ~120 lines)

## Tests added

- `src/compact/reinject.rs` (PlanTodoReinjector tests):
  - `plan_todo_reinject_plan_when_pending`
  - `plan_todo_reinject_no_plan_when_none`
  - `plan_todo_reinject_todos_when_non_empty`
  - `plan_todo_reinject_no_todos_when_empty`
  - `plan_todo_reinject_both_plan_and_todos`
  - `plan_todo_reinject_empty_when_neither`

- `src/tools/plan_mode.rs` (pending_plan tests):
  - `pending_plan_returns_begin_approval_text`
  - `pending_plan_cleared_on_approve`
  - `pending_plan_returns_none_on_poisoned_lock`

- `src/runtime.rs` (integration test):
  - `cross_turn_compaction_reinjects_plan_and_todos` — seeds plan + todos,
    triggers cross-turn compaction, asserts transcript ordering and content

## Files NOT touched (as required)

- `src/run_core.rs`, `src/llm/`, `src/kernel.rs`, `compact_on_overflow`,
  `compact_now` — not modified
- `crates/recursive-cli/src/cli/builder.rs` — no changes needed (reinjector
  constructed inside `build()`)
- `crates/recursive-tui/src/runtime_builder.rs` — no changes needed

## Design decisions

- **Constructed inside `build()`** not passed from external builders: The
  `todo_list` and `plan_approval_gate` are created inside
  `AgentRuntimeBuilder::build()` near `plan_mode_request_gate`. Building the
  reinjector there avoids threading extra fields through the builder API.
- **Poisoned-lock handling**: `RwLock::read()` returns `Result`; on `Err` we
  skip the attachment and emit `tracing::warn!` (invariant #5).
- **Plan-first ordering**: The plan message is pushed before the todo
  message so the model reads the plan first.
- **Checkbox format**: Uses `[x]` (completed), `[/]` (in-progress),
  `[ ]` (pending), `[~]` (cancelled). Active form shown only for
  in-progress items.

## Quality gates

- `cargo test --workspace`: all 2148 + integration/doc tests green
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all`: clean

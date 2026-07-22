# Goal 340 — Re-inject current plan and todo list after cross-turn compaction

**Roadmap**: Compaction upgrade (WS-3c — post-compact plan/todo restoration)

**Design principle check**:
- Implemented as: a `PlanTodoReinjector` in `src/compact/reinject.rs`, invoked
  from `src/runtime.rs::maybe_compact_cross_turn` after the skill reinjector
  (goal 335).
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT emit `Role::Tool` messages — only `Role::System` attachments
  (invariant #8 safe).

## Why

When the agent is in plan mode or has a live todo list, those states live in
the transcript (the proposed plan text, the `TodoWrite` tool results). After
an LLM-summary compaction they are folded into prose or dropped, so the model
loses track of the pending plan and the todo checklist — it may re-propose a
plan or forget outstanding tasks. fake-cc re-injects the plan file reference
and the plan-mode indicator post-compact so the model keeps operating in
plan mode.

Recursive already has the data sources:
- Todos: `AgentRuntime::current_todos()` reads the shared
  `Arc<RwLock<Vec<TodoItem>>>` (`src/tools/todo.rs:5`).
- Pending plan: `PlanApprovalGate::begin_approval` stores the plan text
  (`src/tools/plan_mode.rs:138`); an accessor is needed (see scope).

This goal re-injects both as `Role::System` attachments after the skill
attachments, so plan-mode and todo state survive compaction.

## Scope (do exactly this, no more)

### 1. `src/tools/plan_mode.rs` — expose pending plan read accessor

Add (if not already present — grep `pending_plan`/`current_plan`):
```rust
impl PlanApprovalGate {
    /// Return the currently-pending plan text, if any (set by
    /// `begin_approval`, cleared on approve/reject). Used by post-compact
    /// re-injection so the model retains the plan across compaction.
    pub fn pending_plan(&self) -> Option<String> { /* read the RwLock */ }
}
```
If the field is `Option<String>` behind an `RwLock`, read it and clone. Add
a unit test `pending_plan_returns_begin_approval_text`.

### 2. `src/compact/reinject.rs` — `PlanTodoReinjector`

```rust
use std::sync::{Arc, RwLock};
use crate::message::Message;
use crate::tools::todo::TodoItem;
use crate::tools::plan_mode::PlanApprovalGate;

#[derive(Debug, Clone)]
pub struct PlanTodoReinjector {
    pub todos: Arc<RwLock<Vec<TodoItem>>>,
    pub plan_gate: Arc<PlanApprovalGate>,
}

impl PlanTodoReinjector {
    /// Return 0-2 System attachment messages: the pending plan (if any) and
    /// the current todo list (if non-empty). Empty Vec if neither applies.
    pub fn reinject(&self) -> Vec<Message> { /* ... */ }
}
```

`reinject`:
1. Read `self.plan_gate.pending_plan()`. If `Some(plan)`, emit one
   `Message::system`:
   ```
   [post-compact plan restore]
   You are in plan mode. The pending plan (awaiting approval) is:
   <plan>
   ```
2. Read `self.todos` (lock the `RwLock`); if non-empty, emit one
   `Message::system`:
   ```
   [post-compact todo restore]
   Current task list:
   - [x] done item
   - [ ] pending item
   ...
   ```
   Format each `TodoItem` with its status/content fields (confirm the exact
   field names by reading `TodoItem` at `src/tools/todo.rs:34`).
3. Return the Vec (plan first, then todos). Empty if both absent.

### 3. `src/compact/mod.rs` — register

`pub use reinject::PlanTodoReinjector;`.

### 4. `src/runtime.rs` — wire cross-turn

- Add field `plan_todo_reinjector: Option<PlanTodoReinjector>`; builder setter.
- In `maybe_compact_cross_turn`, after the skill attachments (goal 335),
  insert the plan/todo attachments before the preserved tail:
  `[summary, file-atts, skill-atts, plan/todo-atts, ...preserved]`.
  Emit `MessageAppended` for each.
- `compact_on_overflow` / `compact_now` do NOT reinject plan/todo in this
  goal (follow-up); cross-turn only.

### 5. Builder wiring (`crates/recursive-cli/src/cli/builder.rs`)

`build_runtime` already constructs the shared todo `Arc<RwLock<Vec<TodoItem>>>`
(`builder.rs:138` `TodoWriteTool::new(Arc::new(RwLock::new(vec![])), ...)`) and
the `PlanApprovalGate` is built inside the runtime/kernel. Thread the SAME
todo `Arc` and the plan gate `Arc` into `PlanTodoReinjector`. If the plan
gate is constructed inside `AgentRuntimeBuilder::build()` (not accessible at
`build_runtime`), expose it via a builder accessor or construct the
reinjector inside `build()` where the gate is available — confirm the gate's
construction site by reading `runtime.rs` around the `PlanApprovalGate`
usage. Mirror in TUI `runtime_builder.rs`.

### 6. Tests

`src/compact/reinject.rs`:
- `reinject_plan_when_pending` — gate has a pending plan → one System
  message containing the plan text.
- `reinject_no_plan_when_none` — gate empty → no plan message.
- `reinject_todos_when_non_empty` — todos present → one System message
  listing them.
- `reinject_no_todos_when_empty` — empty todo list → no todo message.
- `reinject_both_plan_and_todos` — both present → two messages, plan first.
- `reinject_empty_when_neither` → empty Vec, no panic.

`src/runtime.rs`:
- `cross_turn_compaction_reinjects_plan_and_todos` — seed a pending plan
  + todos, trigger compaction, assert transcript ordering
  `[summary, file-atts, skill-atts, plan-att, todo-att, ...recent]`.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- Only `Role::System` attachments; `tool_call_pairing.rs` green.
- No `unwrap`/`expect` on the `RwLock` (use `match`/`?` on the lock result,
  invariant #5) — `RwLock::read` can be poisoned; handle `Err` by skipping
  that attachment (log via `tracing::warn`).
- `pending_plan` accessor added with a unit test.

## Notes for the agent

- **RwLock poisoning:** `RwLock::read()` returns `Result`. Do NOT `unwrap`.
  On `Err`, skip the attachment and `tracing::warn!`. Same for the todo
  `RwLock`.
- **Confirm `TodoItem` field names** (`status`, `content`, `active`?) by
  reading `src/tools/todo.rs:34` before formatting. A wrong field name fails
  to compile, so this is safe — just don't guess in the test expectations.
- **Confirm the `PlanApprovalGate` construction site** — if it is built
  inside `AgentRuntimeBuilder::build()`, the reinjector must be constructed
  there too (where the gate `Arc` exists), not in `build_runtime`. Adjust the
  wiring accordingly; the goal's intent is "use the same gate instance the
  runtime uses," not "construct it in `build_runtime` specifically."
- Plan-mode *indicator* (telling the model it is still in plan mode) is as
  important as the plan text — without it the model may resume normal
  execution. Include the "You are in plan mode" line.
- **DO NOT modify** `src/run_core.rs`, `src/llm/`, `src/kernel.rs`,
  `compact_on_overflow`, `compact_now`, or tool files beyond the
  `pending_plan` accessor on `PlanApprovalGate`.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-reinject-plan-todo.md`.

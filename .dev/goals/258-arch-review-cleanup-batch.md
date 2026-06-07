# Goal 258 — Architecture review P3 cleanup batch (L-1, L-3, L-4)

**Roadmap**: Code quality — architecture review follow-up (P3 backlog)

**Design principle check**:
- Implemented as: small targeted cleanups in 3 files (runtime.rs, runtime_goal.rs, kernel.rs)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Architecture review (`docs/review/architecture-review-2026-06-07.md`) identified
three P3 dead-code / stale-documentation items that have been left in the
codebase. None are correctness bugs, but they mislead readers and complicate
the type system. Cleaning them up improves reviewability of the goal loop and
runtime code without touching behavior.

## Scope (do exactly this, no more)

### 1. L-1 — Remove `parent_agent_last_uuid` dead field (P3)

File: `src/runtime.rs`

- Line 1093–1094: doc comment + field `parent_agent_last_uuid: Option<String>`
- Line 1136: builder init `parent_agent_last_uuid: None,`
- Line 1156–1159: builder setter `pub fn parent_agent_last_uuid(mut self, uuid: impl Into<String>) -> Self`

The field is documented as "Reserved for future multi-agent orchestration.
Not yet wired to event emission." and is set by the builder but never read
in `build()`. It's dead public API.

**Action**: Remove the field, the builder init, the builder method, and the
doc comment. Also remove any test that calls `parent_agent_last_uuid(...)`
on the builder (search the test file first; if a test relies on it, that
test is also dead — remove it).

### 2. L-3 — Remove `GoalStatus::Cleared` dead variant (P3)

Files: `src/runtime_goal.rs`, `src/runtime.rs`

The variant is WRITE-ONLY: it's set on `GoalState.status` at runtime.rs:782
(in `clear_goal()`) and runtime.rs:848 (in budget exceeded) but in both
cases the enclosing `Option<GoalState>` is immediately overwritten with
`None` via `*g = None`, dropping the GoalState. Nothing reads the
`Cleared` variant.

- `src/runtime_goal.rs:22-29`: enum definition — remove the `Cleared` arm
  and its doc comment
- `src/runtime_goal.rs:158`: serde roundtrip test for `Cleared` — remove
  this test or replace with another variant
- `src/runtime.rs:782`: `s.status = GoalStatus::Cleared;` — remove this line
  (and adjust the surrounding if-let-Some pattern)
- `src/runtime.rs:848`: `gs.status = GoalStatus::Cleared;` — remove this line

The observable `AgentEvent::GoalCleared` is still emitted at runtime.rs:786
and is what callers actually listen to. The `Cleared` variant is
unnecessary indirection.

### 3. L-4 — Remove stale `SideEffect` comment from kernel.rs module doc (P3)

File: `src/kernel.rs`

Lines 5–12: module doc claims `SideEffect` is a first-class exported type:

```
//! * [`SideEffect`] — side effects that outlive the turn (background jobs,
//!   scheduled wakeups).
```

There is no `SideEffect` struct or enum in the codebase. Background-job
and scheduled-wakeup effects currently escape via shared `Arc<Mutex<>>`
slots (H-1 in the review; a separate P1 refactor).

**Action**: Remove the `SideEffect` bullet from the module doc. Do NOT
add a new type — that's a separate goal (H-1).

### 4. Verify

After all three cleanups:
- `cargo build` clean
- `cargo test --bin recursive` green
- `cargo test --lib runtime::` green
- `cargo test --lib runtime_goal::` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean

## Acceptance

- `parent_agent_last_uuid` field, builder init, and setter method are gone
- `GoalStatus::Cleared` variant is gone; serde roundtrip test for it is gone
- `runtime.rs:782` and `runtime.rs:848` no longer set `GoalStatus::Cleared`
- `kernel.rs` module doc no longer mentions `SideEffect`
- All quality gates pass
- No behavior change observable from any public API (except removing dead
  methods/fields — they were never called)

## Notes for the agent

- Read each file fully before editing. The runtime.rs is large; search
  for the symbol/line numbers in this goal rather than reading the whole
  file.
- For L-1: search the entire `src/` tree for `parent_agent_last_uuid` to
  find all usages, including in tests under `#[cfg(test)] mod tests`.
- For L-3: the serde test at `runtime_goal.rs:158` may be inside a
  `mod tests` block — read the surrounding context. The goal text says
  "remove this test or replace with another variant"; choose the smallest
  change. Probably "remove" is cleanest.
- For L-4: the module doc is at the top of `src/kernel.rs`. Just remove
  the `SideEffect` bullet (3 lines).
- The three changes are in 3 different files and can be made
  independently. Do them in any order.
- **DO NOT** implement `SideEffect` as a new type (that's H-1, a
  separate goal). **DO NOT** add a builder method for
  `parent_agent_last_uuid` "for future use." Dead code stays dead.
- **DO NOT** change the public API of `AgentRuntimeBuilder` beyond
  removing `parent_agent_last_uuid`. If the builder is `pub` and the
  method is `pub`, removing them is a semver-breaking change for
  downstream consumers; that's acceptable for a 0.x codebase but
  document it in the journal entry.

# Goal 275 — tool_audits keyed by (turn, tool_call_id)

**Roadmap**: Phase 17 (Production Hardening) — P0 from
`docs/review/architecture-review-2026-06-15.md` (NEW-TOOL-15, also
recurrence of g268 KERN-1 fix)

**Design principle check**:
- Implemented as: change the key of `tool_audits` from a single
  `tool_call_id: String` to a `(u32 /* turn */, String)` tuple,
  plumbed through `TurnOutcome` and `RunInnerOutcome`.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/runtime.rs:528` calls `tool_audits.remove(tcid)` as it walks
`new_messages` and matches each `Role::Tool` message to its audit
metadata. But `tool_audits` is a `HashMap<String /* tool_call_id */, AuditMeta>`
inside `TurnOutcome` — single global scope per turn.

The audit is *popped off* the map on the first match. Two failure
modes:

1. **Cross-turn ID collision**: if a later turn reuses a
   `tool_call_id` (LLM providers often do; MockProvider explicitly
   does in tests), the new audit overwrites the old one in the
   map before the old turn's emit has a chance to consume it.

2. **Same-turn duplicate IDs**: a buggy model that emits the same
   `tool_call_id` twice in one response will only attach audit to
   the first message.

The 06-10 review (NEW-KERN-1) called this out; the current
`HashMap` key didn't address the root cause (key collision), only
the ordering issue. `recursive resume` then sees a tool message
with no audit metadata and the orphan-detection logic
(`compaction_keeps_tool_calls_paired_with_results`) trips.

## Scope (do exactly this, no more)

### 1. Change the key type

In `src/tools/mod.rs` (around line 128, `AuditMeta` definition
area) and `src/run_core.rs` (line 67, `tool_audits` field on
`RunInnerOutcome`):

```rust
pub type AuditKey = (u32 /* turn */, String /* tool_call_id */);
pub(crate) tool_audits: HashMap<AuditKey, AuditMeta>,
```

`TurnOutcome::tool_audits` (kernel.rs:128) inherits the same
type change.

### 2. Plumb `turn` into the run

`RunCore` (run_core.rs:75) needs to know which turn it's
running. The simplest source: `RunInnerOutcome` doesn't currently
take a `turn` parameter, but `TurnContext` could — OR we can read
`AgentRuntime::checkpoint_turn_index` from the context. The
chosen approach:

- Add `turn: u32` to `TurnContext` (kernel.rs:57)
- In `runtime.rs::execute_kernel_turn` (line 487), pass
  `turn: self.checkpoints.turn_index as u32` into the `TurnContext`
- `RunCore` carries it as a field, prefixes every key:
  `(self.turn, tool_call_id.clone())`

### 3. Replace `.remove()` with `.remove(&key)` semantics

In `src/runtime.rs:525-540`, the emit loop currently does:

```rust
if let Some(tcid) = &msg.tool_call_id {
    if let Some(audit) = tool_audits.remove(tcid) {
        // attach audit
    }
}
```

After this change:

```rust
let key = (outcome_turn, tcid.clone());
if let Some(audit) = tool_audits.remove(&key) {
    // attach audit
}
```

`outcome_turn` is the `TurnOutcome`-level `turn: u32` (also added
to `TurnOutcome`).

### 4. Backward-compat for callers that already key by id

Search for all `.tool_audits` references. The mock test
(`src/llm/mock.rs`) and any in-flight e2e fixtures may key by
`tool_call_id` alone. Update each to use `(turn, id)`.

### 5. Tests

In `src/runtime.rs` `mod tests`:

```rust
#[tokio::test]
async fn audit_survives_collision_across_turns() {
    // Build a runtime with a mock LLM that emits the SAME
    // tool_call_id in turn 1 and turn 2. Run both turns. After
    // turn 1, capture the audit on the persisted tool message.
    // After turn 2, assert the turn-1 audit is still on disk
    // (re-load session) and turn-2's audit is also present.
}

#[tokio::test]
async fn duplicate_tool_call_id_in_same_response_attaches_both() {
    // Mock LLM emits two tool_calls with id "c1" in one
    // assistant message. Two tool_result messages both have
    // tool_call_id "c1". Both must have audit metadata.
}
```

## Acceptance

- `cargo test --workspace` — green (existing + 2 new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "tool_audits.remove" src/runtime.rs src/run_core.rs` —
  uses `&(turn, id)` keying, no bare `tcid` removal
- `grep "HashMap<String, AuditMeta>" src/` — 0 matches (type
  signature everywhere uses the tuple key)

## Notes for the agent

- The compile error count is your friend here: changing the
  `tool_audits` key type will surface every caller. Iterate
  through compile errors; do NOT pre-emptively touch files
  outside the Scope.
- The compaction test
  `compaction_keeps_tool_calls_paired_with_results` (mentioned in
  .dev/AGENTS.md invariant #8) is the regression boundary. It
  must still pass — verify explicitly.
- If `MockProvider` is hard to update for the
  `duplicate_tool_call_id` test, add a `MockProvider::with_
  duplicate_tool_call_id_response` builder method (small scope
  expansion allowed if needed).
- Estimated diff: 4-5 files (kernel.rs, run_core.rs, runtime.rs,
  tools/mod.rs, llm/mock.rs), ~50-100 lines net.
- **Test discipline reminder (from g268 post-mortem)**: prefer
  deterministic serde assertions over runtime dance.

**Disjoint file guarantee**: This goal touches src/kernel.rs,
src/run_core.rs, src/runtime.rs, src/tools/mod.rs,
src/llm/mock.rs. Goal 274 only touches src/http/handlers.rs.
Goal 276 only touches src/session.rs. No overlap — safe to
run in parallel.
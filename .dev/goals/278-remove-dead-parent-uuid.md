# Goal 278 — Remove dead `MessageAppended.parent_uuid`

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-KERN-15)

**Design principle check**:
- Implemented as: delete the `parent_uuid` field from
  `AgentEvent::MessageAppended` and its constructor sites; delete
  the corresponding `parent_uuid` on `TranscriptEntry` (if it is
  *only* set via this event path).
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`AgentEvent::MessageAppended` (src/event.rs:124-130) has a
`parent_uuid: Option<String>` field. Its doc-comment claims:

> Explicit parent UUID override for subagent branch points (g155).
> Used by subagent runtimes (g155).

But in `src/runtime.rs`, every single construction site
(line 414, 458, 536, 543, 557) passes `parent_uuid: None`. The
field is dead surface area — readers think there's a subagent
branch mechanism wired up, but there isn't.

`SessionWriter` (src/session.rs) does maintain its own internal
chain pointer for sequencing and writes it to disk as
`parent_uuid` in `TranscriptEntry`. That is independent of the
event field; the runtime never sets it.

The fix is mechanical: delete the field and the (all-None)
construction sites. If a future goal needs subagent branch
points, it can re-introduce the field with at least one call
site.

## Scope (do exactly this, no more)

### 1. Delete the field on `AgentEvent::MessageAppended`

In `src/event.rs:124-130`, remove `parent_uuid: Option<String>`
and its doc comment. Update the struct literal pattern matches
in `tests` (the round-trip test around line 463-496 of event.rs)
to drop the field.

### 2. Delete the field on `TranscriptEntry`

**First verify**: grep `parent_uuid` in `src/session.rs`. If the
`TranscriptEntry::parent_uuid` field is set by `SessionWriter`'s
internal chain (NOT by the event), then keep it — it's used for
correct on-disk linkage.

If the entry's `parent_uuid` is *only* ever populated by routing
through the event field (which it isn't, based on a quick
grep), also delete it.

Assume the entry field is independent — leave it. This goal
touches ONLY the event-level dead field.

### 3. Update construction sites in `src/runtime.rs`

The five sites at lines 414, 458, 536, 543, 557 each construct
`AgentEvent::MessageAppended { ... }`. Drop the
`parent_uuid: None,` line in each.

### 4. Tests

The round-trip serialization test in `src/event.rs:399-457`
exercises `MessageAppended`. After removing the field, this test
must still pass — `serde_json` will simply omit the field from
the JSON, which is forward-compatible.

Add a regression test (source-grep is acceptable per g268
discipline):

```rust
#[test]
fn message_appended_no_longer_has_parent_uuid_field() {
    // Source-grep check that src/event.rs doesn't define
    // parent_uuid inside AgentEvent::MessageAppended.
    let src = include_str!("event.rs");
    let arm = src
        .split("MessageAppended {")
        .nth(1)
        .expect("MessageAppended arm must exist")
        .split("}")
        .next()
        .expect("MessageAppended arm must close");
    assert!(
        !arm.contains("parent_uuid"),
        "MessageAppended arm must not reference parent_uuid: {arm}"
    );
}
```

## Acceptance

- `cargo test --workspace` — green (existing + new test)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "parent_uuid" src/event.rs` — 0 matches (field is
  deleted from `AgentEvent`)
- `grep "parent_uuid: None" src/runtime.rs` — 0 matches (the
  five construction sites are updated)
- The serialization round-trip test still passes (verified by
  `cargo test`)

## Notes for the agent

- The `TranscriptEntry::parent_uuid` field is used by the JSONL
  session format for message chain linkage. DO NOT remove that
  field — only the dead event field is in scope.
- If you find a downstream consumer (in `crates/agui-protocol`,
  `crates/agui-client`, or `crates/agui-tui`) that pattern-matches
  on `AgentEvent::MessageAppended` and uses `parent_uuid`, update
  that match to ignore the field (it never had a value anyway).
- A quick grep for `parent_uuid` across the whole `src/` tree
  before starting will tell you the full blast radius.
- Estimated diff: 2 files (event.rs, runtime.rs) + tests.
  ~20 lines net.
- **Test discipline reminder (from g268 post-mortem)**: the
  source-grep test is acceptable here because the invariant is
  "this field does not exist" — runtime tests would have to
  exercise the type system in a more convoluted way.

**Disjoint file guarantee**: This goal touches src/event.rs,
src/runtime.rs. Goals 274/275/276/277 don't touch either. Safe
to run in parallel.
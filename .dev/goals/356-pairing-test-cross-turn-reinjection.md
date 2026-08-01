# Goal 356 — Cover invariant #8 across cross-turn compaction reinjection

**Roadmap**: Test-coverage hardening — invariant #8 (tool-call ↔ tool-result pairing)

**Design principle check**:
- Implemented as: NEW tests (and only tests) extending `tests/invariants/tool_call_pairing.rs`.
- ❌ Does NOT modify any production code — `src/runtime.rs`, `src/compact/`, the kernel,
  tools, all untouched. If a test reveals a real pairing bug, STOP and report it (do not
  fix production in this goal; a separate goal fixes it). The deliverable is the guard
  test, pinned against current behaviour.
- No new deps, no new tools, no invariant changes.

## Why (the gap, with evidence)

Invariant #8 ("tool-call ↔ tool-result pairing must be preserved across
compaction / trimming / splicing") is guarded by `tests/invariants/tool_call_pairing.rs`,
which defines a correct `verify_tool_call_pairing(&[Message]) -> Result<(), String>`
helper (line 50) and runs it on hand-built transcripts, one isolated `Compactor`
call (`compaction_preserves_tool_call_pairing:163`), and a JSONL round-trip. **It never
drives a real `AgentRuntime` through the highest-risk mutation path.**

The highest-risk path is the runtime's cross-turn compaction **reinjection** at
`src/runtime.rs:461-534` (inside `maybe_compact_cross_turn`, `src/runtime.rs:370`).
After `Compactor::apply_to_transcript` drains the head and inserts a summary at index 0
(`src/runtime.rs:415-423`), the runtime runs THREE reinjector blocks that re-insert
`Role::System` attachment messages and compute their insertion indices by counting prior
attachments via **string-prefix heuristics**:

- Block A (file restore, `src/runtime.rs:461-479`): re-slices the preserved tail
  (`skip(1)` at `:467`) and inserts `[post-compact file restore: ...]` at `1 + offset`.
- Block B (skill restore, `src/runtime.rs:480-507`): computes `insert_base` by
  `take(r.max_files)` filtering on prefix `"[post-compact file restore:"` (`:494`). The
  comment at `:485-489` literally says **"Approximate: the number of file attachments
  inserted. We don't track the count directly."**
- Block C (plan/todo restore, `src/runtime.rs:508-534`): `insert_base` via `take_while`
  matching `"[post-compact file restore:"` OR `"[post-compact skill restore:"` (`:519-520`).

The three reinjectors emit only `Role::System` messages (`src/compact/reinject.rs:22-24,
179-181, 371-373`), so they cannot *directly* orphan a Tool. **The real pairing risk is
the preserved tail**: `apply_to_transcript` does `transcript.drain(..split)` +
`transcript.insert(0, summary)` (`src/compact/mod.rs:354-355`), and Block A then re-slices
that tail and re-orders it as `[summary, <file-atts>, <preserved-tail>]`. If `split` lands
inside a tool-call pair, or the re-slicing drops/duplicates a message, the tail's pairing
breaks — and NO test pins it today.

**Existing partial coverage (do not duplicate):**
- `compaction_preserves_tool_call_pairing` (`tool_call_pairing.rs:163`) — exercises
  `Compactor::apply_to_transcript` only; no runtime, no reinjectors.
- `cross_turn_compaction_reinjects_plan_and_todos` (`src/runtime.rs:3576`) — the closest
  analog: drives `maybe_compact_cross_turn` with plan/todo, but **no tool calls, no
  file/skill reinjectors, no pairing assertion.**
- `compact_partial_preserves_tool_call_pairing` (`src/runtime.rs:2071`) — intra-turn
  `compact_partial_before`, not cross-turn, no reinjectors.

**The intersection — cross-turn compaction + tool calls in the preserved tail +
reinjectors active + pairing assertion — has ZERO coverage.** This goal adds it.

## Scope (do exactly this, no more)

Add the following tests to `tests/invariants/tool_call_pairing.rs` (extending the existing
file that owns invariant #8 and already defines `verify_tool_call_pairing`,
`assistant_with_tool_call`, `tool_result_msg`). **Production code is not modified.**

### 1. `cross_turn_compaction_with_file_reinjector_preserves_pairing`

Build a runtime with the file reinjector installed AND a small compaction threshold so
`maybe_compact_cross_turn` fires, seed a transcript whose preserved tail contains a
matched tool-call pair, compact, and assert pairing holds.

Setup (mirror the idioms verified in the codebase):
```rust
let provider = Arc::new(MockProvider::new(vec![Completion {
    content: "summary".into(),
    tool_calls: vec![],
    finish_reason: Some("stop".into()),
    usage: None,
    reasoning_content: None,
}]));
// Pre-seed ReadFileState with one file (idiom: src/compact/reinject.rs:457-465 make_state_with)
let read_state = /* Arc<Mutex<ReadFileState>> seeded via locked.record(path, false, content, ts) */;
let mut rt = AgentRuntime::builder()
    .llm(provider)
    .compactor(Compactor::new(100).keep_recent_n(2))   // 100-char threshold ⇒ always fires
    .file_reinjector(FileReinjector::new(read_state))
    .build()
    .expect("build runtime");
// Seed a transcript long enough to clear the threshold, with a tool pair in the tail:
let msgs = vec![
    Message::user("padding ".repeat(20)),                               // bulk to trigger compaction
    assistant_with_tool_call("c1", "read_file", /*content*/),
    tool_result_msg("c1", /*content*/),
    Message::user("more padding ".repeat(20)),
    assistant_with_tool_call("c2", "read_file", /*content*/),
    tool_result_msg("c2", /*content*/),
];
rt.set_transcript(msgs);                                  // src/runtime.rs:819 public seam
rt.maybe_compact_cross_turn(&TokenUsage::default()).await.expect("compact ok");
```

Assert:
- `rt.transcript()[0]` is a compaction summary (compaction actually fired — guards against
  a silently-no-op test). Use the same `is_compaction_summary` / role check the existing
  `compact_boundary.rs` tests use (check how they identify the summary message; do not
  invent a predicate).
- `verify_tool_call_pairing(rt.transcript()).expect("pairing broken after file reinject")`
  — the invariant.
- At least one transcript message `content.starts_with("[post-compact file restore:")` is
  present AND sits between the summary (index 0) and the preserved tail (guards against the
  reinjector silently returning empty, which would make the pairing test vacuously pass).

### 2. `cross_turn_compaction_with_skill_reinjector_preserves_pairing`

Same shape, but install `.skill_reinjector(SkillReinjector::new(vec![skill_on_disk]))` where
`skill_on_disk` is a real skill written to a tempdir (idiom: `create_skill_on_disk` at
`src/compact/reinject.rs:674-701` — the reinjector reads the body from `skill.path` on
disk). Seed a transcript containing an assistant tool call whose `name == "Skill"` (or
`"load_skill"`) so the reinjector's scan of `pre_compact` matches. Assert:
- compaction fired,
- `verify_tool_call_pairing` ok,
- a `[post-compact skill restore:` message is present and lands AFTER any file
  attachments (if both reinjectors were installed — here skill-only, so just present).
This test exercises Block B's "Approximate" index math at `src/runtime.rs:484-496` most
directly.

### 3. `cross_turn_compaction_with_all_reinjectors_preserves_pairing`

Install file + skill reinjectors AND seed `rt.todo_list` + activate
`rt.plan_approval_gate.begin_approval(...)` (idiom: `src/runtime.rs:3592-3613`). Seed a
transcript with a tool pair in the tail. Compact. This drives all three blocks (A, B, C)
and BOTH independent `insert_base` computations (`src/runtime.rs:484` and `:514`). Assert:
- compaction fired,
- `verify_tool_call_pairing(rt.transcript()).expect("pairing broken after full reinject")`,
- the ordering of `[post-compact ...]` messages is `[file-att(s), skill-att(s), plan/todo-att(s)]`
  (i.e. file before skill before plan/todo) — pin the ordering the reinjector chain
  produces, so a future refactor that changes insertion math trips this test.

### Shared setup helpers

If the three tests would duplicate significant setup, extract a small private helper in the
same file (e.g. `fn rt_with_compaction_and(...) -> AgentRuntime`). Do NOT move
`verify_tool_call_pairing` / `assistant_with_tool_call` / `tool_result_msg` out of this
file — they are private to it by design; just reuse them.

### Imports to add to `tool_call_pairing.rs`

```rust
use recursive::{AgentRuntime, Compactor, TokenUsage};
use recursive::compact::{FileReinjector, SkillReinjector};
use recursive::llm::{Completion, MockProvider};
use recursive::tools::fs::ReadFileState;   // confirm exact path; it's re-exported from recursive::tools
use std::sync::{Arc, Mutex};
```
Verify each import resolves against the actual crate re-exports before committing (the
scout report indicates these are the right paths, but confirm `ReadFileState`'s re-export
path — it may be `recursive::tools::ReadFileState`).

## Files NOT to touch

- **Any production code**: `src/runtime.rs`, `src/compact/mod.rs`, `src/compact/reinject.rs`,
  `src/run_core.rs`, `src/kernel.rs`, tools, providers — ALL untouched. This is a
  test-only goal.
- `tests/compact_boundary.rs`, `tests/integration.rs`, `tests/incremental_writes.rs` —
  leave them; the new tests live in `tests/invariants/tool_call_pairing.rs`.
- The `verify_tool_call_pairing` helper itself — do not weaken or broaden it. If a test
  legitimately needs a weaker check (e.g. ignoring System messages), add a SEPARATE helper;
  do not mutate the existing one (other tests depend on its exact behaviour).
- `.dev/flows/`, `Cargo.toml`.

## Acceptance

- `cargo test --test invariants cross_turn_compaction` — the 3 new tests pass (and the
  existing 9 in that file still pass).
- `cargo test --workspace` green overall.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean (the new
  tests must not trip any lint — note `tests/invariants/` is itself gated by the workspace
  `#![deny]` via the `recursive` lib crate root; test code is exempted by `cfg_attr(test,
  allow)`, but double-check the test binary builds clean).
- `cargo fmt --all` clean.

## Notes for the agent (traps)

- **`set_transcript` vs `Arc::make_mut`.** For an integration test in `tests/`, use the
  public `rt.set_transcript(msgs)` seam (`src/runtime.rs:819`). The `*Arc::make_mut(&mut
  rt.transcript) = msgs;` pattern is in-crate only (`rt.transcript` is not public). Confirm
  `set_transcript` is the right public name (check `src/runtime.rs` around line 819 / the
  builder).
- **`keep_recent_n` and the split point.** Pick `keep_recent_n` so the compaction split
  lands such that the preserved tail still contains a COMPLETE tool pair (assistant+tool).
  If the split orphans a tool result, the test may fail for a real reason — that would be a
  genuine bug (report it, don't paper over by padding). Tune `keep_recent_n` and the
  transcript length so the pair is entirely in the preserved tail by construction.
- **The summary message identification.** Don't assume the summary is `Role::Assistant`;
  check how `src/compact/mod.rs:355` constructs it (`Message::assistant(...)` vs a custom
  role) and how `tests/compact_boundary.rs` identifies it. Use the same predicate.
- **`MockProvider` returns Completions in order.** Provide exactly ONE summary Completion
  for one compaction pass. If the test triggers two passes, supply two. One pass is enough
  for these tests.
- **If a test FAILS due to a real pairing bug**, do NOT fix production code in this goal.
  Instead: (a) leave the test in but mark what it revealed in the journal, (b) report the
  bug in the run summary so a follow-up goal can fix it. The deliverable is the guard test.
  (We expect current behaviour to be correct — the reinjectors emit System messages and
  `safe_split_point` already retreats past Tool — but be ready to be wrong.)
- **Don't seed `rt.todo_list` / plan gate for tests 1 and 2.** Block C (plan/todo) runs
  unconditionally (the `plan_todo_reinjector` is always wired by `build()`), but it emits
  nothing if there's no todo/plan state. Keep tests 1 and 2 focused on file / skill
  respectively; only test 3 activates everything.
- **`ReadFileState::record` signature.** Confirm the exact signature at
  `src/tools/fs.rs:59` (`record(&self, path: PathBuf, is_partial: bool, content: String,
  ts: ...)`) before calling. The scout report cites `make_state_with` at
  `src/compact/reinject.rs:457-465` as the idiom — copy that helper's approach.

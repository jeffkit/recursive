# Manual edit: goal-356 — Cover invariant #8 across cross-turn compaction reinjection

**Date**: 2026-08-02
**Goal**: Test-coverage hardening for invariant #8 (tool-call ↔ tool-result
pairing). Add guard tests (and only tests) that drive a real `AgentRuntime`
through the highest-risk mutation path: cross-turn compaction reinjection
(`maybe_compact_cross_turn`, `src/runtime.rs:370`), where the preserved tail is
re-sliced and `Role::System` attachments are re-inserted by three reinjector
blocks using string-prefix `insert_base` heuristics.

## Why

The existing `verify_tool_call_pairing` helper and its tests covered
hand-built transcripts, one isolated `Compactor::apply_to_transcript` call, and
a JSONL round-trip — but never the runtime's cross-turn reinjection path. The
intersection "cross-turn compaction + tool pair in the preserved tail +
reinjectors active + pairing assertion" had ZERO coverage.

## Files touched

- `tests/invariants/tool_call_pairing.rs` — extended only (+312/-4):
  - Imports: `recursive::compact::{FileReinjector, SkillReinjector}`,
    `recursive::llm::{Completion, MockProvider, ToolCall}`,
    `recursive::tools::ReadFileState`, `recursive::{AgentRuntime, Compactor,
    TokenUsage}`, `std::path::PathBuf`, `std::sync::{Arc, Mutex}`.
  - Removed two now-redundant function-local `use` statements inside the
    pre-existing `compaction_preserves_tool_call_pairing` (they duplicated the
    new top-level imports).
  - **No production code touched** — `git status` shows only the test file.

## Tests added

1. `cross_turn_compaction_with_file_reinjector_preserves_pairing` — builds a
   runtime with `.file_reinjector(FileReinjector::new(read_state))` and
   `Compactor::new(100).keep_recent_n(2)`, seeds `ReadFileState` via
   `record(PathBuf, false, content, 1000)` (the `make_state_with` idiom),
   seeds a 6-message transcript whose preserved tail holds a complete tool
   pair, calls `rt.maybe_compact_cross_turn(&TokenUsage::default())`, then
   asserts compaction fired (`transcript[0].is_compaction_summary`), a Tool
   result remains in the tail (non-vacuous), `verify_tool_call_pairing` holds,
   and the `[post-compact file restore:` attachment sits at index 1 between
   the summary and the preserved tail. Drives Block A (`src/runtime.rs:461`).

2. `cross_turn_compaction_with_skill_reinjector_preserves_pairing` — same
   shape with `.skill_reinjector(SkillReinjector::new(vec![skill]))` where
   `skill` is a real SKILL.md written to a tempdir and loaded via
   `discover_skills` (mirrors `create_skill_on_disk` at
   `src/compact/reinject.rs:674`). The seed transcript contains an Assistant
   `Skill` tool call (name arg) in the older portion so the reinjector's
   `pre_compact` scan matches. Asserts compaction fired, pairing holds, and
   the `[post-compact skill restore:` attachment is present at index 1.
   Drives Block B's "Approximate" `insert_base` math (`src/runtime.rs:484`).

3. `cross_turn_compaction_with_all_reinjectors_preserves_pairing` — installs
   file + skill reinjectors AND activates plan/todo state: plan via the public
   `rt.plan_approval_gate().begin_approval(...)` seam, todos via the real
   `TodoWrite` tool from the kernel registry
   (`rt.kernel().tools().get("TodoWrite")` — `todo_list` is private, so the
   registry tool is the public write seam and shares the same `Arc` the
   plan/todo reinjector reads). Asserts compaction fired, pairing holds, and
   the attachment chain is ordered `[file, skill, plan, todo]` — pinning both
   independent `insert_base` computations (`src/runtime.rs:484` and `:514`).

## Shared helpers added (same file)

- `summary_provider(content)` — one-scripted-completion `Arc<MockProvider>`.
- `transcript_with_tool_pair_in_tail()` / `transcript_with_skill_in_older_and_tool_pair_in_tail()`
  — seed transcripts; with `keep_recent_n(2)` over 6 messages,
  `safe_split_point` retreats to index 3 (User), leaving the full `c2` pair in
  the preserved tail by construction.
- `assert_compaction_fired_and_pairing_ok(rt, ctx)` — compaction-fired +
  non-vacuous + `verify_tool_call_pairing`.
- `skill_on_disk(name, body)` — tempdir SKILL.md + `discover_skills`.
- `seed_todos(rt)` — executes the registered `TodoWrite` tool.

## Verification of API seams (adjusted from goal sketch)

- `set_transcript` is the right public seam (`src/runtime.rs:819`); the
  `Arc::make_mut` pattern is in-crate only.
- `ReadFileState::record(&mut self, path: PathBuf, is_partial: bool, content:
  String, timestamp: u64)` at `src/tools/fs.rs:59` — confirmed exact signature.
- `ReadFileState` is re-exported at `recursive::tools::ReadFileState`.
- `rt.todo_list` / `rt.plan_approval_gate` are PRIVATE fields (the goal's
  sketch assumed the in-crate idiom `src/runtime.rs:3592-3613`). Integration
  test uses the public seams instead: `plan_approval_gate()` accessor +
  `TodoWrite` tool via `rt.kernel().tools().get("TodoWrite")`.
- Summary message predicate: `Role::System` + `is_compaction_summary` (same as
  the in-crate runtime tests; `compact_boundary.rs` identifies boundaries via
  the event sink, not the transcript).

## Result

Current behaviour is CORRECT — all 3 new tests pass, pinning the reinjection
path against invariant #8. No production bug revealed (expected: reinjectors
emit only `Role::System`, and `safe_split_point` retreats past Tool /
Assistant-with-tool_calls).

## Validation (all green)

- `cargo test --test invariants cross_turn_compaction` — 3 new tests pass.
- `cargo test --test invariants` — 38 passed (12 in tool_call_pairing: 9
  existing + 3 new; all other invariant modules pass).
- `cargo test --workspace` — OK (exit 0).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean.

## Notes

- One clippy finding during dev: `use recursive::tools::Tool` was flagged
  unused — `execute()` resolves on `dyn Tool` without the trait import; removed.
- `tempfile` is already a dev-dependency; no new deps, no production changes.

# Goal 342 — Add partial `/compact-before` / `/compact-after` TUI commands

**Roadmap**: Compaction upgrade (WS-4b — manual partial compaction)

**Design principle check**:
- Implemented as: two new `AgentRuntime` methods (`compact_partial_before`
  / `compact_partial_after`) + two TUI slash commands calling them.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT introduce a new `Error` variant (invariant #7).

## Why

Today `/compact` (`AgentRuntime::compact_now`, `runtime.rs:828`) summarizes
the entire older region and keeps the recent tail. fake-cc also offers
*partial* compaction: summarize only a segment — either everything *before*
a chosen pivot (`up_to`, keeping the recent suffix verbatim and its prompt
cache) or everything *after* a pivot (`from`, keeping the earlier prefix
verbatim and its cache). This is useful when a user knows a specific stretch
of the conversation is stale but wants to preserve either the recent or the
early context exactly.

This goal adds the two manual partial-compaction paths to Recursive. They
reuse `Compactor::safe_split_point` (pairing safety) and the post-compact
re-injectors (goals 334/335/340) so a partial compact also restores files /
skills / plan / todos. They are manual-only (TUI commands); the automatic
paths are unchanged.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — two partial-compaction methods

```rust
/// Summarize the messages before `pivot_turn`'s region, keep the suffix
/// verbatim. `pivot_index` is a transcript message index (not a turn
/// number) — the TUI command translates a turn number to an index.
pub async fn compact_partial_before(
    &mut self,
    pivot_index: usize,
) -> Result<()>

/// Summarize the messages after `pivot_index`, keep the prefix verbatim.
pub async fn compact_partial_after(
    &mut self,
    pivot_index: usize,
) -> Result<()>
```

Logic (mirror `compact_now`):
- `compact_partial_before`: let `split = safe_split_point(&self.transcript[..pivot_index+1], keep_recent_n)`
  clamped to `pivot_index+1`; summarize `transcript[..split]`; drain `..split`;
  insert summary at 0; run the reinjectors (file/skill/plan-todo) against the
  kept suffix; emit `CompactionBoundary` with `compacted_count = split`.
- `compact_partial_after`: summarize `transcript[pivot_index..]`; keep
  `transcript[..pivot_index]` verbatim; append the summary after the prefix;
  reinject against the kept prefix (dedup against prefix content). Emit
  `CompactionBoundary`.
- Both reuse `safe_split_point` so no tool-call pair is split (invariant #8).
- Both dispatch `PreCompact`/`PostCompact` hooks like `compact_now`.
- On transcript too short / nothing to summarize, return `Ok(())` (no-op),
  like `compact_now`'s short-transcript guard.

### 2. TUI commands (`crates/recursive-tui/src/commands.rs`)

Register two commands:
- `/compact-before <turn>` — translate the turn number to a transcript
  message index (find the first message of that turn — use the turn
  boundaries the transcript already tracks; confirm the mapping by reading
  the TUI transcript model), dispatch `UserAction::CompactPartial { direction: Before, pivot_index }`.
- `/compact-after <turn>` — same with `After`.
- Add `UserAction::CompactPartial { direction: PartialDirection, pivot_index }`
  to `crates/recursive-tui/src/events.rs`, and `PartialDirection { Before, After }`.
- The backend worker (`runtime_builder.rs` / `backend.rs`) handles the
  action by calling `runtime.compact_partial_before/after(pivot_index).await`
  and forwarding the resulting `CompactionBoundary` to the UI as it does for
  `compact_now`.
- Argument parsing: `<turn>` must parse as `usize`; on failure push an
  `Error` transcript block ("usage: /compact-before <turn>"). Missing arg →
  same error.

### 3. Tests (TUI — mandatory in same commit per CLAUDE.md)

In-process harness tests in `commands.rs` `#[cfg(test)]` (use
`crate::harness::Harness`):
- `compact_before_command_dispatches_partial_action` — type
  `/compact-before 2`, assert the dispatched `UserAction::CompactPartial {
  direction: Before, pivot_index: <mapped> }`.
- `compact_after_command_dispatches_partial_action` — same for `After`.
- `compact_before_invalid_arg_shows_error` — `/compact-before abc` → an
  Error transcript block is pushed, no action dispatched.
- `compact_before_missing_arg_shows_error` — bare `/compact-before` → usage
  error.

Runtime unit tests in `src/runtime.rs`:
- `compact_partial_before_summarizes_prefix_keeps_suffix` — seed a
  transcript, call `compact_partial_before(pivot)`, assert the suffix after
  pivot is byte-identical and the prefix is replaced by a summary.
- `compact_partial_after_summarizes_suffix_keeps_prefix` — symmetric.
- `compact_partial_preserves_tool_call_pairing` — a tool-call pair straddling
  the pivot is not split (assert via `safe_split_point` retreat; no orphan
  `Role::Tool`).
- `compact_partial_too_short_is_noop`.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- `.dev/scripts/tui-test-presence.sh` PASS (TUI src changed with tests added).
- Run `.dev/scripts/tui-mutants.sh` (advisory for manual edits per
  `CLAUDE.md`); fix survivors inside the diff hunks.
- `/compact-before <turn>` and `/compact-after <turn>` dispatch correctly;
  invalid/missing args show a usage error.
- `tests/invariants/tool_call_pairing.rs` green.

## Notes for the agent

- **Turn → message-index mapping:** confirm how the TUI maps a turn number
  to a transcript index (read the transcript model in
  `crates/recursive-tui/src/ui/transcript.rs` / `app.rs`). If turns are not
  explicitly tracked, accept a message index instead of a turn number and
  name the commands `/compact-before-index <i>` / `/compact-after-index <i>`
  to avoid a misleading "turn" argument. Document the choice in the journal.
- **Reuse, don't duplicate:** both partial methods share the summarize +
  reinject + emit logic with `compact_now` — extract a private helper if it
  reduces duplication, but do not refactor `compact_now`'s public signature.
- **Manual-only:** do NOT wire partial compaction into the automatic
  intra-turn or cross-turn paths. The automatic layer is microcompact +
  LLM-summary (goals 332/333); partial is a user tool.
- This goal depends on goals 334/335/340 (reinjectors) for the post-partial
  re-injection; if those are not yet landed, scope this goal to the
  summarize+splice without reinject and add reinject in a follow-up. Note
  the dependency in the journal.
- **DO NOT modify** `src/run_core.rs`, `src/llm/`, `src/kernel.rs`, the
  automatic compaction paths, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-partial-command.md`.

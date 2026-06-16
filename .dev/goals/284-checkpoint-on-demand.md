# Goal 284 — Checkpoint On-Demand: Remove Automatic Per-Turn Snapshots

> **Roadmap**: Phase 14.4 — Persistence & State (follow-up to g141, g144)
> **Motivation**: The automatic pre/post snapshot at every turn has proven
> too costly in practice — shadow-git repos routinely grow to 5–20 GB
> because every turn triggers two full-workspace `git add -A` + `commit-tree`
> calls. The core value of shadow-git is `recursive sessions rewind`; the
> automatic snapshot model is not the cheapest way to deliver it.

## Why

Shadow-git was designed with automatic per-turn snapshots (pre + post) so
that `recursive sessions rewind` could restore the workspace to any turn
boundary without user intervention. In practice:

1. **The per-turn overhead is large and constant.** Every turn, even one
   that only calls `read_file`, triggers two `git add -A --force` + two
   `git commit-tree` calls over the entire workspace. On a typical Rust
   project this means staging thousands of files per snapshot.

2. **The benefit is low-frequency.** Rewind is an escape hatch used perhaps
   once per session, if at all. Paying the full snapshot cost on every turn
   to enable a rarely-used feature is a bad trade.

3. **`touched_files` is already recorded.** `checkpoints.jsonl` tracks
   exactly which files each turn modified. The only thing shadow-git adds
   is the actual file *content* at each boundary. We can record content
   lazily — only when the user or agent explicitly asks for a checkpoint.

4. **The main git repo is already there.** For most workspaces the user
   has git. An explicit checkpoint can just be a lightweight operation on
   the files that were touched, not a full-tree scan.

## Scope

### What changes

1. **Remove automatic `snapshot_pre_turn` and `snapshot_post_turn`** from
   `AgentRuntime` (`src/runtime.rs`). The `CheckpointState` machinery and
   `enable_checkpoints` wiring stay in place but are no longer called at
   turn boundaries.

2. **Add a new `checkpoint_save` agent tool** (`src/tools/checkpoint.rs`)
   that the agent can call explicitly when it wants to record a restore
   point. It takes an optional `message` string. Under the hood it calls
   `ShadowRepo::snapshot_for_session` exactly once (no pre/post split).
   This is the only path that creates new shadow-git objects during a run.

3. **Keep `checkpoint_list` and `checkpoint_diff`** read-only tools as-is.
   They remain available to the agent.

4. **Keep `recursive sessions rewind`** CLI as-is. It still reads
   `checkpoints.jsonl` and restores `touched_files` from the shadow repo.
   The only difference: checkpoints now exist only where the agent (or
   user via CLI) explicitly created one, not at every turn boundary.

5. **Keep `touched_files` tracking in `checkpoints.jsonl`** for the
   explicit checkpoints the agent saves. The schema does not change.

6. **Remove the `pre` checkpoint id from `checkpoints.jsonl` entries.**
   With on-demand snapshots there is no longer a guaranteed pre-turn
   checkpoint. The log entry now records only:
   - `turn` (which turn the agent was in when it saved)
   - `id` (the checkpoint id)
   - `message` (the agent-supplied label)
   - `touched_files` (files the agent touched up to this point in the turn)
   - `saved_at` timestamp

   **Backwards compatibility**: the old `pre`/`post` fields are simply
   ignored on read if present; existing sessions with auto-snapshots can
   still be rewound using the last recorded checkpoint.

### What does NOT change

- `ShadowRepo` itself: `open`, `snapshot_for_session`, `restore_paths`,
  `read_file_at`, `list_for_session`, `changed_paths`, `gc`, `clean` — all
  stay. Only the automatic call sites in `runtime.rs` are removed.
- `recursive sessions rewind` CLI behavior and flags.
- The `checkpoint_list` / `checkpoint_diff` agent tools.
- The pathspec exclusions added in the manual fix (target/, node_modules/…).
- The `ShadowRepo::gc()` method and `recursive sessions gc-checkpoints` /
  `recursive sessions clean-checkpoints` CLI commands.

## New tool spec: `checkpoint_save`

```
Tool name: checkpoint_save
Description: Save an explicit restore point for the current session.
  Call this before making a risky batch of changes, or after completing
  a logical unit of work you might want to revert to. Unlike automatic
  checkpoints (which no longer exist), this runs only when you call it.
Parameters:
  message (string, optional): A short label for this checkpoint,
    e.g. "before refactor" or "after adding tests". Defaults to the
    current turn number if omitted.
Returns: The checkpoint id (12-char SHA) on success.
```

The tool is registered in `ToolRegistry` only when checkpoints are
enabled (same gate as the existing checkpoint tools).

## Migration note for existing sessions

Sessions created before this change have `pre`/`post` checkpoint ids in
`checkpoints.jsonl`. The rewind planner (`plan_rewind`) should be updated
to treat the last `pre` **or** `id` field of the most recent log entry
before `to_turn` as the target checkpoint. This way old sessions (with
auto-snapshots) and new sessions (with explicit checkpoints) both work
with `recursive sessions rewind`.

## Testing

- Unit test in `src/tools/checkpoint.rs`: `checkpoint_save_tool_creates_entry`
  — verifies that calling the tool creates a `checkpoints.jsonl` entry with
  the correct turn, message, and that the id resolves in the shadow repo.
- Existing tests `rewind_undoes_turn_and_restores_files_and_transcript` and
  `rewind_does_not_touch_other_workspace_files` must still pass (they call
  `snapshot_for_session` directly, which is unchanged).
- Verify that a `runtime_snapshots_at_turn_boundaries`-style test is removed
  or updated to reflect the new on-demand model.

## Acceptance criteria

- `cargo test --workspace` green (excluding pre-existing failures in
  `tests/http.rs` caused by an unrelated `AgentEvent` struct change).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- No `snapshot_pre_turn` / `snapshot_post_turn` calls remain in `runtime.rs`.
- `checkpoint_save` appears in the default tool registry alongside
  `checkpoint_list` and `checkpoint_diff`.
- Running a session and calling `checkpoint_save` produces a resolvable
  entry in the shadow repo (verified by unit test).

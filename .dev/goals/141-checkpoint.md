# Goal 141 — Per-Turn, Per-Session Checkpoint with Shadow Git Repository

> **Roadmap**: Phase 14.4 — Persistence & State
> **Replaces**: prior draft of g141 that exposed `checkpoint_save` /
> `checkpoint_restore` as agent-callable tools.
> **Design principle check**:
> - Checkpoint is **runtime infrastructure**, not an agent tool.
>   Snapshots happen automatically at turn boundaries; agents do not
>   decide when to save.
> - Orthogonal: lives in `src/checkpoint.rs` + small hooks in
>   `AgentRuntime::run()` and tool dispatch. Zero new dependencies
>   (uses `git` via `std::process::Command`).
> - Concurrency-aware: multiple sessions in the same workspace share
>   one shadow object store but maintain independent checkpoint chains
>   under `refs/sessions/<sid>/HEAD`.

## Why

The agent's `apply_patch` / `write_file` calls produce real filesystem
side effects. If an agent's turn produces a regression and the user
wants to rewind, replaying transcripts is not enough — the files need
to revert too. This goal adds **per-turn workspace snapshots** so that
`recursive sessions rewind <sid> --to-turn N` can restore the
workspace to its state at the start of turn N, identical to Claude
Code's `/rewind`.

## Scope decisions (informed by the conversation that produced this goal)

1. **Granularity is per-turn, not per-tool-call.** A turn is one
   `AgentRuntime::run(goal)` invocation. Finer-grained
   per-tool-call snapshots are a future optimization (out of scope here).

2. **Checkpoints are not agent-visible side effects.** Removed:
   `checkpoint_save`, `checkpoint_restore` tools. Kept: read-only
   tools `checkpoint_list`, `checkpoint_diff` so agents can self-check.

3. **Multiple sessions in one workspace are explicitly supported,
   *without* requiring `git worktree`.** The shadow repo's object
   store is shared (giving free dedup); each session has its own ref
   chain. Restore is **selective** — it only touches files this
   session previously modified, leaving sibling sessions' files
   untouched.

4. **Conflicts are surfaced, not silently overwritten.** When session
   A rewinds a file that session B has since modified, the rewind
   refuses unless `--force` is given.

5. **`run_shell` side effects are best-effort tracked.** When the
   tool is structured (`write_file`, `apply_patch`), we know the
   exact paths. For `run_shell` we fall back to a workspace-wide
   pre/post diff and attribute every changed file to that turn's
   "touched" set.

## Architecture

### Layout

```
<workspace>/
├── .recursive/
│   ├── shadow-git/                      # bare git repo, shared by all sessions
│   │   ├── objects/                     # content-addressed → automatic dedup
│   │   └── refs/sessions/<sid>/HEAD     # each session's checkpoint chain
│   └── sessions/<wsslug>/<sid>/
│       ├── transcript.jsonl
│       ├── checkpoints.jsonl            # turn → checkpoint metadata
│       └── .meta.json
└── ... (user files)
```

### `checkpoints.jsonl` line schema

```json
{
  "turn": 3,
  "pre":  "a1b2c3d4e5f6",         // checkpoint id at turn start
  "post": "f6e5d4c3b2a1",         // checkpoint id at turn end
  "touched_files": ["src/foo.rs", "README.md"],
  "touched_via": "structured",     // "structured" | "shell-diff"
  "started_at": 1717000000,
  "finished_at": 1717000042
}
```

### `ShadowRepo` API (revised)

```rust
pub struct ShadowRepo {
    workspace: PathBuf,
    shadow_dir: PathBuf,
}

impl ShadowRepo {
    /// Init or open the bare repo. Idempotent.
    pub fn open(workspace: impl Into<PathBuf>) -> Result<Self>;

    /// Snapshot the current workspace state and advance the given
    /// session's HEAD ref. The new commit's parent is the previous
    /// HEAD of *that session*, not any other session.
    pub fn snapshot_for_session(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<CheckpointId>;

    /// List checkpoints for one session in reverse chronological order.
    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<CheckpointInfo>>;

    /// Read a single file's contents at a given checkpoint.
    /// Used for both restore and conflict detection.
    pub fn read_file_at(
        &self,
        checkpoint: &CheckpointId,
        path: &str,
    ) -> Result<Option<Vec<u8>>>;   // None if file did not exist at that checkpoint

    /// Restore *only* the given file paths to their state at `checkpoint`.
    /// Files that did not exist at the checkpoint are deleted.
    /// Files in the workspace not in `paths` are untouched.
    pub fn restore_paths(
        &self,
        checkpoint: &CheckpointId,
        paths: &[String],
    ) -> Result<RestoreStats>;

    /// Diff between two checkpoints, optionally limited to file paths.
    pub fn diff(
        &self,
        a: &CheckpointId,
        b: Option<&CheckpointId>,
        paths: &[String],
    ) -> Result<String>;

    /// Garbage-collect: drop refs for sessions that no longer exist
    /// on disk under .recursive/sessions/. Safe no-op for now.
    pub fn gc(&self) -> Result<()>;
}
```

Internals:

- `git update-ref refs/sessions/<sid>/HEAD <new>` is used to update
  refs atomically (POSIX rename inside the repo). No more raw
  `fs::write` to ref files.
- The temp index file is per-session: `tmp-index-<sid>` so concurrent
  snapshots don't collide.
- `restore_paths` builds a temp index from the checkpoint's tree,
  then runs `git checkout-index -- <path1> <path2> …` so unrelated
  files in the workspace are not touched.
- For deletion (file existed at checkpoint = no, in workspace = yes,
  in `paths` set), we explicitly `fs::remove_file`.

### Runtime integration

`AgentRuntime` gains:

```rust
pub struct AgentRuntime {
    // existing fields…
    shadow: Option<Arc<ShadowRepo>>,
    session_id: Option<String>,
    turn_index: usize,
    checkpoints_writer: Option<CheckpointsWriter>,
}
```

`run(goal)`:

1. If `shadow` and `session_id` both set:
   `pre = shadow.snapshot_for_session(sid, "turn N pre: <goal>")`
2. Build a `TouchedFiles` collector and inject it into the
   `ToolRegistry` for this turn (see below).
3. Execute the kernel turn as today.
4. After kernel returns:
   `post = shadow.snapshot_for_session(sid, "turn N post: <goal>")`
   compute `touched = collector.into_set()`. If kernel called
   `run_shell`, also union `diff(pre, post)` paths.
5. Append a record to `checkpoints.jsonl`.
6. Set `outcome.checkpoint_id = Some(post)`.

### TouchedFiles tracker

A small struct held in an `Arc<Mutex<...>>` and registered once per
turn:

```rust
#[derive(Default)]
pub struct TouchedFiles {
    paths: HashSet<String>,
    saw_shell: bool,
}
```

Hooked into `ToolRegistry::invoke` (or a dedicated wrapper layer):
when the tool name is `write_file` / `apply_patch`, parse the args
and add each path. When the tool name is `run_shell`, set
`saw_shell = true` so the runtime knows to fall back to diff
attribution.

### CLI: `recursive sessions rewind`

```
recursive sessions rewind <session-id> --to-turn N [--force]
```

1. Load `checkpoints.jsonl`. Find the entry for turn N. Take its
   `pre` checkpoint id and the `touched_files` of all turns >= N
   (because rewinding to start of N means undoing N, N+1, …).
2. For each `path` in the union touched set:
   - Read the file's current bytes from the workspace.
   - Read the file's bytes at `pre` checkpoint.
   - Read the file's bytes at the post-snapshot of the **most recent
     turn this session knows about** (= the last turn before rewind).
   - If current ≠ last-known-post → file was modified externally
     (likely by a sibling session). Without `--force`, abort and
     print a list of conflicting files. With `--force`, proceed.
3. Call `shadow.restore_paths(&pre, &touched_paths)`.
4. Truncate `checkpoints.jsonl` to entries with `turn < N`.
5. Truncate `transcript.jsonl` to messages associated with
   `turn < N` (transcript line metadata already tracks message ↔
   turn association via `SessionWriter`).

### What we do *not* do

- We do not track files modified by anything other than the agent's
  tools (e.g. user editor saves). Same limitation as Claude Code.
- We do not snapshot `.recursive/` itself.
- We do not maintain checkpoints across `clean` invocations.
- We do not deduplicate refs across sessions on disk; only objects.

## Tests

Core (`src/checkpoint.rs`):

- `shadow_repo_init_creates_dir` — open creates `.recursive/shadow-git/`.
- `snapshot_per_session_independent` — open two sessions A, B;
  snapshot in each; lists are independent (one entry each).
- `snapshot_dedups_objects` — snapshot the same file content from
  two sessions; only one blob in the object store.
- `restore_paths_only_touches_specified_files` — workspace has
  files X (in restore set) and Y (not in set); modify both,
  restore X-only → X reverts, Y unchanged.
- `restore_paths_handles_deletion` — file exists at workspace but
  not at checkpoint, included in paths → file is deleted.
- `read_file_at_returns_none_for_missing` — file not in checkpoint
  tree → None.
- `concurrent_snapshots_use_distinct_temp_indexes` — start two
  snapshots concurrently in two sessions, both succeed.

Runtime (`src/runtime.rs` or `tests/checkpoint_runtime.rs`):

- `runtime_snapshots_at_turn_boundaries` — mock provider, run
  twice → 2 entries in checkpoints.jsonl with monotonic turn ids.
- `runtime_records_touched_files_for_write_file` — mock provider
  emits a `write_file` tool call → checkpoints.jsonl entry has
  that path.
- `runtime_records_touched_files_for_apply_patch` — same but for
  apply_patch (multi-path).
- `runtime_falls_back_to_diff_for_run_shell` — mock provider
  emits a `run_shell` that touches a file → that path appears
  in touched_files via "shell-diff".
- `runtime_works_when_shadow_unavailable` — git not on PATH →
  runtime runs without checkpoints, no panic.

CLI / E2E (`tests/checkpoint_rewind.rs`):

- `rewind_undoes_single_turn` — write file in turn 1; modify in
  turn 2; rewind to turn 2 → file content matches turn-1 state.
- `rewind_to_turn_n_undoes_n_and_later` — three turns, rewind to
  turn 2 → effects of turn 2 and turn 3 both gone.
- `rewind_does_not_touch_sibling_session_files` — session A and
  session B both modify different files; A rewinds → B's file
  intact.
- `rewind_detects_conflict_when_sibling_modified_same_file` —
  A and B both modify the same file; A rewinds without --force
  → returns error listing conflict.
- `rewind_force_overrides_conflict` — same setup, with --force
  → restore proceeds.
- `rewind_truncates_transcript_and_checkpoints_jsonl` — after
  rewind, `transcript.jsonl` and `checkpoints.jsonl` only contain
  entries for surviving turns.

## Acceptance

- `cargo build` green.
- `cargo test` green; ≥ 18 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- A demo: in a fresh tempdir, run `recursive run "echo hi"` twice,
  then `recursive sessions rewind <sid> --to-turn 1` succeeds and
  `recursive sessions show <sid>` reflects only turn 0.

## Out of scope (defer to future goals)

- Per-tool-call (sub-turn) snapshots.
- Snapshot pruning / GC by age or count.
- Cross-session merge / conflict resolution beyond "abort or force".
- Capturing user editor changes between turns (would require a
  filesystem watcher).
- Encryption / signing of snapshot objects.
- Worktree isolation as a "promotion" operation when the user wants
  full isolation.

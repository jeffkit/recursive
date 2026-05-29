# Goal 142 — Relocate workspace-private state to the user data dir

> **Roadmap**: Phase 14 — Persistence & State (cleanup follow-up to g141)
> **Design principle check**: Orthogonal — pure path-resolution change.
> No tool/runtime API surface changes. Backwards compatibility via a
> one-time `recursive migrate` command + a startup warning when old
> paths still exist.

## Why

Today the agent writes several "agent run byproducts" into the
project directory itself:

```
<workspace>/.recursive/
├── sessions/            ← per-session transcripts, meta, costs, checkpoints
├── shadow-git/          ← bare repo for per-turn workspace snapshots
└── scratchpad.json      ← agent's working-memory store
```

Recursive's own repo gitignores `.recursive/*` (with a
`!skills/` exception), but **users' projects don't** — and they
shouldn't have to. The current layout means:

- `git status` shows `??.recursive/`
- `cp -R project/ backup/` carries every snapshot blob and JSONL log
- Multiple sessions writing to the same workspace litter the project
  tree
- The shadow-git directory can grow indefinitely

The fix is to move every "agent byproduct" out of the project
directory entirely. Project-bundled assets (`skills/`, `mcp.json`)
stay where they are because they need to ship with the project.

## Scope

### Migrate (move out of `<workspace>/.recursive/`)

| Artifact | New location |
|---|---|
| `sessions/` | `~/.recursive/workspaces/<ws-hash>/sessions/` |
| `shadow-git/` | `~/.recursive/workspaces/<ws-hash>/shadow-git/` |
| `scratchpad.json` | `~/.recursive/workspaces/<ws-hash>/scratchpad.json` |

`<ws-hash>` is the first 12 hex chars of
`blake3(canonical_workspace_path)`. The directory also contains a
`path.txt` recording the original absolute path so a human can map
hash back to project later.

### Stay (project-bundled assets)

- `<workspace>/.recursive/skills/` — project-shipped skill library
- `<workspace>/.recursive/mcp.json` — project-shipped MCP server list

### Already in user dir (no change)

- `~/.recursive/config.toml`
- `~/.recursive/memory.json` (with workspace fallback)
- `~/.recursive/memory/facts.jsonl` (with workspace fallback)

## Architecture

### `src/paths.rs` (new)

```rust
/// Per-user data root: `~/.recursive` (or `RECURSIVE_HOME` if set).
pub fn user_data_dir() -> PathBuf;

/// Per-workspace user-side scratch dir:
/// `<user_data_dir>/workspaces/<ws-hash>/`.
/// Created on first call; writes `path.txt` once.
pub fn user_workspace_dir(workspace: &Path) -> Result<PathBuf>;

/// Convenience: `<user_workspace_dir>/sessions/`.
pub fn user_sessions_dir(workspace: &Path) -> Result<PathBuf>;

/// Convenience: `<user_workspace_dir>/shadow-git/`.
pub fn user_shadow_git_dir(workspace: &Path) -> Result<PathBuf>;

/// Convenience: `<user_workspace_dir>/scratchpad.json`.
pub fn user_scratchpad_path(workspace: &Path) -> Result<PathBuf>;

/// Workspace hash used in path components. Public for diagnostics
/// and for `recursive migrate`'s log output.
pub fn workspace_hash(workspace: &Path) -> String;

/// Detect legacy in-tree paths so callers can warn the user.
pub fn legacy_paths_in_workspace(workspace: &Path) -> Vec<PathBuf>;
```

The `RECURSIVE_HOME` env override is preserved so tests can sandbox
without touching the user's actual `~/.recursive`.

### Touch sites

- `ShadowRepo::open` — replace `workspace.join(".recursive").join("shadow-git")`
- `SessionWriter::create` — replace `workspace.join(".recursive").join("sessions")…`
- `SessionReader::list_sessions` / `list_all_sessions` — read from new location
- `scratchpad_path` (`tools/memory.rs`) — new helper
- `rewind::checkpoint_log_path` — derive via new helper
- `main.rs` `resolve_session_path` — search new location, with
  legacy fallback so already-migrated users still find their old
  data while transitioning

### `recursive migrate` subcommand

```
recursive migrate                       # migrate current workspace
recursive migrate --workspace <path>    # migrate a specific workspace
recursive migrate --dry-run             # preview only
```

Behavior:
1. Resolve `<workspace>` (default: cwd).
2. List legacy in-tree paths via `legacy_paths_in_workspace`.
3. For each present legacy path, `mv` it to its new home under
   `~/.recursive/workspaces/<hash>/`. If the destination already
   exists, abort with a helpful message (don't merge silently).
4. After all moves succeed, optionally remove the now-empty
   `<workspace>/.recursive/` directory only if it has nothing left
   (skills/ and mcp.json stop the cleanup).
5. Print a summary.

### Startup warning

In `run_once` / `run_loop` / `run_resume` / `repl`, before doing
anything heavy, call `legacy_paths_in_workspace(&config.workspace)`.
If non-empty, log:

```
warning: legacy in-tree state detected at <workspace>/.recursive/
         (sessions/ shadow-git/ scratchpad.json)
hint:    run `recursive migrate` to move it under ~/.recursive
```

This is a warning, not a hard error — the run still proceeds, but
new data goes to the new location only.

## Tests

Unit (`src/paths.rs`):

- `user_data_dir_honors_env_override` — `RECURSIVE_HOME=/tmp/x` →
  `user_data_dir()` returns `/tmp/x`.
- `workspace_hash_is_stable_across_calls` — same input, same hash.
- `workspace_hash_differs_for_different_paths` — `/a` and `/b`
  differ.
- `user_workspace_dir_writes_path_txt` — first call creates the
  directory and writes the original path.
- `legacy_paths_detects_in_tree_state` — temp workspace with a fake
  sessions dir + shadow-git → returned vec has both.
- `legacy_paths_returns_empty_when_clean` — fresh workspace returns
  empty.

Integration (`tests/storage_relocation.rs`):

- `shadow_repo_uses_user_data_dir` — set `RECURSIVE_HOME` to a temp,
  open a ShadowRepo for some workspace, assert the bare repo lands
  under `<RECURSIVE_HOME>/workspaces/<hash>/shadow-git/`.
- `session_writer_uses_user_data_dir` — same for sessions.
- `migrate_moves_sessions_and_shadow_git` — pre-populate a workspace
  with legacy paths, run migrate (in-process API), assert files
  appear at new location and source is gone.
- `migrate_skips_skills_and_mcp_json` — legacy + skills/ + mcp.json
  present → migrate moves only the byproducts, leaves skills and
  mcp untouched.
- `migrate_aborts_on_destination_collision` — destination already
  has a `sessions/` → migrate returns error and source is untouched.

E2E:

- Update `tests/checkpoint_e2e.rs` to set `RECURSIVE_HOME` to a temp
  so the test doesn't pollute the user's real `~/.recursive`.

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 8 new tests
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Running `recursive run "echo hi"` in a fresh workspace creates
  exactly nothing under `<workspace>/.recursive/` (skills/ already
  exists is the only acceptable resident).
- `ls ~/.recursive/workspaces/` shows the new dir with `path.txt`.

## Out of scope (defer)

- Garbage-collecting old workspace dirs that no longer exist on disk.
- Encrypting per-workspace data at rest.
- Sharing checkpoints across workspaces (intentionally siloed by
  hash).
- Importing existing in-tree state from a *different* workspace's
  user dir (manual `mv` is fine).

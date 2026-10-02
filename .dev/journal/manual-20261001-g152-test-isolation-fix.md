# 20261001 — g152 incremental_writes + resume_by_id hermeticity & session-id collision fix

Date: 2026-10-01
Goal: fix failing `cargo test --workspace` (incremental_writes 3–4 failures under pipeline env)

## Root cause
`RECURSIVE_SESSIONS_DIR` (Goal-H J1) is a **hard override** in `paths::user_sessions_dir`
that beats `RECURSIVE_HOME`. Test suites pin `RECURSIVE_HOME` via `HomePin`/`IsolatedWorkspace`,
but the self-improve pipeline env carries `RECURSIVE_SESSIONS_DIR` → every test writer landed
in ONE shared root. Combined with `SessionWriter::create` deriving session_id purely from
`<1s-timestamp>-<slug>`, all suites running within the same second merged into one
transcript.jsonl → cross-test contamination (wrong contents / inflated counts).

## Changes
- `src/session/writer.rs`: session ids now get a random 8-hex suffix
  (`<ts>-<slug>-<uuid8>`). Two same-second same-workspace creates no longer collide
  onto one directory (real product bug: silent transcript merge).
- `src/test_util.rs`: `IsolatedWorkspace` also pins `RECURSIVE_SESSIONS_DIR` to
  `<home>/sessions` (same env_lock acquisition — the lock is non-reentrant).
- `src/config.rs`: the `from_env_injects_memory_and_scratchpad_layers` test additionally
  pins/restores `RECURSIVE_SESSIONS_DIR` (it uses the NoLock pin variant).
- `tests/resume_by_id.rs`: `HomeOverride` now pins both env vars via one `SessionEnvGuard`.

## Tests
`cargo test --workspace` 57/57 binaries green (×2 consecutive runs);
`cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo fmt --all` applied.

## Notes
- `src/tools/shell.rs::timeout_kills_child_process` is a known load flake (passes standalone,
  2 of 3 full runs) — left untouched; already carries the repo's poll-for-marker mitigation.
- `reader.rs::list_sessions` was intentionally NOT scoped by slug under the override:
  `cli_command_surfaces::agents_lists_active_session` and the e2e override contract expect
  the flat-root listing.

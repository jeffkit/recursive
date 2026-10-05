# Manual landing of issue #135 — multi-tenant workspace registry (DSH `workspace` borrow)

- goal source: issue #135 (scene gap report, P2, jeffkit follow-up on DSH `packages/workspace/`)
- mode:        orchestrator-direct
- branch:      v2-pipeline-135-1006014833
- verdict:     completed

## Result

Recursive sessions were flat: no grouping above a session, no archive
lifecycle, no "what is still running here" gate before a project is put
away. This adds the organisational base layer: a per-user **workspace
(project) registry** keyed by the realpath of the directory, carrying a
header-only session index and an archive lifecycle. It complements #85
(tenant identity in the HTTP layer) — that answers *who* is calling, this
answers *what* they are organising.

## What landed

### New module `src/workspace/`

- `registry.rs` — `WorkspaceRegistry`, `WorkspaceRecord`, `SessionHeader`,
  `ArchivePolicy` / `ArchiveOutcome`, `RecoveryReport`.
- `activity.rs` — the archive-admission capability seam: `ActivityProbe`
  trait, `ActiveWork` / `ActiveWorkKind`, `NoActivityProbe`,
  `StoreActivityProbe`.
- `mod.rs` — re-exports + module docs + a compiling doctest.

Four DSH-derivable properties, each with an acceptance test:

1. **realpath canon.** `create()` keys on `std::fs::canonicalize` (resolves
   symlinks, like `fs.realpath`). A directory that resolves to one already
   registered is refused with `Error::WorkspaceConflict`, whatever the
   spelling — so two entries can never alias one directory.
   Test: `symlink_to_registered_directory_is_rejected` (and
   `exact_directory_reregistration_is_rejected`).
2. **Two-write mutations + crash recovery.** `create` / `remove` /
   `set_archived` / `archive` each write a `pending/<hash>.json` marker
   *before* rewriting `index.json`, then clear it. `WorkspaceRegistry::open`
   runs `recover()` (finishes interrupted ops idempotently) then `validate()`.
   Tests: `interrupted_create_self_heals_on_reopen`,
   `interrupted_delete_self_heals_on_reopen`.
3. **fail-loud on unexplained inconsistency.** A state inconsistent *without*
   a marker is not guessed at: duplicate canonical paths, empty/non-absolute
   ids, and unparseable index/marker files all raise
   `Error::WorkspaceCorrupt`. Tests:
   `duplicate_canonical_path_without_marker_fails_loud`,
   `corrupt_marker_fails_loud`.
4. **Archive admission + stop-then-archive ordering.** `archive()` asks the
   probe what is running; `Admit` refuses on any active work
   (`Error::WorkspaceActiveWork`); `StopThenArchive` persists the archive
   write **first**, then calls `probe.stop`. Tests:
   `archive_blocks_on_active_work_under_admit`,
   `stop_then_archive_persists_archive_before_stopping` (the fake probe
   records the archive flag it observed at stop time and asserts it was
   already `true`).
5. **Header-only index.** `index_sessions()` reads only each session's
   `.meta.json`; transcript bodies are never opened. Test:
   `index_sessions_reads_only_headers` (writes only a `.meta.json`).
6. **Non-destructive removal.** `remove()` drops the registry record only;
   the directory and its files stay. Test: `remove_is_non_destructive`.

### Wiring

- `src/lib.rs` — `pub mod workspace;`.
- `src/error.rs` — `Error::WorkspaceConflict`, `Error::WorkspaceCorrupt`,
  `Error::WorkspaceActiveWork`.
- `crates/recursive-cli/src/cli/workspace.rs` — `recursive workspace
  {list,add,show,archive,unarchive,remove,index,recover}` over the registry;
  `archive` uses `StoreActivityProbe` and refuses (suggesting `--stop`)
  unless `--stop` asks for stop-then-archive.
- `crates/recursive-cli/src/main.rs` — `Cmd::Workspace` variant + one match arm.

### Store-backed probe signals

`StoreActivityProbe` reports the signals that are observable from disk:
sessions whose status is `Active` (a live turn) and enabled cron triggers
(scheduled wakeups). Its `stop` disarms scheduled wakeups and returns exactly
those. Jobs and subagents are in-process with no on-disk registry — that is
exactly the capability seam: a runtime that owns them wraps the probe and
appends.

## Tests added

- 20 unit tests under `src/workspace/` (15 registry + 5 activity), all green:
  `cargo test --lib workspace::` → `20 passed; 0 failed`.
- 3 black-box CLI tests in `crates/recursive-cli/tests/cli_command_surfaces.rs`
  (new `workspace` section, reusing the existing spawn harness): lifecycle
  roundtrip (add → list → archive → list --all → remove, directory survives),
  archive-refuses-on-active-work, and `--stop` honesty.

## Gate results

- `cargo test --lib workspace::` ✅ (20/20).
- `cargo test --workspace` ✅.
- `cargo test --test invariants` ✅ (48 passed; 0 failed).
- `cargo clippy --all-targets --all-features -- -D warnings` ✅.
- `cargo fmt --all -- --check` ✅.
- `cargo test --doc workspace` ✅ (module doctest compiles).
- `sh .dev/scripts/cli-test-presence.sh` ✅ PASS (the new
  `crates/recursive-cli/tests/` change satisfies the gate — no opt-out).
- `sh .dev/scripts/agent-test-presence.sh` ✅ PASS.

### CLI smoke (isolated `RECURSIVE_HOME`, `RECURSIVE_SESSIONS_DIR` empty)

`workspace add` → `list` → `archive` (no active work) → `list --all` →
`remove` → `ls -d` all behave; the directory survives removal. A hand-written
index with two records sharing one canonical path makes `list` exit 1 with
`workspace registry corrupt: duplicate canonical path /a owned by `a` and `b``
— fail-loud confirmed end to end.

(Note: with a `RECURSIVE_SESSIONS_DIR` override set, the store probe reads the
override directory, so a smoke run in the pipeline env sees the pipeline's own
active session and `archive` correctly refuses — admission working as
designed; the override is a hard path override documented in `paths.rs`.)

## Decisions worth flagging

1. **Archive unit is the workspace, not a session.** DSH archives a project;
   accepting criteria's "会话 with an active job" is the workspace's active
   work (its sessions' live turns, its scheduled wakeups, plus whatever a
   runtime probe contributes). The directory and history are never deleted by
   archive *or* remove.
2. **Reads are lock-free, mutations serialise on one chain (`Mutex`).** The
   probe is invoked from inside `archive` while the chain is held, so a probe
   that wants to read registry state must be able to do so without the lock —
   hence read methods don't take it. The stop-ordering test relies on this.
3. **`StoreActivityProbe` reports turns but does not force-kill them.**
   A live turn's stop handle lives with the running runtime; a disk-only probe
   cannot reach it. It is reported (so `Admit` blocks) and left for the runtime
   seam to stop — and `ActivityProbe::stop` *returns* the work it actually
   stopped, so the CLI never claims a stop that did not happen.
4. **`Display` path vs canonical id kept separate** (`display` vs `id`) so the
   user sees the spelling they typed while identity stays realpath-only.

## Review round 1 (independent reviewer: NEEDS_FIX) — fixes

1. **`recover()` write-ahead order was inverted.** It unlinked each marker
   inside the parse/apply loop and only wrote the index afterwards, so a crash
   mid-recovery (or a corrupt marker sorting last) lost an applied mutation
   while its marker was already gone. Now: read *all* markers → apply → save
   the index → *then* unlink. A parse error aborts before anything is applied
   or cleared, so the whole journal stays for inspection. Regression test:
   `corrupt_marker_leaves_the_well_formed_markers_pending` (verified to fail
   against the previous loop: the good marker was unlinked, 1 marker left).
2. **CLI claimed work was "stopped" when the probe never stopped it.**
   `ActivityProbe::stop` now returns the subset of work it actually stopped
   (`Vec<ActiveWork>`, replacing the `bool` on `ArchiveOutcome`), the store
   probe returns only disarmed cron triggers, and the CLI prints `stopped` /
   `left running` per item. `--stop`'s help text now says what it really does.
   Tests: `store_activity_probe_stop_never_claims_a_turn_was_stopped`,
   `_reports_only_disarmed_schedules`,
   `stop_then_archive_reports_only_what_the_probe_stopped`,
   `workspace_archive_stop_reports_a_live_turn_as_left_running`.
3. **Declared gate `cli-presence` failed** — the new CLI surface had no
   test-bearing change. Fixed by adding the black-box tests above (no
   `RECURSIVE_CLI_TEST_PRESENCE=0` opt-out); the gate now reports PASS on the
   exact committed change set.
4. **`RegistryIndex.version` was written but never read.** `load_index` now
   refuses a registry whose schema version is newer than this build supports
   (mirroring `SessionReader::load_meta`). Test:
   `registry_written_by_a_newer_schema_is_refused`.
5. **`StoreActivityProbe` + `RECURSIVE_SESSIONS_DIR`** (documentation-level):
   the doc comment on the probe now states that the override is a hard one that
   ignores the workspace, so every workspace reports the override directory's
   live sessions.

## Invariant audit

| invariant | status |
|---|---|
| 1. Agent loop stays small | ✅ — no `run_inner` touch |
| 2. Orthogonality | ✅ — new `workspace` module; error variants; CLI subcommand |
| 3. Sandbox | ✅ — registry writes only under `user_data_dir()` |
| 4. Tests required | ✅ — 20 new unit tests + 3 black-box CLI tests |
| 5. No `unwrap()` in product code | ✅ |
| 6. No new deps | ✅ — reuses blake3 / serde / atomic_write |
| 7. Finish reasons are data | ✅ — unchanged |
| 8. Tool-call ↔ tool-result pairing | n/a |

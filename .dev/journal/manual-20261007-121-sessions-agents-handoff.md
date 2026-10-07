# Manual change — issue #121

- **Date:** 2026-10-07
- **Goal:** #121 fix(cli): `sessions` 无机器出口、`agents` 无活体判定——交接四问
  （status/为什么/花多少/最后发生什么）一个都答不全
- **Files touched:**
  - `crates/recursive-cli/src/main.rs`
    - `SessionCmd::List` / `Show` grew `--json`; `Show` also `--tail N` /
      `--full`; `Cmd::Agents` became `Agents { json }`. Every branch honours
      the **global** `--json` too (`json || cli.json`) — it used to be
      silently ignored under `Cmd::Sessions`.
    - `sessions list` human line now carries `updated_at`, `message_count`
      and a total-token column.
    - `sessions show` prints a `cost:` line (every token bucket) and honours
      `--tail` (last N entries) / `--full` (no 200-char preview cut); the
      legacy `.json` branch now shows `(last N of M entries)` and applies
      `--tail` to `--json` too.
    - `session_token_total` sums reasoning as its own bucket (Goal 273 prices
      it at the output rate). `cmd_agents`' human header/empty line now say
      "in-flight" instead of "active" (it lists `stale` rows too).
    - `cmd_agents` now calls `recursive::session::locked_by_live_process`
      (the lock+pid liveness that already existed for the *write* side in
      `session/lifecycle.rs`) and annotates each row `live` (a process holds
      the `.lock`) vs `stale` (recorded `Active`, no live owner — the P0-1
      corpse). `--json` emits `{count, agents:[...]}`.
    - New helpers/structs: `SessionListEntry` / `SessionListView` /
      `SessionShowView`, `session_entry_to_json`, `tail_entries`,
      `tail_messages`, `session_token_total`, `format_session_cost`,
      `agent_liveness`, `AgentRow`.
  - `src/http/session_mirror.rs` *(new)* — mirror a closing HTTP session into
    the native `<sessions>/<workspace-slug>/<id>/` layout (`.meta.json` +
    chained `transcript.jsonl`), the same treatment #57 gave AG-UI threads.
    HTTP sessions persist through `StorageBackend` (flat `<id>.jsonl` +
    `session-meta/<id>` KV) and were invisible to
    `SessionReader::list_sessions` (CLI list / resume picker / episodic
    recall). Best-effort: the flat transcript stays authoritative for cold
    load. The write takes a `SessionLock` (like `SessionWriter`) so it can
    never clobber a session a live `recursive resume` owns.
  - `src/http/mod.rs` — `mod session_mirror`; `mirror_closing_session` helper
    called from the two teardown paths that own a `SessionState`
    (`evict_idle_sessions`, `flush_all_sessions`). DELETE deliberately does
    **not** mirror: a deleted session must not reappear in the resume picker.
    The mirror root is **injected**: `AppState::session_mirror_root`
    (`Option<PathBuf>`, `None` = mirror off), resolved once at HTTP startup by
    `crates/recursive-cli/src/main.rs` through `user_sessions_dir`; every test
    fixture passes `None` (one in-crate flush test injects a tempdir and
    asserts the mirror landed there). It used to be resolved *inside* the
    mirror via `paths::user_sessions_dir`, which forced the HTTP integration
    tests to pin `RECURSIVE_HOME`/`RECURSIVE_SESSIONS_DIR` process-globally
    and flaked unrelated trigger tests — see "Fix round 2".
  - `src/session/mod.rs` — `ExportedTranscript` now carries `provider`,
    `updated_at`, `preset`, `cost` (previously dropped by
    `sessions export`). All new fields are `#[serde(default)]`, so old export
    JSON still deserialises.
- **Tests added:**
  - `crates/recursive-cli/src/main.rs::tests::agent_liveness_flags_live_stale_and_finished`
  - `…::session_token_total_sums_every_billed_bucket`
  - `…::format_session_cost_reports_each_bucket_or_a_placeholder`
  - `…::tail_entries_keeps_only_the_last_n`
  - `…::tail_messages_keeps_only_the_last_n`
  - `…::sessions_and_agents_accept_the_json_and_show_flags`
  - `src/http/session_mirror.rs::tests::{unsafe_ids_never_resolve_to_a_path,
    native_session_dir_resolves_under_the_injected_root,
    mirror_session_writes_under_the_injected_root,
    a_live_lock_blocks_the_mirror,
    mirror_writes_meta_and_chained_transcript,
    mirrored_transcript_is_a_chained_resume_seed,
    mirror_overwrites_instead_of_appending}`
  - `src/http/mod.rs::goal_121_session_mirror_wiring::http_entry_enables_the_native_session_mirror`
    + `goal_396_persistence_tests::flush_all_persists_and_drains_every_session`
    (now asserts the mirrored `.meta.json`/`transcript.jsonl` land under the
    injected root)
  - `src/session/mod.rs::tests::export_carries_provider_preset_updated_at_and_cost`
  - `crates/recursive-cli/tests/cli_session_surfaces.rs` (end-to-end against the
    real binary, isolated home):
    - `sessions_list_json_emits_the_persisted_meta`
    - `sessions_show_json_reports_cost_and_the_full_transcript`
    - `sessions_show_tail_limits_the_printed_window`
    - `agents_separates_live_stale_and_finished_sessions`
  - `tests/http_common/mod.rs` / `tests/http.rs` / every other `AppState`
    literal — `session_mirror_root: None` (no guard, no env). `tests/http.rs`
    and `tests/http_cold_load.rs` keep their original content otherwise.
  - `crates/recursive-cli/tests/cli_command_surfaces.rs` — updated the
    `agents` empty-output assertion to the new wording.
- **Notes:**
  - No new dependencies.
  - `sessions show --json` serialises the full `SessionMeta` plus the
    transcript entries (messages keep every persisted field; compaction
    boundaries surface with `type: "compact_boundary"`).
  - HTTP mirror is wired at eviction + graceful shutdown only (the two sites
    that own the `SessionState`). A session that is only ever DELETE'd is not
    mirrored — by design.
  - `session_mirror::mirror_session` has **no** `cfg!(test)` guard: the
    integration crates (`tests/http.rs`, `tests/http_cold_load.rs`) link the
    lib built without `cfg(test)`, so a guard would not have stopped them
    anyway. Instead the sessions root is a per-`AppState` value, so a test
    isolates (or disables) the mirror without touching process-global env.
  - Known limitation: the mirror runs at teardown only, so a *running* HTTP
    session is not yet in the native layout — `recursive agents` reports live
    native sessions but not a running HTTP session. Mirroring at session
    creation is a separate change.
  - Deferred (documented, not silently skipped): routing HTTP/AG-UI session
    persistence *through* `StorageBackend` so Redis/S3 deployments get the
    same unified layout — that is the layering follow-up called out in
    `src/agui_session.rs:31` (#56), a separate change.
  - Gates: `cargo test -p recursive-cli --bin recursive` (167 pass) +
    targeted `recursive-agent` lib tests (mirror + export) + `cargo fmt --all
    -- --check`.

## Fix round 2 (review: `VERDICT:NEEDS_FIX`)

- **Blocking (flaky `cargo test --workspace`) — fixed at the source, not by locking.**
  Round 1 guarded the two flush tests with `tests/http_common/mod.rs::SessionsHomeGuard`,
  which pinned `RECURSIVE_HOME` + `RECURSIVE_SESSIONS_DIR` process-globally behind a
  *private* mutex. Every concurrently running test in the `http` binary that resolves
  env-derived state (`TriggerStore::default_path` → `user_workspace_dir`) could then be
  redirected mid-test (2/3 runs red: `trigger_crud_list_get_delete_patch`,
  `webhook_fire_*`, `scheduler_fires_due_cron_and_advances` → 404). The guard is deleted;
  the mirror root is now injected per `AppState` (`session_mirror_root`, `None` =
  mirror off), so **no test in `tests/http*.rs` touches `RECURSIVE_HOME` /
  `RECURSIVE_SESSIONS_DIR` at all**.
  - `tests/http.rs` and `tests/http_cold_load.rs` are back to their pre-change content
    apart from `session_mirror_root: None` on the `AppState` literals.
  - Evidence: `cargo test -p recursive-agent --test http` → 126 passed, 3/3 runs;
    the reviewer's repro filters → 10/10 runs green; `--test http_cold_load` → 8 passed.
- **Minor — `mirror_into` could write `.meta.json` with no `transcript.jsonl`:** both
  artefacts are now serialized *before* either write (`serialize_transcript` +
  `to_string_pretty`), so a serialization failure writes neither.
- **Minor — `SessionsHomeGuard` duplicated `IsolatedWorkspace`:** gone with the guard;
  the in-crate evict/flush tests no longer pin env either (they inject `None` or a
  tempdir root), leaving only one test-isolating mechanism in play.
- **Minor — `status: Completed` unconditionally:** documented on
  `mirror_closing_session` (both call sites mirror *after* `runtime.close`).
- Also deleted the only remaining env coupling in this feature's tests
  (`session_mirror::tests::a_live_lock_blocks_the_mirror` used `IsolatedWorkspace`);
  `src/http/mod.rs` gained `goal_121_session_mirror_wiring`, a source-level pin that the
  `recursive http` entry still sets `session_mirror_root: Some(...)` (otherwise `None`
  would silently disable the feature in production), and
  `flush_all_persists_and_drains_every_session` now asserts the mirror landed under the
  injected root (previously nothing asserted the mirror on the teardown path), and
  `tests/http.rs::graceful_shutdown_mirrors_the_session_into_the_native_layout` drives
  the whole path from outside the crate (HTTP session → `flush_all_sessions` → the
  mirrored dir under the injected root, read back with `SessionReader`) — the coverage
  the disabled-in-fixtures mirror would otherwise have lost.
- Gates re-run: `cargo test --workspace` (4531 passed, 0 failed) ·
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean ·
  `cargo fmt --all -- --check` clean.
- Mutation check on the touched mirror module:
  `cargo mutants -p recursive-agent --file src/http/session_mirror.rs -F
  "role_str|prompt_of|build_meta|serialize_transcript"` → **10 caught, 1 unviable,
  0 missed** (`build_meta -> Default::default()` is unviable: `SessionMeta` has no
  `Default`). The `mirrored_transcript_is_a_chained_resume_seed` test added here is what
  kills the `role_str -> ""` and the `i > 0` parent-id mutants.
  (The unfiltered 22-mutant run was abandoned: ~10 min/mutant on this box.)

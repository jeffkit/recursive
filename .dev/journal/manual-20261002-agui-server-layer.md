# Manual landing of issue #56 — AG-UI server is now a layer (verdict + residual hardening)

- issue source: `.dev/issues/04-agui-server-not-a-layer.md` (架构缺陷系列 4/9, P0)
- branch:       `v2-pipeline-56-1001233712-cont` (base aaa49cc; WIP d958f92 / 9247a32 / 34f120e)
- verdict:      **completed with caveats** — layer extraction landed and verified; acceptance
  criteria on layering/tests/line-count are met; the "separate crate" end-state is
  deliberately deferred (see Caveat 2).

## Context found in the worktree

The flow had already landed the core extraction across three WIP commits:

- `d958f92` created `src/http/agui.rs` (~1,069 lines),
- `9247a32` shrank `handlers.rs` to the thin adapter (674 → ~110 lines for
  `agui_run`) and moved session dir helpers into `agui.rs`,
- `34f120e` added `build_session_runtime_parts` / `test_config_stub` so the
  AG-UI runtime assembly stays free of `AppState`.

The working tree at session start had, however, **reverted** the split: a
373-line inlined copy of `AguiConverter`, the interrupt store, both permission
hooks, and a 674-line monolithic `agui_run` were back inside `handlers.rs`,
`mod agui` had been dropped from `src/http/mod.rs` (making `agui.rs` dead,
uncompiled code), and the 8 AG-UI unit tests existed in **both** files.

## What this session did

1. **Restored the layered shape** (the fix direction of #56):
   - `src/http/mod.rs` re-declares `mod agui;`; `handlers.rs` back to the
     thin adapter (`agui_run` = parse body → `prepare_run` → admission →
     `build_agui_runtime` → `spawn_agui_run` → SSE framing, 110 lines).
   - All AG-UI symbols (`AguiConverter`, `sanitize_thread_id_for_session`,
     `agui_session_dir`, `OpenInterrupt` store, `AguiInterruptDetail`,
     `TestInterruptHook`, `ClientToolStub`, `ClientToolHook`) live only in
     `src/http/agui.rs`.
2. **De-duplicated the 8 unit tests** that existed in both files
   (converter framing ×2, sanitize ×5, seed mapping ×1) — they stay beside
   the implementation in `agui.rs`; `handlers.rs` keeps only the
   through-HTTP regression (`agui_non_resume_turn_seeds_full_messages_history`,
   issue #62).
3. **Clippy fixes in `agui.rs` tests**: redundant field names
   (`interrupt_before: interrupt_before`) and `clippy::await_holding_lock`
   on the two `pinned_home()` + `.await` tests (same
   `#[allow]` pattern the file elsewhere uses for the std env lock).
4. **Docs (issue 附带建议)**: new `docs/architecture/agui.md` — layer map,
   wire flow, interrupt/resume round-trip, testing strategy — plus an index
   entry in `docs/architecture/index.md` and a README section (previously
   `grep agui README.md` → nothing).

## Acceptance criteria vs reality

| #56 acceptance | status |
|---|---|
| server logic free of axum types, unit-testable without HTTP | ✅ `prepare_run` / `build_agui_runtime` / `spawn_agui_run` take `RunAgentInput` + workspace path; 16 unit tests in `agui.rs` run with no server (`cargo test --lib -- http::agui` → 18 passed) |
| `agui_run` reduced to parse → drive stream → SSE frame | ✅ 110 lines; no session mgmt / persistence / state machine in the handler |
| resume / interrupt-before state machine independently tested | ✅ coverage rule, 409 conflict, no-prior-run, no-open-interrupt, seed-minus-goal, cancelled sentinel — all in `agui.rs::tests` |
| `handlers.rs` shrinks; AG-UI symbols no longer scattered | ✅ 3,982 → 3,034 lines (vs pre-refactor base; `agui_run` 674 → 110); non-test AG-UI references in `handlers.rs` are only the adapter + error mapper |
| `cargo test --workspace` / clippy -D warnings / fmt green | ✅ with **one pre-existing flake**, not ours (see Caveat 1) |

## Caveats / follow-ups

1. **`tests/incremental_writes.rs` is flaky on this branch, its base
   (aaa49cc), d958f92, AND main (97b51ca)** — not caused by #56. Root cause
   (bisected to a deterministic 2-test repro): `SessionWriter::create`
   derives `session_id = filesystem_safe_timestamp() + slug` with **1-second
   resolution**; all 6 tests share workspace `/tmp/g152-test-ws`, so two
   tests that run within the same second get the **same session_id** and the
   second writer appends into the first test's `transcript.jsonl`
   (`left: 4, right: 2`). `--test-threads=1` does not help; per-test unique
   workspaces (or a uuid/collision-check suffix in `create`) would. This is
   issue-#57 territory (native session layout) — main already has
   `b213cff` toward it; the test side still collides on main today.
2. **`crates/recursive-agui/` (server as its own crate) is NOT done.** The
   issue's Suggested fix asks for a new crate depending only on a `ThreadStore`
   trait from #57. What landed is the *module* form: transport-free, unit-
   tested, axum-free — but still inside `src/`, still able to touch
   `crate::` internals directly. Doing the crate split before #57's
   `ThreadStore` would mean re-touching the persistence interface twice, so
   the module boundary is the right intermediate state. Follow-up once #57
   lands.
3. **Duplicate test names were only in the test module** — verified
   byte-identical bodies before deleting the `handlers.rs` copies, so no
   assertion coverage was lost.
4. A stash mishap mid-session (my README/`agui.rs` edits were in a stash
   whose pop conflicted with an unrelated older stash) was fully recovered
   from the dangling stash commit `c8099f3`; verified via `git diff HEAD`
   afterwards. No foreign changes were kept.

## Gates

- `cargo test --workspace` — all suites green except the pre-existing
  `incremental_writes` flake (Caveat 1); `recursive-agent` lib: **2,411 passed**;
  `http::` module: 101 passed; `http::agui`: 18 passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.

## Invariant audit

| invariant | status |
|---|---|
| 1. Agent loop stays small | ✅ untouched |
| 3. Sandbox / `resolve_within` | ✅ untouched |
| 5. No `unwrap()`/`expect()` in non-test code | ✅ new clippy work is test-only |
| 7. Finish reasons are data | ✅ Error outcome stays a `RunFinished` variant |
| 8. Tool-call ↔ tool-result pairing | ✅ seed mapping still skips unpaired halves; tests pin it |

## NEEDS_FIX resolution (2026-10-02) — rebase onto main, port #66 on top

The reviewed branch had forked at `aaa49cc` while main advanced 20+ commits;
the diff was part #56 extraction, part wholesale revert of landed main work
(#57/#63/#58/#59/#65/#69/#70/#54, flow v2 fixes). Resolution, per the review:

1. **Rebased the branch onto `main` (14af0cd2)** — every deleted main feature/
   test restored by the rebase itself; conflicts resolved toward main, with the
   #56 extraction re-applied on top. Verified identical to main afterwards:
   `src/agui_session.rs`, `src/tools/http_call.rs`, `tests/docs_living_paths.rs`,
   `tests/invariants/loop_size_orthogonality.rs`, flow v2 files, CI gates.
2. **Ported main's #66 work (`da0aa94c` + `182d5f94`) onto the layered shape**
   instead of letting the rebase drop it: `CancelOnDrop` + keep-alive in the
   thin `agui_run` adapter; the cancel token registry (`agui_active_runs`)
   insert/remove inside `spawn_agui_run`; admission permit + per-thread run
   fence (`SessionHost::try_begin_run`, 409 on duplicate runs) held by the
   driver task for the whole background run; `AguiConverter::open_accumulated`
   dedup (token deltas must not duplicate the final `AssistantText`);
   `.streaming(true)`; fixture `agui_active_runs` fields; `agui_e2e`
   HomeOverride pinning `RECURSIVE_SESSIONS_DIR`.
3. **Re-landed the reviewer-endorsed branch-local fixes** resolved against
   main: native-session persistence (`persist_run` — meta/cost/lock/uuid
   chain), resume disk splices (`apply_resume_tool_results`), blake3
   `thread_session_key` mapping (+ `distinct_thread_ids_never_share_a_session_directory`
   restored in handlers.rs tests), SessionWriter same-second collision fix
   (`create_in_same_second_gets_distinct_dirs`), `PinnedRecursiveHome` also
   clearing `RECURSIVE_SESSIONS_DIR`, `docs/architecture/agui.md` + README.
4. **Fixed on the way**: `build_session_runtime_parts` took the model from
   `RECURSIVE_MODEL` env — now a parameter fed `state.config.model` on both
   the REST and AG-UI paths (channels cannot drift). The dead legacy
   sanitiser left in `agui.rs` was dropped (it lives on as
   `legacy_sanitize_thread_id` in `src/agui_session.rs`).

The diff vs main is now 9 files, all #56/#57-adjacent: `src/http/agui.rs`
(new layer), `src/http/handlers.rs` (thin adapter), `src/http/mod.rs`
(`mod agui` + `test_config_stub`), `src/session/writer.rs` (collision fix),
`src/test_util.rs` (env pin), docs/README/journals.

Gates after resolution: `cargo test --workspace` 3,827 passed / 0 failed
(58 suites, incl. `agui_e2e` 8/8 with the three #57 native-session tests);
`cargo clippy --workspace --all-targets --all-features -- -D warnings` clean;
`cargo fmt --all -- --check` clean.

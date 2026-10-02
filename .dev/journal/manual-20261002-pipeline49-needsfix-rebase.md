# Manual change — pipeline-49 NEEDS_FIX resolution (rebase onto main)

- **Date**: 2026-10-02
- **Goal**: Address reviewer NEEDS_FIX on `v2-pipeline-49-1002005124-cont`
  (review of `git diff main` at HEAD `0ed4e37`): the branch diverged at
  `23189cd` while main advanced 20+ commits, so the reviewable diff showed
  wholesale reverts of landed work. Required: rebase, drop stray artifacts,
  restore guard tests/CI steps, re-run gates.
- **Files touched**:
  - History rewrite (no content change): the 3 WIP commits
    (`e13b077`/`6389ae4`/`fc1ed71` after rebase) + stray-artifact removal +
    pipeline-49 deltas squashed into `4e4ab6b`; the real pre-rebase tree is
    preserved in `stash@{0}` ("pipeline-49 deltas + journal edit, pre-rebase
    backup 20261002-023439").
  - `tmp-g152-test-ws/**` — dropped (committed by WIP commit `ea01c7a`;
    not covered by Cargo.toml `exclude`, would ship in the crate).
  - `src/tools/mod.rs` — restored `#[cfg(feature = "web_fetch")]` on
    `pub use web_fetch::WebFetch` (commit `3a2a2fe`). This is the only
    *source* delta beyond the branch's own pipeline-49 work.
- **What was NOT re-implemented** (restored by the rebase itself, verified
  present on the rebased tree): #57 `src/agui_session.rs` + handlers wiring;
  #69 `Config::from_env` RECURSIVE_ALLOW_TOOLS read + the three pinning tests;
  `ToolCall` in `message.rs` + `message_module_does_not_import_llm`
  invariant; #59 `tests/docs_living_paths.rs`; flow-engine fixes
  (`97b51ca` review budget 5400, `18a4ce1` quote-escaping + s18, `97246d5`
  L2 resume + s9/s10, no swallow-fallback in `flow_v2_paths.py::_eval`);
  `.zcode` engine table (`78f13af`); CI `--no-default-features` guard steps
  (kept — see below); `#[allow(dead_code)]` on `epoch_day_to_ymd` and the
  `#[cfg(not(feature = "web_search"))]` silencer (`c526280b`, already on main).
- **The no-default-features finding was real and is now fixed**: both CI
  guard combos failed at the merge-base and on pristine main
  (E0432 `web_fetch` at `src/tools/mod.rs:169` — 23189cd dropped the cfg
  from the re-export only). Fixed with the one-line cfg restore; the
  unmerged pipeline-61 branch carries the identical fix as `f279dad7`.
  Note for a follow-up: `cargo check --no-default-features
  [--features http] --lib` only passes with an **empty ambient
  RECURSIVE_SESSIONS_DIR** is irrelevant here, but `RUSTFLAGS=-D warnings`
  now also passes (verified both with and without).
- **Tests added**: none new —
  `env_schema_description_matches_per_tier_reality`,
  `allow_tools_from_env`, `env_schema`-adjacent poll mitigation,
  resume_by_id env pins and the writer uuid-suffix behaviour all carried
  over from the branch's own deltas and still pass.
- **Gates (this worktree, ambient pipeline env neutralised)**:
  - `env -u RECURSIVE_SESSIONS_DIR cargo test --workspace` →
    **3806 passed / 0 failed** (57 suites; includes agui_e2e 8/8).
    Heads-up for future runs in this worktree: the pipeline exports
    `RECURSIVE_SESSIONS_DIR` (Goal-H J1 hard override) and
    `tests/agui_e2e.rs::HomeOverride` pins only `RECURSIVE_HOME`, so
    suite-local `list_sessions` assertions can see the shared pipeline
    sessions root and double-count. The gate command above is the
    canonical invocation.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean
  - `cargo fmt --all -- --check` → clean
  - `cargo check --no-default-features --lib` → green (both with and
    without `--features http`), `RUSTFLAGS=-D warnings` included.
- **Notes**: after the rebase the branch is exactly main + 2 commits
  (`4e4ab6b` pipeline-49 deltas, `3a2a2fe` web_fetch cfg); `git diff main`
  is 9 files / +311 −17 with no deletions of landed main work.

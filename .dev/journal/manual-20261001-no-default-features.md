# Journal: fix --no-default-features build (issue #55)

Date: 2026-10-01
Goal: Make `cargo check/clippy --no-default-features --lib` pass for the bare
kernel and all 8 single-feature combos (issue #55, P0, arch-defect series 3/9),
plus a CI feature-matrix guard so it cannot rot again.

## Files touched
- `src/tools/run_background.rs` — new neutral home of
  `pub const DEFAULT_KILL_GRACE_PERIOD` (moved out of `mcp`).
- `src/mcp.rs` — re-exports the constant from `tools::run_background`
  (`pub use`), so all existing `mcp::DEFAULT_KILL_GRACE_PERIOD` users keep
  working.
- `src/acp/session.rs:178` — now references
  `crate::tools::run_background::DEFAULT_KILL_GRACE_PERIOD` instead of
  `crate::mcp::…`; `acp` no longer depends on the `mcp` feature (helps #54).
- `src/tools/registry.rs` (`build_standard_tools_with_transport_opt`) —
  `#[cfg(not(feature = "web_search"))] let _ = (&provider,&key,&jina);`
  consumes the three web-search params when the feature is off (fixes 3
  unused-variable warnings under `-D warnings`).
- `src/session/mod.rs:357` — `epoch_day_to_ymd` gets
  `#[cfg_attr(not(feature = "http"), allow(dead_code))]` (only http handlers
  use it; dead in the bare kernel).
- `.github/workflows/ci.yml` — new `feature-matrix` job: matrix of
  `["", mcp, http, cli, web_fetch, anthropic, skill-hub, web_search]`, each
  running `cargo check --lib --no-default-features [features]` AND
  `cargo clippy --lib --no-default-features [features] -- -D warnings`.

## Tests added
- `src/session/writer.rs::create_in_same_second_gets_distinct_dirs` —
  regression pin for the session-id collision fix (second `create` within the
  same second must get its own directory, not silently reuse the first's
  transcript).

## Notes
- README:38-39 promise is now true again (bare kernel builds, clippy-clean);
  no README change needed.
- Verified locally: all 8 combos `cargo check` + `clippy -D warnings` green;
  full `cargo clippy --all-targets --all-features -D warnings` green;
  `cargo test --workspace` green across repeated runs; `cargo fmt --check` ok.
- Continuation pass (2026-10-02): full-suite `cargo test --workspace` exposed
  3–4 `incremental_writes` / `resume_by_id` failures that were NOT caused by
  the feature fix but by `RECURSIVE_SESSIONS_DIR` (hard override, Goal-H J1)
  leaking the pipeline env into every writer + same-second session-id
  collisions. Fixed the same way main did in 4e4ab6b6:
  - `src/session/writer.rs` — session ids get a random 8-hex suffix when the
    timestamped directory already exists (real product bug: silent merge of
    two sessions created in the same second);
  - `src/test_util.rs` — `PinnedRecursiveHome` / `IsolatedWorkspace` also pin
    `RECURSIVE_SESSIONS_DIR` under the same `env_lock`;
  - `tests/resume_by_id.rs` — `SessionEnvGuard` pins both vars;
  - `src/config.rs` — `from_env_injects_memory_and_scratchpad_layers` pins
    `RECURSIVE_SESSIONS_DIR` for its span (episodic recent-N window).
- Long-term follow-ups stay in #53/#54 (make acp/schema optional so the bare
  kernel is actually minimal, not just compilable).
- NEEDS_FIX resolution (2026-10-02, pipeline-55 continuation branch
  `v2-pipeline-55-1002191734-cont`): independent review rejected the branch
  because it was 66 commits behind `main` — `git diff main` read as wholesale
  reverts of landed work (#56 agui_session/http::agui layering, #57 AG-UI
  threads as native sessions, #66 cancel + streaming + run fence, #63/#65/#69
  http_call/skills/allow-tools env, #59 living-docs + invariant-registry guard
  tests). Resolution: merged `main` (a7104bdc) into the branch; conflicts only
  in `.github/workflows/ci.yml` (kept this branch's stronger `feature-matrix`
  job on top of main's two inline --no-default-features checks),
  `src/session/mod.rs` (kept the feature-scoped `cfg_attr` allow from this
  branch), `src/session/writer.rs` (kept main's #57 additions
  `create_at`/`open_or_create`/`add_usage`/`update_identity` + tests alongside
  this branch's collision guard). Reviewer nit: the duplicated
  `#[async_trait::async_trait]` on `TestInterruptHook` does not exist on this
  branch (single `#[async_trait]` at `src/http/agui.rs:389`, matching main);
  verified with a workspace-wide scanner — no duplicated attribute anywhere
  in `src/`. Post-merge gates all green: `cargo test --workspace` 58/58
  result lines ok, clippy `--workspace --all-targets --all-features -D
  warnings` clean, `fmt --check` clean, and all 8
  `--no-default-features [features]` lib combos clippy-clean. `git diff main`
  is now 7 files / +99 −10: the feature-matrix CI job, the
  `DEFAULT_KILL_GRACE_PERIOD` move + re-export, the `epoch_day_to_ymd`
  cfg_attr, the web_search param-consumption cfg, the session-id collision
  guard, and the journal.

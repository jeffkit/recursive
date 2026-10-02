# Issue #56 branch — second rebase reconciliation (main: SkillSource + v3 host)

- branch:  `v2-pipeline-56-1002105237-cont` (continuation of pipeline-56)
- main at: `fafac9eb` (v3 分布式宿主) — branch had forked at merge-base `fac622dc`
- action:  `git merge main` into the branch (merge commit `b921b56d` + fmt `e419f029`)

## Why

The first NEEDS_FIX rebase (see `manual-20261002-agui-server-layer.md`) rebased onto
main `14af0cd2`, but main then advanced further (SkillSource trait split #74, v3
distributed host, flow-v2 fixes). A `git diff main...HEAD` showed the branch carrying
**wholesale reverts of that newer main work**: `src/skills.rs` missing
`SkillSource`/`StaticSkillSource` (−209 lines), `src/tools/load_skill.rs` back to the
`Vec<Skill>` form, `src/lib.rs` re-export trimmed, and `.dev/flows/self_improve_bridge_v2.py`
back to the pre-v3 host (no `run_host_v3`, no `note_retry`). The #56 surface itself
(`src/http/agui.rs`, thin `agui_run`, `test_config_stub`, session-writer collision fix,
`PinnedRecursiveHome`, docs) was already correct and identical-to-or-ahead-of main.

## Merge resolution (4 conflicts, all in `src/http/handlers.rs`)

| conflict | resolution |
|---|---|
| `build_session_runtime` body | **HEAD** (`build_session_runtime_parts` + layered setters) — main's copy is the pre-#56 monolith; the layered form feeds both REST and `/agui` from the same context-management assembly |
| re-added `AguiConverter` block (~280 lines) | **HEAD** (deleted) — lives in `src/http/agui.rs`; duplicated definition would be a compile error |
| re-added monolithic `/agui` driver body (~430 lines) | **HEAD** (deleted) — lives in `agui.rs::spawn_agui_run`. **#66 semantics ported into the layer**: `FinishReason::Cancelled` → `record_run_failed` metrics + `RunFinished Error { code: "cancelled" }` (main's post-14af0cd2 improvement, previously only in the monolith) |
| dangling tail (cancel registry / monitor spawn) | **HEAD** (deleted) — replaced by the thin adapter's `spawn_agui_run` + `CancelOnDrop` + keep-alive + `agui_cancel` + `agui_prepare_error_response` (restored verbatim from pre-merge HEAD; `metrics_handler` re-appended from the same source after the conflict eating made `mod.rs`'s import dangle) |

main-side work landed untouched by the merge: SkillSource/StaticSkillSource,
`LoadSkill::from_source`, `registry.rs` wiring, `lib.rs` re-exports, v3 host bridge +
flow-v2 test updates, `manual-20261002-skillsource-trait.md`.

## Post-merge shape (diff vs main is now exactly the #56/#57 surface)

`README.md`, `docs/architecture/agui.md`, `src/http/agui.rs` (new layer, 1,829 lines),
`src/http/handlers.rs` (thin adapter: 4,502 → 1,746 lines; `agui_run` 654 → ~211),
`src/http/mod.rs` (`mod agui` + `test_config_stub`), `src/session/writer.rs`
(same-second collision fix), `src/test_util.rs` (`PinnedRecursiveHome` also clears
`RECURSIVE_SESSIONS_DIR`). No `.dev/` or `src/skills.rs` delta remains vs main.

## Gates

- `cargo test --workspace` — **3,796 passed / 0 failed** (58 suites; lib 2,427;
  `http::` 63 incl. through-HTTP #62 regression; `http::agui` 13; `agui_e2e` 8/8;
  invariants 43).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean (post-merge autofix committed separately).

**CORRECTION (NEEDS_FIX round 2, 2026-10-02):** the numbers above were green
but the suite *set* was silently thinner than main's: the merge had dropped
main's 39-test `handlers.rs` test module, keeping only 13 in `agui.rs`
(#66 anti-double-render/cancel tests, `agui_run_respects_run_semaphore`,
the #62 through-HTTP regression, `parse_permission_mode_all_variants`,
`sse_message_from_canonical_*`, `format_timestamp_*`, `tool_progress_*`,
and several handler tests had no equivalent anywhere). Round 2 restored them
(32 in `handlers.rs::tests`, 18 in `agui.rs`) and fixed the two behaviour
regressions this merge also introduced (`/agui` wall-clock budget dropped
from the `build_agui_runtime` chain; resume state machine ran before the
per-thread fence). Details in `manual-20261002-agui-server-layer.md`.

## Verification of "no phantom deletions"

- `pub trait SkillSource` present at `src/skills.rs:217`; `LoadSkill::from_source`
  wired in `src/tools/registry.rs`.
- `run_host_v3` + `note_retry` present in `.dev/flows/self_improve_bridge_v2.py`.
- AG-UI symbols in `handlers.rs` are adapter-only (parse → prepare → admission →
  build → spawn → SSE frame + cancel/error mapping); state machine, converter,
  interrupt store, hooks, driver all live in `src/http/agui.rs`.

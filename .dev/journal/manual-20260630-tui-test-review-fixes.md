# Manual edit: tui-test-review-fixes

**Date**: 2026-06-30
**Goal**: Act on the TUI test-mechanism review. Seven findings, fixed by
priority (high → low). Net effect: the mutation gate and PTY tour go from
advisory SOP steps to enforced / automated gates; the PTY harness stops
sleeping a fixed `--wait-ms`; the backend cancel path gets an end-to-end
test; coarse highlight-detection API removed; parse_keys boundary cleaned.

## Findings & fixes

1. **[P1, important] Wire the mutation gate into the self-improve flow.**
   `.dev/scripts/tui-mutants.sh` was SOP-only (step 3) — an AI could skip
   it and land weak TUI tests. The project's **active** self-improve path
   is the Flowcast flow `.dev/flows/self-improve.flow.js` (per
   `.dev/flows/SELF_IMPROVE.md`, it "等价替代" the legacy bash wrapper);
   its quality gates are `cargo test / clippy / fmt` (built-in) plus
   project gates loaded from `.flowcast/gates.json` via `mergeGates`.
   Added a `tui-mutants` entry to `.flowcast/gates.json`:
   `cmd: sh .dev/scripts/tui-mutants.sh`, `onFail: resume-fix`,
   `timeout: 1200000`. `tui-mutants.sh` already self-skips (exit 0) when
   no `crates/recursive-tui/src/` file changed vs `main...HEAD`, so the
   gate is a no-op for non-TUI goals. `onFail: resume-fix` feeds the
   survivor report back to the agent to strengthen tests then re-runs —
   the actual effectiveness loop. Also kept the parallel gate in the
   legacy `self-improve.sh` (after the smoke gate, with one resume-fix
   chance) since CLAUDE.md / AGENTS.md still reference that wrapper;
   `RECURSIVE_TUI_MUTANTS=0` opts out of the bash path.

2. **[P2, important] Automated PTY regression gate.**
   The PTY tour was manual; `lib.rs` raw-mode / alternate-screen / mouse
   was the least-covered layer (mutation gate explicitly allows survivors
   there). Refactored `tui-pty-harness` into a **lib + bin** so tests can
   call the engine directly (subprocess `cargo run` from a test risks a
   target-dir build-lock deadlock; cross-crate `CARGO_BIN_EXE_` resolution
   is fragile). Added `crates/recursive-tui/tests/pty_regression.rs` —
   boots the real `recursive-tui` binary under a PTY via
   `CARGO_BIN_EXE_recursive-tui` and asserts the splash (`/resume` +
   `/help` hint) and the `/help` modal. Runs on every
   `cargo test -p recursive-tui`.

3. **[P3, medium] PTY harness content-stability poll.**
   `spawn_and_snapshot` slept a fixed `--wait-ms` and snapshotted — flaky
   on slow CI, slow on fast machines. Replaced with a poll: the reader
   thread tracks when the rendered screen last changed; the main thread
   snapshots once the screen has been stable for `--stable-ms` (default
   120), capped at `--wait-ms`. Added `--stable-ms` flag. Critical fix:
   the poll must NOT declare "stable" before the first render — a
   `got_output` flag ensures a slow-booting TUI isn't captured as blank
   (caught by `pty_boot_renders_splash` failing on the first run).
   Added `stability_poll_returns_early_when_child_exits` regression test.

4. **[P4, medium] `cargo-mutants` missing → hard-fail.**
   `tui-mutants.sh` exits 2 when cargo-mutants is absent, but
   `self-improve.sh` didn't treat that as a failure — so a missing
   prerequisite silently skipped the gate. The new P1 gate handles exit 2
   explicitly: hard-fail with install instructions (`cargo install
   cargo-mutants`) or opt-out via `RECURSIVE_TUI_MUTANTS=0`.

5. **[P5, small] Backend worker-loop coverage.**
   Confirmed existing coverage: event mapping, shell dispatch, offline
   mode, `wait_for_cancel`, `interrupt_action_sets_cancel_flag`. Gap: the
   full cancel-during-turn path (abort → `truncate_transcript` →
   `UiEvent::Interrupted`) was not end-to-end tested — the flag test only
   checked the flag flips. Added `interrupt_aborts_running_turn_and_emits_interrupted`:
   a `HangTool` (`std::future::pending`) + a `MockProvider` with
   `with_on_complete(notify)` lets the test deterministically race an
   `Interrupt` against an in-flight turn and assert `UiEvent::Interrupted`
   arrives. Covers the backend layer the in-process harness can't reach.

6. **[P6, small] Remove coarse `Screen::has_bg` / `row_has_bg`.**
   These coarse helpers (any non-default bg) invited the highlight-bar
   anti-pattern the SOP warns against — panel base fills make every row
   "have bg". No test used them (the `has_bg` in `transcript.rs` is a
   local var, not the method). Removed both methods + the now-unused
   `Modifier` import. Kept `row_has_bg_color` / `row_has_bg_other_than`
   (fine-grained). Updated SOP, goal template, and harness module doc.

7. **[P7, small] `parse_keys` `\xNN` boundary.**
   `i + 3 < bytes.len() + 1` was an awkward way to write
   `i + 4 <= bytes.len()`. Replaced with the clear guard; behaviour
   unchanged, existing parse_keys tests still pass.

## Files touched

- `.flowcast/gates.json` — add `tui-mutants` project gate (the ACTIVE
  Flowcast path reads gates from here via `mergeGates`) (P1, P4).
- `.dev/scripts/self-improve.sh` — **deprecated** (deprecation banner +
  stderr nudge pointing to the flow); reverted the TUI gate block I had
  added here so no new logic lives in a deprecated script. The flow owns
  the gate now (P1, P4).
- `.dev/skills/tui-acceptance.md` — step 3 marked enforced via the flow's
  `tui-mutants` gate; anti-patterns
  updated (has_bg removed, stable-ms note added).
- `.dev/AGENTS.md` — TUI mutation gate added to the hard-gate list.
- `CLAUDE.md` — mandatory quality gates mention the TUI mutation gate.
- `.dev/goals/_TEMPLATE-tui-acceptance.md` — has_bg reference reworded.
- `crates/tui-pty-harness/Cargo.toml` — add `[lib]`.
- `crates/tui-pty-harness/src/lib.rs` — **new**: engine (pub) + engine
  tests, stability poll with `got_output` guard (P3, P7).
- `crates/tui-pty-harness/src/main.rs` — slimmed to a CLI wrapper over
  the lib.
- `crates/recursive-tui/Cargo.toml` — `tui-pty-harness` dev-dep.
- `crates/recursive-tui/tests/pty_regression.rs` — **new**: PTY
  integration regression gate (P2).
- `crates/recursive-tui/src/harness.rs` — remove `has_bg` / `row_has_bg`
  + `Modifier` import (P6).
- `crates/recursive-tui/src/backend.rs` —
  `interrupt_aborts_running_turn_and_emits_interrupted` test (P5).
- `crates/recursive-tui/src/skill_commands.rs` — two pre-existing clippy
  1.95 lints (`unwrap_used` at non-test line, `needless_borrow` in test)
  fixed so the gate is green; unrelated to the review but blocking.
- `Cargo.lock` — updated for the new dev-dep.
- `CLAUDE.md`, `AGENTS.md`, `.dev/AGENTS.md`, `.dev/README.md`,
  `.dev/OPERATIONS.md`, `website/{zh,en}/guide/self-improve.md`,
  `.claude/skills/recursive-loop/SKILL.md`,
  `docs/architecture/{agent-loop,sessions}.md`,
  `crates/recursive-cli/src/{main.rs,cli/output.rs}`,
  `tests/invariants/finish_reason_data.rs` — migrate all
  `self-improve.sh` references to the Flowcast self-improve flow
  (`.dev/flows/self-improve.flow.js`) and mark the legacy bash wrapper
  deprecated. Historical journals / goals / proposals / observations
  left intact (point-in-time records).

## Deprecation of the legacy bash wrapper

Per operator direction: `.dev/scripts/self-improve.sh` (and
`parallel-self-improve.sh`) are deprecated. The canonical self-improve
path is the Flowcast flow. Concretely:
- `self-improve.sh` gained a deprecation banner + per-invocation stderr
  nudge (`RECURSIVE_LEGACY_BASH_SELF_IMPROVE=1` silences). The TUI gate
  block I added earlier was reverted — no new logic in a deprecated
  script; the gate lives in `.flowcast/gates.json` and is enforced by
  the flow.
- All live, authoritative docs (CLAUDE.md, AGENTS.md, .dev/AGENTS.md,
  .dev/README.md, .dev/OPERATIONS.md, website guides, the
  /recursive-loop skill, architecture docs, and code/test comments
  describing the auto-resume contract) now reference the flow.
- Historical journals / goals / proposals / observations were NOT
  edited — they are point-in-time records and rewriting them would
  falsify history. `.dev/OPERATIONS.md` keeps its bash-specific body as
  legacy reference under a prominent "canonical path is the flow"
  banner.

## Tests added

- `tui_pty_harness::tests::stability_poll_returns_early_when_child_exits`
- `recursive_tui::backend::tests::interrupt_aborts_running_turn_and_emits_interrupted`
- `recursive_tui` integration `pty_regression::{pty_boot_renders_splash, pty_help_command_opens_modal}`

## Quality gates (in main checkout)

- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui -p tui-pty-harness --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui -p tui-pty-harness` — 300 lib + 2 PTY
  regression + 6 pty-harness, all green. PTY regression re-run 3× stable.

## Notes

- The mutation gate (P1) is enforced by the Flowcast self-improve flow
  via the `tui-mutants` project gate in `.flowcast/gates.json`, not via a
  `cargo test` — `cargo-mutants` mutates source in-place and is slow, so
  it belongs in the flow's gate chain, not the test suite. Run manually:
  `.dev/scripts/tui-mutants.sh`. The legacy `self-improve.sh` is
  deprecated and does NOT carry this gate.
- The PTY regression test boots the real binary; it inherits the user's
  `~/.recursive/config.toml`. That's fine — the splash renders regardless
  of online/offline state. If the binary ever fails to boot without
  config, the test will catch it.
- Did NOT touch the worktree workflow — this was a direct manual edit
  session in the main checkout (no in-flight self-improve run; `.dev/runs/`
  and `.worktrees/` checked empty).

## Follow-up: close the "write tests when writing TUI code" gap (A+B+C)

Operator flagged a gap: the mutation gate is *reactive* — it only fires
after a run, so an agent writing TUI code with zero/tautological tests
wastes a resume-fix cycle, and direct (non-flow) edits had no
enforcement at all. Closed at three layers:

- **A. Contract**: `.dev/AGENTS.md` now states TUI src changes MUST ship
  a test in the same commit (in-process `Harness`, `tests/`, or PTY), so
  the flow's system-prompt injection makes the agent proactive.
- **B. Goal template**: `.dev/goals/_TEMPLATE-tui-acceptance.md` marks
  the Tests section REQUIRED and adds a presence-gate acceptance bullet.
- **C. Presence pre-gate**: new `.dev/scripts/tui-test-presence.sh` —
  fast (ms) check that fails (exit 1) if `crates/recursive-tui/src/`
  changed with no test-bearing addition (`#[test]`/`#[cfg(test)]`/`mod
  tests` in a changed src file, a `tests/` change, or a `tui-pty-harness`
  change). `RECURSIVE_TUI_TEST_PRESENCE=0` opt-out for pure refactors
  (journal-documented). Registered as the `tui-presence` flow gate in
  `.flowcast/gates.json`, ordered BEFORE `tui-mutants` (final chain:
  test → clippy → fmt → e2e → tui-presence → tui-mutants). Verified all
  three paths: skip (no TUI src), pass (test marker added), fail (src
  only). Also documented in CLAUDE.md mandatory gates for direct edits.

No git pre-commit hook added — the repo has no committed-hook convention
(`.git/hooks` is local-only), so a hook wouldn't share across clones.
Direct-edit enforcement relies on the CLAUDE.md instruction + the script;
flow enforcement is automatic. A committed `core.hooksPath`-based hook
is a possible future hardening.

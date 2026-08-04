# Manual edit: tui-mutants gate — pty baseline flake fix (boot-starvation retry)

**Date**: 2026-08-04
**Goal**: Make `bash .dev/scripts/tui-mutants.sh` green for the Goal-383 worktree
(`selfimprove-1785804163884`, HEAD `06aa17a`, uncommitted TUI graceful-cancel changes in
`crates/recursive-tui/src/backend.rs` + `events.rs`). Round 2/3: the gate failed at the
**unmutated baseline** with `error: test failed, to rerun pass -p recursive-tui --test
pty_regression` — `pty_boot_renders_splash` panicked at `pty_regression.rs:86` with an empty
screen (`boot should show either the online splash or the offline setup guidance, got:` and
nothing after it). No mutants were even tested (`ERROR cargo test failed in an unmutated tree`).

## Diagnosis

The failure is a **load-induced baseline flake**, not a code regression:

- `cargo-mutants` runs the whole `recursive-tui` test suite (including the PTY integration
  tests) for the unmutated baseline AND for every mutant, with `--jobs = hw.ncpu` (14 on this
  Mac) in copy mode.
- `tui_pty_harness::spawn_and_snapshot` polls for screen stability but **caps at `wait_ms`
  of wall clock** (`crates/tui-pty-harness/src/lib.rs`, the `start.elapsed() >= cap` break).
  The `got_output` guard only prevents declaring "stable" before the first render — it does
  NOT extend the cap.
- The PTY tests passed `tour("", 3000)` — a 3 s boot cap. Under 14-way parallel
  cargo-mutants load, the real `recursive-tui` binary takes > 3 s to reach its first frame,
  so the cap fires and the snapshot is blank.
- Verified not-a-regression: isolated re-run of `cargo test -p recursive-tui --features
  "recursive/test-utils,weixin" --test pty_regression` passes in ~1.4 s (and again in 1.07 s
  after the change). Round-1 journal (`manual-20260804-goal383-e2e-gate-infra.md`) already
  flagged this flake mode: "the `pty_boot_renders_splash` 3s boot cap is too tight under CPU
  starvation".

## Changes

**`crates/recursive-tui/tests/pty_regression.rs`** (test-only; no product-code change):

- Extracted the single-attempt tour into `tour_once(keys, wait_ms)`.
- Added `const BOOT_RETRY_MS: u64 = 15_000` and a `tour()` wrapper: if the first snapshot
  comes back **blank** (binary still booting — the cap fired before any output), retry once
  with the 15 s budget. Non-blank snapshots are never retried; the assertions stay strict
  (a blank screen is retried, never accepted as a valid splash).
- 15 s is a *cap*, not a sleep — the stability poll still returns as soon as the screen
  settles, so a healthy boot (≈1.1–1.4 s) never pays the full budget; the retry only
  materialises under the CPU starvation that caused the flake.
- Updated the module doc to stop claiming unconditional "non-flaky on slow CI" and describe
  the retry.

Why retry-on-blank instead of just raising the cap to 15 s: raising the cap directly would
make every mutant's test run wait up to 15 s *whenever the machine is loaded* (compounding
across 14 parallel jobs and every mutant), while the retry keeps the common fast path at the
3 s cap and only spends the long budget when the first snapshot is actually blank.

## Tests added

- None new; both existing PTY tests (`pty_boot_renders_splash`, `pty_help_command_opens_modal`)
  now exercise the `tour()` retry wrapper (fast path). The retry path itself is trivially
  simple (blank → one retry) and is covered by the gate run: under the gate's own parallel
  load any blank first snapshot now gets a second chance instead of failing the baseline.

## Verification (all green)

- `cargo test -p recursive-tui --features "recursive/test-utils,weixin" --test pty_regression`
  → 2 passed, ~1.07 s
- `cargo test --workspace` → 0 failed
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean
- `.dev/scripts/tui-test-presence.sh` → PASS
- `bash .dev/scripts/tui-mutants.sh` → **GATE_EXIT=0**; baseline `ok Unmutated baseline in
  39s build + 6s test` (was `FAILED ... 80s build + 21s test`); `5 mutants tested in 2m:
  3 caught, 2 unviable` — **0 missed / 0 survived**.

## Notes / lessons

1. **The tui-mutants gate can fail at the *unmutated baseline* before testing any mutant**
   when the machine is loaded — the PTY boot cap (3 s) is shorter than a CPU-starved binary's
   first frame. The fix must live in the PTY tour's boot budget (retry on blank), not in the
   gate script's parallelism (parallelism is what keeps the gate within its 20-min timeout).
2. **`cargo-mutants --in-diff` on a 700-line backend.rs change can be small** — this run
   produced only 5 mutants; the expensive part is the baseline build + the PTY tests running
   for every mutant.
3. The sibling Goal-384 worktree was in its review phase during this gate run (no concurrent
   cargo-mutants/docker build), which also helped the baseline pass; run manual gate
   re-runs when the machine is idle-ish to avoid the load flake entirely.

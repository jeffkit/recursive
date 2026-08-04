# Manual edit: Goal-383 e2e gate round — infra failure (killed run), not code

**Date**: 2026-08-04
**Goal**: Fix the `e2e` gate red for the Goal-383 worktree (`selfimprove-1785804163884`,
HEAD `06aa17a`, uncommitted TUI graceful-cancel changes in `crates/recursive-tui/src/backend.rs`
+ `events.rs`). The flow's fix-round prompt said "edit source files to fix every error" but the
`.gate-e2e-output.log` was truncated (9 lines ending at the plugin `tsc` build) with no error
lines — the known signature of a **killed gate run**, not a code failure (see
`manual-20260803-goal382-stream-interrupt-persist.md`).

## Diagnosis

1. **Stranded state from the flow's 600s-timeout kill** (AGENTS.md failure mode #4 + goal-364
   update): alive mcp2cli session daemon `argusai-wt-06aa17a`, `wt-06aa17a-aimock` container,
   and no `recursive:e2e-wt-06aa17a` image (the `argus-build` docker release compile inside the
   colima VM takes 12-14 min alone — far beyond the flow's 600s e2e gate timeout; the gate's
   stdout only shows the plugin build, everything after goes to files, so the truncated log is
   exactly what a 600s kill captures).
2. **Concurrent-worktree contention made it worse**: a sibling worktree
   (`selfimprove-1785806032998`, Goal-384, HEAD `dc895b6`) was running its own e2e gate in the
   SAME colima VM. Two release builds shared the VM → my build ran 48 min at the builder stage,
   then the docker build client was SIGINT'd (exit 130, `argus-build` reported
   `"Build exited with code 130"`), the session died, and the gate failed fast with
   `smoke FAILED ✗`. The sibling's build survived and passed (its image `e2e-wt-dc895b6` was
   created 10:03). Root cause of the SIGINT was not pinned to a single actor; the practical
   fix was to re-run when the VM is idle.
3. **`tui-mutants` baseline flake**: the first tui-mutants re-run failed at the *unmutated*
   baseline (`cargo test` exit 4) because `pty_boot_renders_splash` got an empty screen —
   `tui_pty_harness` caps boot at `wait_ms=3000` (crates/tui-pty-harness/src/lib.rs), and under
   heavy load (docker build in VM + 14 parallel cargo-mutants jobs) the real TUI binary cannot
   boot + render in 3s. Isolated re-run of the pty test passes (1.4s). Not a code regression.

## Changes

No product-code edits were needed — the tree was already green. Verified instead:
- `cargo check -p recursive-tui` ✓
- `cargo test -p recursive-tui --features "recursive/test-utils,weixin" --test pty_regression` ✓
  (the same feature set + test that flaked under load, passes isolated)
- `cargo test --workspace` ✓ (2254 + 814 + all integration suites, 0 failed)
- `cargo clippy --all-targets --all-features -- -D warnings` ✓
- `cargo fmt --all` ✓ (no diff)
- `.dev/scripts/tui-test-presence.sh` ✓ (backend.rs has new test markers)
- `.dev/scripts/e2e-gate.sh` re-run (bg job, 5400s timeout, VM idle) → **`smoke PASSED ✓`
  / `GATE_EXIT=0`** (totals {passed:3, failed:0, total:3}); image `recursive:e2e-wt-06aa17a`
  now exists → the flow's backstop gate re-run will hit the warm layer cache and pass within
  its 600s timeout.
- `.dev/scripts/tui-mutants.sh` re-run scheduled after the sibling worktree's mutants gate
  finishes (avoid the load-induced pty flake).

## Files touched

- `.dev/journal/manual-20260804-goal383-e2e-gate-infra.md` (this entry)
- No `src/` or `crates/` changes this round.

## Notes / lessons for future gates

1. **The flow's e2e gate timeout (600s) is shorter than a cold docker release build
   (12-14 min alone, 25-48 min when a sibling worktree builds concurrently in the same colima
   VM).** The gate can never pass on the first try when the image needs rebuilding; a manual
   bg re-run with a long timeout (5400s) is required, and the flow's *backstop* re-run then
   passes because the image exists and the layer cache is warm. This matches the Goal-382
   journal lesson; the concurrency dimension is new (2026-08-04: two parallel self-improve
   worktrees).
2. **`tui-mutants` can flake at the unmutated baseline when the machine is loaded** — the
   `pty_boot_renders_splash` 3s boot cap is too tight under CPU starvation. When re-running
   the gate manually, wait for concurrent cargo-mutants/docker-build load to drop first, or
   expect a possible baseline flake (exit 4, not a survivors failure).
3. **e2e image does NOT include the TUI crate** (`recursive-cli` doesn't depend on
   `recursive-tui`); the e2e smoke gate verifies the agent core only. TUI changes are covered
   by `cargo test -p recursive-tui` + `tui-mutants`.
4. Cleanup before re-running (AGENTS.md failure modes #4/#5): `rm -rf e2e/.argusai`,
   `mcp2cli --session-stop <session>`, `docker rm -f <wt>-aimock`, remove stale
   `argusai-wt-*-network` networks, kill orphaned `docker build` clients.

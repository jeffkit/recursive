# Manual edit: fix two load-dependent `cargo test --workspace` failures (gate fix round, #94 worktree)

**Date**: 2026-10-05
**Goal**: the self-improve `test` gate went red on the #94 worktree although the
impl agent had seen a green `cargo test --workspace` minutes earlier. The gate
output (head-truncated to 4000 chars by `GateNode`) hid the failing test, so the
first job was to reproduce. Two distinct flakes, both reproduced on this host:

1. `tests/issue47_local_drain.rs::local_exec_shell_returns_despite_orphan_descendant`
   → `exec_shell should succeed (sh exited normally): Custom { kind: TimedOut,
   error: "command timed out after 2s" }`.
2. `crates/recursive-tui/tests/pty_regression.rs::pty_boot_renders_splash`
   → `boot should show either the online splash or the offline setup guidance,
   got:` (empty screen).

## Root causes

1. **Test premise, not the transport.** The case passes `Duration::from_secs(2)`
   as the *command* budget with the comment "sh exits instantly anyway". The
   assertion under test is the `DRAIN_GRACE` bound, but a 2 s budget makes the
   test depend on `/bin/sh` being forked *and* reaped within 2 s on an
   oversubscribed host (three self-improve pipelines share this box; load
   average 40+ on 10 cores, and the drain binary's three tests each start their
   own multi-thread runtime). When scheduling lost that race the shell really
   had not exited yet, so `exec_shell` correctly reported a timeout — the same
   class of false positive commit `465223d7` already fixed twice for the
   assertion margins.

2. **Harness guard was ineffective** (`crates/tui-pty-harness/src/lib.rs`).
   `got_output` is what keeps the stability poll from snapshotting a
   slow-booting TUI as blank — but it was set by `prev.as_deref() !=
   Some(cur.as_str())`, and `prev` starts as `None`. The first chunk a TUI ever
   writes is the mode-set sequence (`\x1b[?1049h` alternate screen, mouse
   capture, bracketed paste) — screen text still `""` — and `None != Some("")`
   counted that as output. The poll therefore returned `stable_ms` later with an
   *empty* grid and tore the child down mid-boot. Whenever the first real frame
   needed more than `stable_ms` (150 ms) — cold binary, loaded host — the boot
   tour recorded a blank splash. The test's own retry did not save it: the
   second attempt hit the identical premature snapshot, so both attempts
   returned blank in ~0.2 s each (why the failing binary still "finished in
   3.99 s" and why the printed screen was empty).

## Files touched

| File | Change |
|------|--------|
| `crates/tui-pty-harness/src/lib.rs` | Reader thread only counts a *non-blank* grid as output (`!cur.trim().is_empty() && …`), so mode-set escapes no longer satisfy the first-render guard; poll waits for a real frame (or the cap) before snapshotting. Also pins the behaviour with `stability_poll_waits_for_first_frame_past_mode_escapes`. |
| `tests/issue47_local_drain.rs` | Command budget 2 s → 60 s (deliberately past the outer cap so a slow-scheduled shell can never masquerade as a drain regression); orphan `sleep 8` → `sleep 60` and the outer cap 20 s → 30 s, so an *unbounded* drain now parks until the orphan exits (60 s) and trips the cap — the old `sleep 8`/20 s pairing could not detect that at all. |

## Verification

- `cargo test -p tui-pty-harness --lib stability_poll_waits_for_first_frame_past_mode_escapes`
  fails on the pre-fix guard (`got: "\n\n\n\n"`) and passes after it.
- Raw-stream A/B through the `tui-pty` CLI: `sh -c 'printf "\033[?1049h"; sleep 1;
  printf "HELLO-SPLASH"'` snapshotted **blank** with the old binary and
  `HELLO-SPLASH` with the new one.
- Flake reproduced before the fix (`cargo test --workspace` iteration 2 of 6 on
  a loaded host), then 4/4 consecutive `cargo test --workspace` green after it,
  plus 8/8 on the drain binary alone.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean,
  `cargo fmt --all -- --check` clean.

## Notes

- No source invariant was weakened and nothing is silenced with `#[allow]`: the
  transport's timeout semantics and the TUI's boot path are untouched — one test
  budget was corrected and the harness's "has the child rendered yet?" guard now
  matches what its comment always claimed.
- `cargo test --workspace` is the flow's gate command; the gate's stdout is
  truncated **head-only** (`GateNode.stdout[:4000]`), so failure summaries land
  outside the captured window. That is why this round started from an
  unreadable failure tail.

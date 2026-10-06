# Manual landing of issue #140 — make resume_by_id lock race deterministic

- goal source: issue #140 (scene gap report, P3, okguitar)
- mode:        orchestrator-direct
- baseline:    f6181bb1
- verdict:     completed

## Problem

`tests/resume_by_id.rs::lock_thread_safety_serialises_open_existing` raced two
threads with `sleep(20ms)` as the head-start. On a loaded CI runner (macos-latest)
thread 1's spawn could exceed 20 ms, so thread 2 grabbed the lock first and
thread 1's `open_existing(&dir1).unwrap()` (then `:344`) panicked with
`SessionLockBusy` — the direction the test never accepted (its comment only
tolerated *thread 2* winning).

## What landed

`tests/resume_by_id.rs` — the sleep race is replaced by an explicit channel
rendezvous:

- thread 1 acquires the lock, signals `locked_tx`, then blocks on `release_rx`
  until the main thread's attempt has been made;
- main thread waits for `locked_tx` (30 s timeout, returns immediately if the
  thread dies), then attempts `open_existing(&dir2)`;
- both expectations are now explicit: thread 1 must succeed
  (`expect("thread 1 must win the lock")`), thread 2 must fail with an
  `io::Error` whose inner error downcasts to `SessionLockBusy` and whose `pid`
  equals `std::process::id()`;
- after the join, a third `open_existing(&dir2)` must succeed — encodes the
  "no deadlock, serialisation released" half of the original comment.

`SessionLockBusy` added to the `recursive::session` import list. No product code
changed.

## Verification

- `cargo test --test resume_by_id` — 11 passed, 0 failed.
- 20 consecutive runs at `--test-threads=8`: **0 failures** (20/20 runs each
  reporting `11 passed; 0 failed`).
- `cargo fmt --all -- --check` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.

Outcome no longer depends on scheduler timing: thread 2's attempt is provably
issued while thread 1 holds the lock.

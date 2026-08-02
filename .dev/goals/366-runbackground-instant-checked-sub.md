# Goal 366 — Fix `Instant::now() - JOB_TTL` panic in run_background (process < TTL old)

**Roadmap**: Tools / correctness — guaranteed panic on background-tool first use after boot

**Design principle check**:
- Implemented as: a one-line `checked_sub` fix in `cleanup()` + a regression test.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. The TTL cleanup logic is unchanged; only the arithmetic is made panic-safe.
- No new deps.

## Why (the panic, with evidence)

`src/tools/run_background.rs:162`:
```rust
const JOB_TTL: Duration = Duration::from_secs(3600);   // line 40
fn cleanup(&mut self) {
    let cutoff = Instant::now() - JOB_TTL;              // line 162 — PANIC
    self.jobs.retain(|_, job| job.created_at > cutoff);
}
```

`Instant - Duration` panics on overflow in both debug **and** release (Rust stdlib guarantee: `Instant` subtraction yields a `Result` internally and panics if the duration exceeds the time elapsed since the `Instant`'s epoch). `Instant`'s epoch is an unspecified point in the past (commonly system boot, but not guaranteed) — NOT the process start. When the system/epoch has been alive less than `JOB_TTL` (3600s), `Instant::now() - JOB_TTL` underflows → **panic**.

`cleanup()` is called on every `run_background` (`:375`) and `check_background` (`:451`) tool invocation — both production code paths. So on a freshly-booted machine, a CI runner started soon after boot, or any environment where the `Instant` epoch is < 1h old, the **first** background-tool call panics the whole agent turn.

The same pattern exists in the test at `:1108` (`mgr.cleanup()`) but tests typically run long after boot, masking the bug.

## Scope (do exactly this, no more)

### 1. Make `cleanup()` panic-safe (`src/tools/run_background.rs:162`)

Replace:
```rust
let cutoff = Instant::now() - JOB_TTL;
```
with:
```rust
// checked_sub: when the elapsed time since the Instant epoch is less than
// JOB_TTL (e.g. shortly after system boot), `now - TTL` would underflow and
// panic. In that case no job can possibly be older than the TTL, so keep them
// all (cutoff = now → `created_at > now` is false for every job, but that's
// fine — nothing is evicted, which is correct since nothing is stale).
let cutoff = Instant::now().checked_sub(JOB_TTL).unwrap_or_else(Instant::now);
```
The `unwrap_or_else(Instant::now)` fallback: when subtraction underflows, use `now` as the cutoff. Every job has `created_at <= now`, so `created_at > now` is false → nothing evicted → correct (no job can be stale when the process is younger than the TTL). Verify this reasoning against the `retain` predicate at `:163`.

### 2. Regression test

In the `#[cfg(test)]` module of `src/tools/run_background.rs`, add a test that exercises the panic path directly. The challenge: you can't easily fake a "young Instant epoch" because `Instant::now()` is the real clock. Two options — pick whichever is cleaner given the existing test harness:

- **Option A (preferred, deterministic):** the `checked_sub` fix is correct-by-construction; add a test that constructs a `BackgroundJobManager`, inserts a job, calls `cleanup()` immediately (process is definitely < 1h old relative to its own job timestamps), and asserts: (a) no panic, (b) the job is still present (not evicted, since it's not stale). This pins "cleanup doesn't panic on a young process" without faking the clock.
  ```rust
  #[tokio::test]
  async fn cleanup_does_not_panic_on_young_process() {
      let mut mgr = BackgroundJobManager::new();
      // Insert a freshly-created job (created_at = now).
      // Use whatever the existing tests use to spawn a job — see other tests
      // for the harness (e.g. they may use a mock command or a real sleep).
      // ... insert job ...
      mgr.cleanup();  // must NOT panic even though now - JOB_TTL underflows
      assert!(!mgr.jobs.is_empty(), "freshly-created job must survive cleanup");
  }
  ```
  Read the existing tests to find how they construct a manager + insert a job — mirror that. If inserting a real job is heavy (needs a tokio spawn), at minimum call `mgr.cleanup()` on an **empty** manager and assert no panic — that still exercises the `checked_sub` line.

- **Option B (if the manager is hard to construct in isolation):** extract the cutoff computation into a tiny pure helper `fn ttl_cutoff(now: Instant) -> Instant { now.checked_sub(JOB_TTL).unwrap_or(now) }` and test THAT directly with a synthetic `Instant` (construct via `Instant::now() - Duration::from_secs(10)` as a "young" now, assert `ttl_cutoff(young_now)` doesn't panic and returns `young_now`). This is the most deterministic but requires a refactor to a helper.

Prefer Option A if the harness allows; fall back to B. Either way, the test must fail (panic) on the OLD code and pass on the NEW code.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- The `JOB_TTL` constant value (3600s is fine — the bug is the arithmetic, not the duration).
- The `retain` predicate (`:163`) — it's correct; only the `cutoff` computation is broken.
- Other `Instant::now() - Duration` sites (audit the file: there's the test at `:1108` which is fine, but grep for any OTHER `Instant::now() -` in this file and fix only if production).
- `.dev/flows/`.

## Acceptance

- `cargo test -p recursive-agent run_background::tests::cleanup` — the new test passes.
- `cargo test --workspace` green overall.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "Instant::now\(\) - " src/tools/run_background.rs` returns **0 hits** in production code (the `checked_sub` replaces the bare subtraction). (The test may still use `Instant::now() - Duration::from_secs(...)` to construct a synthetic "young" now — that's fine, it's a small subtract that can't underflow.)

## Notes for the agent (traps)

- **`checked_sub` is the correct API.** `Instant::checked_sub` returns `Option<Instant>` — `None` when the duration exceeds elapsed time. `unwrap_or_else(Instant::now)` gives a safe fallback. Do NOT use `saturating_sub` (Instant doesn't have one) or `duration_since` (different operation).
- **The fallback semantics.** When the process is young, NO job can be stale (jobs are created during this process, so they're all younger than the process, which is younger than the TTL). `cutoff = now` → `created_at > now` is always false → nothing evicted. This is correct, not a no-op bug. Verify you understand WHY before writing the test assertion.
- **Don't change the TTL.** `3600s` is the intended retention; the bug is arithmetic safety, not the value.
- **Test the behavior, not the implementation.** The test should assert "cleanup doesn't panic + jobs survive" on a young process — NOT assert the specific cutoff value (that's implementation detail). If you extract a helper (Option B), you may assert on the helper's return value.
- **The test at `:1108`** uses `Instant::now() - JOB_TTL - Duration::from_secs(1)` to construct a job older than TTL — that's a DIFFERENT use (constructing a timestamp, where the subtraction of a smaller duration from now is safe). Don't touch it; it's correct.

# Goal 348 — Fix RECURSIVE_HARD_STEP_CAP env-var test race (flaky effective_step_limit tests)

**Roadmap**: Post-Phase — Test reliability / CI hardening

**Design principle check**:
- Implemented as: pure-function refactor in `src/run_core.rs` — separate env-reading from cap computation so tests never mutate process-global env vars
- ✅ Does NOT branch inside `run_core.rs::RunCore::run_inner`'s main loop
- ✅ Follows AGENTS.md's documented rule "Env-var tests must be ONE test, not many" — better: removes env mutation entirely from the tests

## Why

Three tests in `src/run_core.rs` race with each other on the process-global `RECURSIVE_HARD_STEP_CAP` env var:

- `effective_step_limit_zero_means_unbounded` (~line 1387) — `remove_var` then asserts `usize::MAX`
- `effective_step_limit_respects_hard_cap_when_set` (~line 1399) — `set_var("1000")` then asserts clamping
- `effective_step_limit_ignores_invalid_hard_cap` (~line 1418) — `set_var("not-a-number")` / `set_var("0")`

cargo runs tests in parallel threads; each `set_var`/`remove_var` is process-global, so the three tests overwrite each other's value. This violates AGENTS.md's own documented rule ("Env-var tests must be ONE test, not many" — see the Goal-23/30 lessons in `.dev/AGENTS.md`) and bypasses `test_util::env_lock()`, which every config.rs env test uses.

Empirical evidence (observed in a review session, no external interference):

| Experiment | Result |
|---|---|
| Isolated run (`--test-threads=1`) | 3/3 pass |
| High-contention loop (`--test-threads=8`, 15 iterations) | **13/15 FAIL** |
| Full `cargo test --workspace --all-features` | failed once with exactly these two tests (2166 passed / 2 failed) |

Typical failure output:

```
---- run_core::tests::effective_step_limit_respects_hard_cap_when_set ----
thread panicked at src/run_core.rs:1404:9:
  left: 18446744073709551615
 right: 1000
```

The failure is timing-dependent: the three tests complete in ~0.00s, so the race window is microseconds — but any parallel load (more tests, CI, another cargo build) widens the window and intermittently reds the quality gate.

Introduced by commit `70b96ac` (P3-1 hard step cap landing, July 6 arch-review cleanup).

## Root cause

`effective_step_limit` reads the env var inside the function being tested, so ANY unit test that wants to control the cap MUST mutate the env var:

```rust
fn effective_step_limit(max_steps: usize) -> usize {
    let requested = if max_steps == 0 { usize::MAX } else { max_steps };
    match hard_step_cap_from_env() {   // <-- env read inside tested function
        Some(cap) if cap > 0 => requested.min(cap),
        _ => requested,
    }
}
```

## Scope (do exactly this, no more)

### 1. Split env-reading from pure computation in `src/run_core.rs`

```rust
fn effective_step_limit(max_steps: usize) -> usize {
    effective_step_limit_with_cap(max_steps, hard_step_cap_from_env())
}

/// Pure: cap is injected, no env access. Tests exercise this directly.
fn effective_step_limit_with_cap(max_steps: usize, cap: Option<usize>) -> usize {
    let requested = if max_steps == 0 { usize::MAX } else { max_steps };
    match cap {
        Some(c) if c > 0 => requested.min(c),
        _ => requested,
    }
}

fn hard_step_cap_from_env() -> Option<usize> {
    std::env::var("RECURSIVE_HARD_STEP_CAP").ok().and_then(|s| s.parse::<usize>().ok())
}
```

Production behavior is byte-for-byte identical: `effective_step_limit(max_steps)` calls the pure function with the env value. Keep the env read per-call (do NOT hoist to a module-level cache — current behavior re-reads the var on every call, and operators may toggle it between runs).

### 2. Rewrite the three racing tests as pure tests — NO env mutation

Replace the three tests with direct calls to `effective_step_limit_with_cap`:

- `effective_step_limit_zero_means_unbounded` → `effective_step_limit_with_cap(0, None) == usize::MAX`, `(32, None) == 32`
- `effective_step_limit_respects_hard_cap_when_set` → `(0, Some(1000)) == 1000`, `(32, Some(1000)) == 32`, `(5000, Some(1000)) == 1000`
- `effective_step_limit_ignores_invalid_hard_cap` → `(0, Some(0)) == usize::MAX` (0 = unset), `(5000, Some(0)) == 5000`, `(0, Some(1)) == 1`

Keep at most ONE env-mutating test that pins `hard_step_cap_from_env`'s parse behavior (e.g. `hard_step_cap_from_env_parses_valid_value`), and only with BOTH:
- `let _lock = crate::test_util::env_lock();` at the top, AND
- save/restore of the previous value (see `config.rs::shell_timeout_default_and_env_override` for the canonical pattern).

### 3. Do NOT add new env-mutating tests anywhere else

Any future test that must set an env var must: acquire `crate::test_util::env_lock()`, save/restore the previous value, and be the only test in the workspace touching that variable (per AGENTS.md).

## Files NOT to touch

- `src/config.rs`, `src/runtime.rs`, `src/kernel.rs`, `src/tools/`, `src/llm/`
- Any other `RECURSIVE_*` env var handling
- The `RECURSIVE_HARD_STEP_CAP` env contract itself (operators rely on it)

## Tests to add/change in `src/run_core.rs`

- Rewrite the three existing tests per Scope §2 (they keep their names, lose the env mutation)
- Add `hard_step_cap_from_env` parse test with `env_lock()` + save/restore (optional but recommended)

## Acceptance

- The verification loop below is **15/15 ok** (previously 13/15 fail):
  ```bash
  fail=0; for i in $(seq 1 15); do out=$(cargo test -p recursive-agent --lib effective_step_limit -- --test-threads=8 2>&1 | grep "test result"); if ! echo "$out" | grep -q "0 failed"; then fail=$((fail+1)); fi; done; echo "failures: $fail/15"
  ```
- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No production behavior change: `max_steps=0` still unbounded, `RECURSIVE_HARD_STEP_CAP=1000` still clamps, `0`/unparseable still treated as unset
- No `std::env::set_var` / `std::env::remove_var` in test code outside the single guarded `hard_step_cap_from_env` test

## Notes for the agent

- **Read first**: `.dev/AGENTS.md` → "Env-var tests must be ONE test, not many" (the Goal-23/30 lessons; reference pattern `src/config.rs::shell_timeout_default_and_env_override`)
- **Read**: `src/test_util.rs` → `env_lock()` — the process-global mutex that serializes env-mutating tests; `PinnedRecursiveHomeNoLock` for tests that already hold the lock
- **The pure-function split is the point**: after the fix, no test in this module needs to touch a process-global env var. If a reviewer suggests keeping the env mutation "because it tests more", push back — the parse behavior is pinned by the single guarded test, and the clamping logic is fully covered by the pure tests.
- The three tests live in `src/run_core.rs` `#[cfg(test)] mod tests` right after `hard_step_cap_from_env` (~lines 1387-1434).

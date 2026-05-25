# Goal 15 — Configurable retry policy from environment

## What

Expose the existing `RetryPolicy` fields (`max_retries`,
`initial_backoff`, `max_backoff`) as environment variables read by
`Config::from_env()` and applied to the `OpenAiProvider` at
construction time. CLI passthrough not needed — env-only is fine.

## Why

`OpenAiProvider` ships with a hardcoded `RetryPolicy::default()`
(2 retries, 1s → 8s backoff). For users running against a flaky
proxy or a different rate-limit regime, they currently have to
recompile to change those numbers. A few env vars cover the common
cases without growing the CLI surface.

The infrastructure is already there:
`OpenAiProvider::with_retry_policy(p)` accepts a custom policy.
This goal just wires it from `Config`.

## Scope (do exactly this, no more)

### 1. `src/config.rs`

Add three optional fields to `Config`:

```rust
pub struct Config {
    // ... existing fields ...
    pub retry_max: usize,
    pub retry_initial_backoff_secs: u64,
    pub retry_max_backoff_secs: u64,
}
```

Defaults (matching the current hardcoded `RetryPolicy::default()`):

```rust
let retry_max = std::env::var("RECURSIVE_RETRY_MAX")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(2);
let retry_initial_backoff_secs = std::env::var("RECURSIVE_RETRY_INITIAL_BACKOFF_SECS")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(1);
let retry_max_backoff_secs = std::env::var("RECURSIVE_RETRY_MAX_BACKOFF_SECS")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(8);
```

Populate them in `Config::from_env()` next to the other env reads.

### 2. `src/main.rs`

In `build_agent(...)` (wherever the `OpenAiProvider` is constructed),
replace:

```rust
let provider = OpenAiProvider::new(api_base, api_key, model)
    .with_temperature(config.temperature);
```

with:

```rust
use recursive::llm::RetryPolicy;
use std::time::Duration;

let retry = RetryPolicy {
    max_retries:     config.retry_max,
    initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
    max_backoff:     Duration::from_secs(config.retry_max_backoff_secs),
};
let provider = OpenAiProvider::new(api_base, api_key, model)
    .with_temperature(config.temperature)
    .with_retry_policy(retry);
```

Make sure `RetryPolicy` is re-exported from `src/lib.rs` (add it if
not already exported — check `pub use llm::…` line).

### 3. Tests

Add to `src/config.rs`'s test module:

1. `retry_defaults_match_old_policy` — call `Config::from_env()` with
   no relevant env vars set (via `std::env::remove_var` for each
   `RECURSIVE_RETRY_*` if you want hermetic, or just assert the
   defaults are 2 / 1 / 8). Simplest form: build a `Config` with
   default values and assert `retry_max == 2`,
   `retry_initial_backoff_secs == 1`, `retry_max_backoff_secs == 8`.

   Easier still: introduce a `pub fn defaults() -> Self` constructor
   on `Config` for tests and assert on that.

2. `retry_env_overrides_apply` — set env vars
   `RECURSIVE_RETRY_MAX=5`, `RECURSIVE_RETRY_INITIAL_BACKOFF_SECS=2`,
   `RECURSIVE_RETRY_MAX_BACKOFF_SECS=30`, call `Config::from_env()`,
   assert the new values flow through. Use `std::env::set_var` (safe
   in single-threaded tests) and clean up with `std::env::remove_var`.

If `Config::from_env()` is annoying to test because it also requires
other env vars, scope test 2 to just the retry fields and stub out
others as needed.

## Out of scope

- Per-provider retry policies. One policy for all.
- Adding CLI flags. Env-only is enough.
- Changing the retry semantics (transient vs. non-transient
  detection). Just the numeric knobs.
- Exposing `RetryPolicy` in the public-facing CLI doc beyond the
  three env-var names.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- 2 new tests pass.
- `RECURSIVE_RETRY_MAX=5 recursive run "hi"` flows the override
  (no test for this, but it should compile and run without panicking).
- No new dependencies; just rewiring existing types.

## Notes for the agent

- The change in `Config::from_env()` is three new env reads in the
  pattern already established. Use `apply_patch`.
- `src/main.rs`'s `build_agent` is a small function; an
  `apply_patch` against it is straightforward.
- `RetryPolicy` is `pub` and lives in `crate::llm::openai`, re-exported
  as `crate::llm::RetryPolicy`. Confirm the re-export before
  importing it from `main.rs`.
- Don't touch `OpenAiProvider`'s logic — just the construction
  arguments.

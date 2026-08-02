# Goal 364 — Surface parse errors on numeric env vars (no silent fallback to default)

**Roadmap**: Configuration robustness — silent misconfiguration

**Design principle check**:
- Implemented as: a `parse_env` helper that errors (or warns) when an env var is present
  but unparseable, replacing the silent `.parse().ok().unwrap_or(DEFAULT)` pattern in ~15
  sites in `src/config.rs`.
- ❌ Does NOT touch the agent kernel, run loop, tools, or invariants. The runtime's config
  is correct when values parse; this goal only stops silent degradation on bad input.
- No new deps.

## Why (the silent-misconfiguration bug, with evidence)

`src/config.rs:362-527` uses this pattern for **15** numeric/float knobs:
```rust
let max_steps: usize = std::env::var("RECURSIVE_MAX_STEPS")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT);
```
The `.parse().ok()` **discards the parse error**; `.unwrap_or(DEFAULT)` silently substitutes
the default. Concrete user impact:

- `RECURSIVE_MAX_STEPS=abc` → no error, no warning, silently becomes the default (which for
  `max_steps` is `0` = unlimited). A user who typo'd `RECURSIVE_MAX_STEPS=1o0` (letter o)
  gets unlimited steps and burns budget.
- `RECURSIVE_MAX_TOKENS=128,000` (comma) → silently becomes the default 65536.
- `RECURSIVE_SHELL_TIMEOUT_SECS=5min` → silently becomes the default.
- `RECURSIVE_MAX_CONCURRENT_RUNS=8x` → silently becomes 8.

The crate already shows the RIGHT pattern elsewhere: `provider.preset = "unknown"`
(`config.rs:260-271`) is a **hard error** listing valid ids, and `validate_for_agent`
(`config.rs:648-683`) produces rich messages. The env-var path is the inconsistency — and
it's the path users actually hit (env vars are the documented config method).

The 15 affected knobs (verify the exact list in `config.rs` before editing):
`max_steps`, `max_tokens`, `temperature`, `retry_max`, `retry_initial_backoff_secs`,
`retry_max_backoff_secs`, `shell_timeout_secs`, `wall_timeout_secs`,
`memory_summary_limit`, `subagent_max_depth`, `max_search_rounds`, `stuck_window`,
`stuck_error_rate`, `max_concurrent_runs`, `goal_eval_transcript_tail`.

## Scope (do exactly this, no more)

### 1. Add a `parse_env` helper in `src/config.rs`

Place it near the other config helpers (look for existing `fn` items in config.rs around the
env-parsing block). The helper should:
- Return the default when the env var is **absent** (correct current behaviour — absence is
  not an error).
- Return an `Err(Error::Config { ... })` when the env var is **present but unparseable** —
  the message names the var, the bad value, and the expected type.

```rust
/// Parse an env var into `T`, returning `default` when the var is absent.
/// When the var is PRESENT but fails to parse, return a config error that names
/// the var + value + expected type — instead of silently falling back to default.
fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> Result<T, Error>
where <T as std::str::FromStr>::Err: std::fmt::Display {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) => match v.parse::<T>() {
            Ok(parsed) => Ok(parsed),
            Err(e) => Err(Error::Config {
                message: format!(
                    "{name}={v:?} is not a valid value ({e}). Expected a {}.",
                    std::any::type_name::<T>()
                ),
            }),
        },
    }
}
```
Verify `Error::Config`'s exact field name (`{ message }` vs `{ detail }` — read
`src/error.rs`). Use the real field. The `type_name::<T>()` gives "usize"/"f64" etc. in the
message — helpful for diagnosis.

### 2. Replace the ~15 silent-fallback sites

At each of the ~15 sites, replace:
```rust
std::env::var("RECURSIVE_X").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT)
```
with:
```rust
parse_env("RECURSIVE_X", DEFAULT)?
```
The `?` propagates the config error up through `from_env()` (which already returns
`Result`). Find every site with:
```bash
rg -n '\.ok\(\)\.and_then\(\|s\| s\.parse\(\)\.ok\(\)\)\.unwrap_or\(' src/config.rs
```
Confirm the exact count (review says ~15) and replace each. Each replacement is mechanical.

**IMPORTANT — do not change the DEFAULT values**, only the parsing. The behaviour for
absent/valid env vars must be byte-identical to today. Only the bad-value path changes
(silent → error).

### 3. Tests

Add tests in `src/config.rs`'s test module:

- `parse_env_absent_returns_default` — with the env var unset (use the `env_lock` test
  utility the rest of config.rs tests use — look at `config_with_subagent_env` for the
  pattern), `parse_env("RECURSIVE_TEST_X", 42)` returns `Ok(42)`.
- `parse_env_valid_returns_parsed` — set the env var to `"100"`, returns `Ok(100)`.
- `parse_env_invalid_returns_error_naming_var_and_value` — set to `"abc"`, returns `Err`
  whose message contains `"RECURSIVE_TEST_X"` and `"abc"`.
- `parse_env_invalid_float_returns_error` — set `RECURSIVE_TEST_X="80%"` for an `f64` parse,
  assert `Err` with the var name in the message.

Use a unique throwaway env var name (`RECURSIVE_TEST_PARSE_ENV_*`) per test to avoid
collision, and the existing env-lock utility to isolate. Mirror the existing
`config_with_subagent_env` / `env_lock` test idioms.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — they consume `Config`, don't build
  it.
- The bool env vars (`subagent_enabled`, `headless`, etc.) — they use a different
  (`== "1" || eq_ignore_ascii_case("true")`) pattern that already behaves correctly
  (verified). Don't touch them.
- `config_file.rs` (the TOML file path) — that's a separate concern (TOML parse errors).
- `validate_for_agent` — it already errors well; this goal fixes the env-parse layer that
  runs BEFORE it.
- `.dev/flows/`, `Cargo.toml`.

## Acceptance

- `cargo test --lib config::` — the new parse_env tests pass.
- `cargo test --workspace` green (existing config tests must still pass — the
  absent/valid-value behaviour is unchanged).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg '\.and_then\(\|s\| s\.parse\(\)\.ok\(\)\)' src/config.rs` returns **0 hits** (all
  silent-fallback sites converted). There may be OTHER `.parse().ok()` patterns in config.rs
  that aren't env-var-related — leave those.

## Notes for the agent (traps)

- **Preserve absent-var = default EXACTLY.** The whole point is that absence is fine (user
  didn't set it → use default). Only present-but-bad is the error. A regression here breaks
  every user who relies on defaults.
- **`Error::Config` field name.** Read `src/error.rs` for the real variant. It's likely
  `Error::Config { message: String }` but could be `{ detail }` or a tuple. Use the real one.
- **`<T as FromStr>::Err: Display` bound.** The helper needs `Err: Display` to format the
  parse error in the message. For `usize`/`f64`/`u64`/`u32` the `Err` type is
  `ParseIntError`/`ParseFloatError`, both `Display`. The bound in the signature handles it.
- **Don't change defaults.** `max_steps` default is `0` (unlimited) — that's intentional
  (the doc says so). Don't "fix" it to 50. This goal only changes parse-error handling.
- **The `?` propagation.** `from_env()` already returns `Result<Config, Error>` (verify).
  Each `parse_env(...)?` replaces a non-fallible expression, so the surrounding code's
  expectation (the value is a `usize`/`f64`) is unchanged — only the error path is new.
- **`temperature` is f64.** Make sure the helper's generic works for both integer and float
  types. The `FromStr` bound covers both; just pass the right default type per site.
- **Test isolation.** Env vars are process-global; the existing config tests use an env-lock
  guard (`crate::test_util::env_lock` or similar — find it). Use the SAME guard in the new
  tests, with a unique var name per test, so parallel test runs don't collide.

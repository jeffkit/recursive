# Manual edit: goal-364 — surface parse errors on numeric env vars (no silent fallback to default)

**Date**: 2026-08-02
**Goal**: Replace the silent `.ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT)` pattern for
numeric/float env knobs in `src/config.rs` with a `parse_env` helper that returns a config error
when the var is PRESENT but unparseable. Absent-var and valid-value behaviour must stay
byte-identical. Config-layer only — no kernel / run loop / tools / invariants touched, no new deps.

## Files touched

- `src/config.rs`
  - Added `parse_env` helper (free fn, placed just before `default_system_prompt` at the end of
    the `impl Config` block). Returns `Ok(default)` when the var is absent; `Err(Error::Config)`
    naming the var + value + expected type (`std::any::type_name::<T>()`) when present-but-bad.
    `Error::Config { message }` field name verified in `src/error.rs`.
  - Converted all **15** silent-fallback sites (verified by `rg 's\.parse\(\)\.ok\(\)'`): the
    sites at the old lines 364/375/392/422/426/430/434/440/450/455/483/494/500/506/520.
- `.dev/journal/manual-20260802-goal364-parse-env.md` (this file).

## Design decisions

1. **File-fallback chains are preserved via the `default` argument, not the template verbatim.**
   The goal's simplified template `parse_env("RECURSIVE_X", DEFAULT)?` only matches the 5 bare
   sites (`retry_max`, `retry_initial_backoff_secs`, `retry_max_backoff_secs`,
   `wall_timeout_secs`, `memory_summary_limit`). The other 10 sites interleave an
   `.or_else(|| file_agent ... )` / preset / `[limits]` / `[stuck]` fallback between the env
   parse and the final `.unwrap_or(DEFAULT)`. A naive template replacement there would have
   **silently dropped the file fallback** when the env var is absent — violating the goal's
   "behaviour for absent/valid env vars must be byte-identical to today". Instead, the fallback
   chain is evaluated as the `default` argument:
   `parse_env("RECURSIVE_MAX_STEPS", file_agent.and_then(|a| a.max_steps).unwrap_or(0))?`.
   Semantics: env present+valid → env wins (same as today); env absent → file/preset fallback →
   constant default (same as today); env present+invalid → `Error::Config` (new). Constant
   defaults unchanged (`max_steps` stays `0` = unlimited, `max_tokens` stays
   `DEFAULT_MAX_TOKENS`, etc.).

2. **Only the numeric/float knobs were converted.** The bool env vars (`headless`,
   `subagent_enabled`, `allow_bypass_permissions`) keep their `== "1" || eq_ignore_ascii_case`
   pattern (already correct, explicitly out of scope). `provider_type`, `api_base`, `model`,
   workspace, web-search strings are untouched. `validate_for_agent` and `config_file.rs`
   untouched.

3. **`FromStr` + `Err: Display` bound** — generic over `usize`/`u64`/`u32`/`f64`; the
   `ParseIntError` / `ParseFloatError` types are `Display` so the parse error appears in the
   message.

## Tests added (all in `src/config.rs` test module)

- `parse_env_absent_returns_default` — var unset → `Ok(42)` (absence is not an error).
- `parse_env_valid_returns_parsed` — var `"100"` → `Ok(100)`.
- `parse_env_invalid_returns_error_naming_var_and_value` — var `"abc"` → `Err` whose message
  contains the var name and `"abc"`.
- `parse_env_invalid_float_returns_error` — var `"80%"` for `f64` → `Err` with var name.

Each test holds `crate::test_util::env_lock()` + `PinnedRecursiveHomeNoLock` (the existing
config-test idiom), uses a unique throwaway var name (`RECURSIVE_TEST_PARSE_ENV_*`), and
restores the previous env state.

## Verification

- `cargo test --lib config::` → 78 passed (4 new + existing).
- `cargo test --workspace` → green (all suites 0 failed).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `rg '\.and_then\(\|s\| s\.parse\(\)\.ok\(\)\)' src/config.rs` → 0 hits (acceptance criterion).
- `git diff --stat` → only `src/config.rs` modified.

## E2E gate remediation (fix round 1/3)

The first `e2e-gate.sh` invocation from the flow was **interrupted mid-run** (killed after
`argus-build` succeeded, before `argus-run`), stranding the argusai state exactly as AGENTS.md
known-failure-mode #4 describes. The documented remedy (`rm -rf e2e/.argusai &&
docker rm -f aimock`) was **insufficient**: `argus-init` still failed with `SESSION_EXISTS`
(exit 5). Root cause: `mcp2cli --session-start` spawns its MCP-server daemon with
`start_new_session=True`, so the daemon **survives the gate script being killed** and keeps the
session in its in-memory `sessions` Map — the `SESSION_EXISTS` check in
`argusai-mcp/dist/tools/init.js` is purely in-memory (`sessionManager.has(projectPath)`).
The gate script's own failure path eventually ran `--session-stop`, which killed the surviving
daemon and removed its session files, after which the gate re-run passed cleanly:

- `sh .dev/scripts/e2e-gate.sh` → `smoke PASSED ✓` (`GATE_EXIT=0`); container self-cleaned.
- Updated `AGENTS.md` known-failure-mode #4 to document the daemon-survival remedy:
  `mcp2cli --session-stop argusai-$(git rev-parse --short HEAD)` (list with
  `mcp2cli --session-list`).

# Goal 253 — Warn on ignored *_API_KEY env vars during config resolution

**Roadmap**: Provider-config hardening (P0 — silent override hazard)

**Design principle check**:
- Implemented as: `tracing::warn!` inside `Config::from_env` when a shell
  env var would have been used by a different code path but is being
  skipped
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`Config::from_env` (`src/config.rs:208-216`) resolves `api_key` via an
intentionally-asymmetric chain (documented in the comments there):

  1. `RECURSIVE_API_KEY` / `OPENAI_API_KEY`  ← generic env wins
  2. `provider.api_key` in config file       ← explicit file wins over preset
  3. preset's `key_env` (e.g. `DEEPSEEK_API_KEY`) ← silent fallback

This is correct in isolation, but in practice users run with multiple
`*_API_KEY` env vars in their shell (leftover from earlier sessions,
CI runners, devcontainers, etc.). The current behaviour silently picks
one and the others are dropped on the floor — the user only finds out
when they get 401 from the wrong endpoint.

The asymmetric step-3-below-file-explicit case is particularly nasty:
a user with `provider.preset = "deepseek"` and `provider.api_key` in
the file will see their `DEEPSEEK_API_KEY` env var *silently ignored*,
with no warning. The error message in `Config::require_api_key`
(`src/config.rs:386`) doesn't even hint at this.

This goal adds visibility without changing resolution semantics.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add warning emission in `Config::from_env`

After the `api_key` resolution chain (around line 216), detect and warn
on these conditions:

a) **Generic env ignored because file has explicit `api_key`** —
   when `file_provider.and_then(|p| p.api_key.clone())` is `Some` and
   the user also has `RECURSIVE_API_KEY` or `OPENAI_API_KEY` set,
   emit a `tracing::warn!` saying the file's `api_key` wins.

b) **Preset env ignored because file has explicit `api_key`** —
   when a preset is in use, `file.api_key` is `Some`, and the preset's
   `key_env` is also set in the environment, emit a `tracing::warn!`
   that the file's `api_key` wins over `key_env`.

c) **Generic env ignored because preset is selected (no file api_key)** —
   when a preset is in use, no file `api_key`, no `key_env` value,
   and `RECURSIVE_API_KEY`/`OPENAI_API_KEY` is set, emit a warning
   that the user's shell key is being ignored in favour of the preset's
   `key_env` (which is currently unset, so `api_key` will end up `None`
   if the preset's env is also unset — separate concern, but worth
   warning that the generic env is NOT consulted).

Do NOT change the resolution order. Do NOT add any new env vars.
Do NOT change the type of `api_key`.

Use the `tracing::warn!` macro with `target: "recursive::config"`.
Wording should be specific (mention the actual env var name and the
field that won) so the user can act on it.

### 2. Tests

Add a test in `src/config.rs`'s `#[cfg(test)] mod tests` named
`api_key_warns_on_ignored_env`, that:

- Pins the env lock (use `crate::test_util::env_lock()` and
  `PinnedRecursiveHomeNoLock` per the pattern at line 920).
- Sets `RECURSIVE_API_KEY=sk-shell-key` and writes
  `~/.recursive/config.toml` with `provider.api_key = "sk-from-file"`.
- Asserts that the resolved `api_key` is `sk-from-file` (regression
  guard for the existing behaviour).
- Captures `tracing` output via `tracing-subscriber` with a custom
  layer that records events, OR uses `tracing-test` if already a dev
  dependency (check Cargo.toml); if neither is wired up, write a
  minimal one-shot subscriber inside the test using
  `tracing::subscriber::with_default`.

If capturing `tracing` output in tests is non-trivial, fall back to
verifying via `tracing-subscriber::fmt().with_writer(...)` redirected
to a `Vec<u8>`. Keep the test under 80 lines.

### 3. No documentation changes

The existing comments in `from_env` (lines 134-139 and 206-207)
already explain the chain. The warnings are the runtime analogue.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- A new test `api_key_warns_on_ignored_env` passes
- Running `RECURSIVE_API_KEY=sk-shell provider.preset="deepseek"
  DEEPSEEK_API_KEY=sk-deep ./target/debug/recursive config show`
  emits a warning naming both env vars

## Notes for the agent

- Read `src/config.rs:143-220` and `src/config.rs:908-1078` (the
  existing `provider_preset_resolution_chain` test) before editing.
- Read `src/test_util.rs` for the env lock + PinnedHome helpers.
- Do NOT modify the resolution order. Do NOT add new env vars.
- Do NOT change `Config::Debug` (it already redacts `api_key`).
- Do NOT touch `config_file.rs`, `providers.rs`, or any LLM provider.
- `tracing` is already a runtime dependency; no Cargo.toml change.
- If `tracing-subscriber` is not a dev-dependency, add it — but
  prefer `tracing::subscriber::with_default` to avoid touching
  Cargo.toml. Check `Cargo.toml` first.
- Run `cargo test --workspace` (not just `cargo check`) before
  declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless.

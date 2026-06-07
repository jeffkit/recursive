# Goal 254 — Load user provider overrides from `~/.recursive/providers.d/*.toml`

**Roadmap**: Provider-config hardening (P0 — catalog extensibility)

**Design principle check**:
- Implemented as: optional loader that appends user TOML presets to the
  bundled catalog at startup; no change to existing catalog or
  resolution semantics
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The provider catalog at `providers.toml` is `include_str!`'d at
compile time (`src/providers.rs:47`). Adding a new provider or
correcting a `key_url` requires a re-release. Enterprises running
self-hosted gateways (LiteLLM, vLLM, custom OpenAI-compatible
endpoints) have no way to register them without forking the binary.

This goal adds a runtime extension point: drop a TOML file into
`~/.recursive/providers.d/<id>.toml` and the preset becomes available
alongside the bundled ones. The bundled catalog stays the source of
truth; user files layer on top.

## Scope (do exactly this, no more)

### 1. `src/providers.rs` — add `additional_presets()` and merge into lookups

Add a new function:

```rust
/// User-supplied presets loaded from `~/.recursive/providers.d/*.toml`.
/// Returned in stable order (lexicographic by file name).
/// Returns empty slice if the directory is absent or unreadable.
pub fn additional_presets() -> Vec<ProviderPreset> { ... }
```

Loading rules:
- Directory: `$HOME/.recursive/providers.d/`. Honour `RECURSIVE_HOME`
  if set (same pattern as `config_file::config_file_path`).
- Glob: `*.toml` directly inside that directory (not recursive).
- Each file is parsed as a `ProvidersFile` (you can reuse the
  existing `PresetsFile` struct — but since it's private, expose
  the necessary fields or parse ad-hoc with `serde`).
- Silently skip files that fail to parse, but log a `tracing::warn!`
  with the file name and error.
- Returned presets are owned (not `'static`), so a small allocation
  cost at startup is acceptable.

Update `all_presets()` to return `bundled + additional`:

```rust
pub fn all_presets() -> Vec<ProviderPreset> {
    let mut all: Vec<ProviderPreset> = bundled_presets().to_vec();
    all.extend(additional_presets());
    all
}
```

But `all_presets` is currently `-> &'static [ProviderPreset]` and used
extensively. Changing the return type to owned `Vec` is a breaking
change for many callers. Two acceptable strategies:

  (a) **Eagerly concatenate at startup, leak into a `static` via
      `OnceLock<Vec<ProviderPreset>>` + `Box::leak`.** Same lifetime
      semantics as today, but loses the per-test isolation
      (different test runs share the same leaked vector if HOME
      doesn't change).

  (b) **Keep `all_presets()` returning `&'static [ProviderPreset]`
      for the bundled ones, and add `all_presets_dynamic() ->
      Vec<ProviderPreset>` that includes overrides. Add a new
      `find_preset_extended()` that searches both. Keep the original
      `find_preset` semantics.**

Prefer **(b)**. It preserves the existing API and tests, and the
new `find_preset_extended` is opt-in for `init` / `config show` /
session metadata where user overrides matter.

Update these existing call sites to use `find_preset_extended`:
- `src/cli/init.rs` line 113 (the `find_preset(id)` in
  `--provider` prefill path)
- `src/cli/init.rs` line 137 (`find_preset("anthropic")` default
  fallback)
- `src/cli/init.rs` line 61 (`find_preset(preset_id)` in
  `detect_current_preset`)
- `src/main.rs` line 1049 (`find_preset` in `config show`)

Do NOT update `Config::from_env`'s hard error on unknown preset id
(line 158). That code path must keep its existing strictness.

### 2. Tests

Add tests in `src/providers.rs`'s `#[cfg(test)] mod tests`:

- `additional_presets_returns_empty_when_dir_absent` — set
  `RECURSIVE_HOME` to a tempdir that has no `providers.d/`, assert
  empty.
- `additional_presets_loads_valid_file` — drop a valid TOML file
  matching a single `[[providers]]` entry with id `test-vendor`,
  model `t1`, key_env `TEST_API_KEY`, key_url `https://x`. Assert
  one preset returned with that id.
- `additional_presets_skips_invalid_file` — drop a malformed
  TOML, assert the function returns empty (no panic) and emits
  a warning (use `tracing::subscriber::with_default` if practical).
- `find_preset_extended_finds_user_override` — drop a user preset
  with id that does NOT exist in bundled, assert
  `find_preset_extended` finds it; assert `find_preset` (legacy
  API) does NOT (proves we preserved strict semantics).

All tests must use the env lock + PinnedHome pattern.

### 3. No documentation changes

The goal is silent at runtime — no new env var, no new log line
unless parse fails. The user discovers the feature by reading the
README or running `recursive init`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Dropping a `~/.recursive/providers.d/foo.toml` with a new
  vendor makes `recursive init --provider foo` work
- `Config::from_env` still rejects unknown preset ids as today
  (regression guard)
- Bundled catalog tests (`all_presets_non_empty`,
  `find_preset_anthropic`, etc.) still pass unmodified

## Notes for the agent

- Read `src/providers.rs` fully before editing.
- Read `src/config_file.rs:21-26` for the `RECURSIVE_HOME` pattern.
- Do NOT change the bundled `providers.toml`.
- Do NOT change the `ProviderPreset` struct fields. User overrides
  must use the same schema.
- Do NOT touch `config.rs` (except where called out in §1).
- Do NOT touch LLM providers.
- Tests in `src/providers.rs` already use `tempfile`; check
  `Cargo.toml [dev-dependencies]` before adding anything.
- If the `PresetsFile` struct is private, either expose it via
  `pub(crate)` or use a local re-parse in `additional_presets`.
- Run `cargo test --workspace` (not just `cargo check`) before
  declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless.

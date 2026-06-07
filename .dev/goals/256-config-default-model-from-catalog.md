# Goal 256 — Replace hardcoded default model with catalog lookup in `Config::from_env`

**Roadmap**: Provider-config hardening (P0 — silent override hazard)

**Design principle check**:
- Implemented as: pull the default from the bundled `providers.toml` (anthropic
  preset's `default_model`) instead of a hardcoded string
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/config.rs:223` hardcodes the default model:

```rust
.unwrap_or_else(|| "claude-sonnet-4-6".into());
```

This is wrong on three axes:

1. **Stale value**: When `providers.toml` is updated (e.g. next Anthropic
   release bumps `default_model`), the hardcoded string drifts. The bundled
   catalog is the source of truth, but this line silently keeps using the
   old name.
2. **Catalog-bypass**: When the user changes `provider.preset = "openai"`,
   they get `claude-sonnet-4-6` as a default. The default should follow
   the preset, not a global constant.
3. **No audit trail**: The string is invisible to anyone editing the
   bundled catalog. A `rg "claude-sonnet-4-6" src/` would be required to
   find it.

Fix: derive the default from the catalog. If the user has not picked a
preset, fall back to the anthropic preset's `default_model`. If the user
HAS picked a preset, use that preset's `default_model`. This makes the
default catalog-driven.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — change the `unwrap_or_else` at line 223

Current (line 217-223 area):
```rust
let model = file_provider
    .as_ref()
    .and_then(|p| p.model.clone())
    .or_else(|| preset.and_then(|p| p.default_model.clone()))
    .unwrap_or_else(|| "claude-sonnet-4-6".into());
```

Replace with:
```rust
let model = file_provider
    .as_ref()
    .and_then(|p| p.model.clone())
    .or_else(|| preset.and_then(|p| p.default_model.clone()))
    .unwrap_or_else(|| {
        // Fall back to the bundled catalog's anthropic default so the
        // hardcoded string lives in exactly one place: providers.toml.
        crate::providers::find_preset("anthropic")
            .map(|p| p.default_model.clone())
            .unwrap_or_default()
    });
```

The `unwrap_or_default()` outer fallback handles the (impossible-in-practice)
case where the bundled catalog has no anthropic entry — empty string would
be caught downstream by `Config::require_api_key` or the LLM client.

### 2. Tests

Add a test in `src/config.rs`'s `#[cfg(test)] mod tests` named
`default_model_follows_preset`, that:
- Uses the env lock + `PinnedRecursiveHomeNoLock` pattern.
- Sets `RECURSIVE_PROVIDER_PRESET=openai` (or whatever the env var is
  — read `from_env` first to confirm the name; if there isn't an env
  override for preset, write a config file with
  `provider.preset = "openai"`).
- Asserts that the resolved `model` equals the openai preset's
  `default_model` (read it via `find_preset("openai").default_model` so
  the test doesn't bake in the value).
- Asserts the model is NOT `"claude-sonnet-4-6"`.

Add a second test `default_model_falls_back_to_anthropic_catalog`,
that:
- No preset configured.
- Asserts the model equals `find_preset("anthropic").default_model`.

Both tests must use the env lock + PinnedHome pattern. Total test
addition: under 60 lines.

### 3. No documentation changes

The bundled `providers.toml` is already documented. The default model
is documented in the catalog's `default_model` field. This change just
makes runtime follow the catalog.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- The string `"claude-sonnet-4-6"` does not appear in `src/config.rs`
  (it may still appear in `src/cli/init.rs` — that's a separate goal)
- The two new tests pass
- Bundled catalog tests (`all_presets_non_empty`,
  `find_preset_anthropic`, etc.) still pass unmodified

## Notes for the agent

- Read `src/config.rs:208-230` (the `from_env` model resolution block)
  and `src/config.rs:908-1078` (the existing `provider_preset_resolution_chain`
  test) before editing.
- Read `src/test_util.rs` for the env lock + PinnedHome helpers.
- Do NOT touch `providers.toml` — this is a code change only.
- Do NOT touch `providers.rs`.
- Do NOT touch `Config::Debug` (it already redacts `api_key`).
- Do NOT touch any LLM provider or `src/llm/`.
- If `unwrap_or_default()` produces an empty string, downstream code
  should reject it. Verify that the existing
  `Config::require_api_key`-equivalent for model (if any) handles empty
  string; if not, that's out of scope for this goal.
- Run `cargo test --workspace` (not just `cargo check`) before
  declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless.

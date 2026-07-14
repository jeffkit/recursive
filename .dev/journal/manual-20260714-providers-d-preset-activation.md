# Manual edit: providers-d-preset-activation

**Date**: 2026-07-14
**Goal**: Let `provider.preset = "<id>"` activate a preset the user added via
`~/.recursive/providers.d/<id>.toml`. Previously `Config::from_env` resolved
preset ids with the bundled-only `find_preset`, so every providers.d preset id
was rejected at startup with `provider.preset = "nvidia" not found in
providers.toml`, which surfaced in the TUI as the generic
"offline — no LLM provider configured" state.
**Files touched**: `src/config.rs`
**Tests added**: Case 7 in `provider_preset_resolution_chain` — writes a preset
to `providers.d/`, then asserts `provider.preset = "case7-vendor"` resolves
api_base/model/type (7a), pulls api_key from the preset's `key_env` env var
(7b), and that the providers.d id appears in the unknown-id error's valid list
(7c, proving `all_presets_effective` drives the message).
**Notes**:
- Switched `from_env`'s preset lookup from `find_preset` (bundled-only,
  `&'static`) to `find_preset_effective` (bundled + providers.d + remote
  cache, owned). `preset` is now `Option<ProviderPreset>` (owned), so
  downstream uses go through `preset.as_ref().map(...)` instead of borrowing
  the `&'static` reference. The final `preset: preset.map(|p| p.id.clone())`
  in the struct literal still consumes it (last use).
- The strict bundled-only `find_preset` is intentionally preserved for other
  callers; only `from_env` switched to the effective catalog, matching what
  `recursive init --provider <id>` (uses `find_preset_extended`) and
  `recursive providers add` (writes providers.d) already expect.
- Error message wording changed from "not found in providers.toml" to
  "not found in providers.toml or ~/.recursive/providers.d/.", and the valid-id
  list now comes from `all_presets_effective()` so providers.d ids are listed.
- Test gotcha: `PinnedRecursiveHomeNoLock::new(tmp.path())` sets
  `RECURSIVE_HOME = tmp.path()` verbatim. `user_data_dir()` returns it
  verbatim (no `.recursive`), while `config_file_path()` *appends* `.recursive`.
  So in this test config.toml lives at `tmp/.recursive/config.toml` but
  providers.d lives at `tmp/providers.d/` — NOT `tmp/.recursive/providers.d/`.
  Also `ProviderPreset.models` has no `#[serde(default)]`, so a providers.d
  preset file must include a `[[providers.models]]` table or it silently
  fails to parse (warned + skipped by `additional_presets`).
- Verified end-to-end against the user's real `~/.recursive` (preset =
  "nvidia", providers.d/nvidia.toml, NVIDIA_API_KEY in secrets.env):
  `recursive config show` now resolves preset=nvidia, api_base, model, and
  api_key from NVIDIA_API_KEY. Previously it errored out.
- Worktree: `.worktrees/providers-d-preset` on branch
  `fix/providers-d-preset-activation`. Gates: `cargo fmt --all`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --workspace` — all clean.

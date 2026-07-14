# Manual edit: init-onboarding-smooth

**Date**: 2026-07-14
**Goal**: Make `recursive init` (a) actually save custom-vendor presets to
`~/.recursive/providers.d/`, (b) refresh the remote catalog up front so
the user sees the latest upstream models (not just the compile-time
bundled snapshot), and (c) advertise the persistence surface in the
wizard's tail and in the README.
**Files touched**:
- src/providers.rs — add `pub fn write_user_preset` (writes a single
  preset wrapped in `[[providers]]` so the same `PresetsFile` envelope
  used by `additional_presets()` parses it), `pub struct WrittenPreset`.
- crates/recursive-cli/src/cli/init.rs — manual-mode branch now collects
  vendor id / display name / key_env and writes the preset under
  `~/.recursive/providers.d/<id>.toml`; default_response adds
  `+N more` hints per vendor; tail hints at `providers list` /
  `providers update` / the `~/.recursive/providers.d/` drop-in path;
  top of wizard refreshes the catalog (5 s timeout, fail-soft).
- README.md — new "Adding a custom provider" subsection under
  Configuration, covering wizard flow, hand-written preset format, the
  remote catalog, and how to override a bundled id.

**Tests added**:
- src/providers.rs::tests::write_user_preset_round_trips_through_additional_presets
- src/providers.rs::tests::write_user_preset_overwrites_existing_file
- crates/recursive-cli/src/cli/init.rs::tests::slugify_simple_id /
  slugify_lowercases_and_replaces_spaces /
  slugify_replaces_path_traversal_chars /
  slugify_trims_leading_and_trailing_dashes /
  slugify_keeps_underscore / slugify_preserves_digits
- crates/recursive-cli/src/cli/init.rs::tests::format_models_brief_only_default
  / _two_models_one_default_one_extra /
  _many_models_shows_extra_tail

**Notes**:
- First cut of `write_user_preset` used `toml::to_string_pretty(&preset)`
  directly, which serialised a *bare* `{ id = ..., name = ... }` table —
  `additional_presets()` silently skipped it because `PresetsFile` only
  recognises `[[providers]]`. Switched to a `PresetsFileSer` wrapper
  struct so the round-trip parser sees the same envelope the bundled
  catalog uses. Caught by the round-trip test.
- Verified end-to-end: ran `recursive init` against a fake custom
  vendor, confirmed `~/.recursive/providers.d/<id>.toml` was created
  with the right content, then deleted the test artefact.
- Did **not** touch `Config::from_env`'s strict "preset must be in
  bundled catalog" check. Once a user picks a custom vendor through
  the wizard, `init` writes `provider.preset = <id>` which is invalid
  against the bundled catalog; the next `recursive run` will fail at
  config load with "preset = X not found in providers.toml" until the
  user picks a bundled preset or runs `config set provider.preset ""`.
  That's a separate limitation (not in scope for this PR) and probably
  belongs to Goal 352.


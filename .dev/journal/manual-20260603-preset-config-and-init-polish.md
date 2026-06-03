# Manual edit: provider.preset config field + init wizard polish

**Date**: 2026-06-03
**Goal**: Let users write `provider.preset = "deepseek"` in `~/.recursive/config.toml`
to auto-fill type/api_base/model/api_key from the bundled `providers.toml`
catalog, instead of hand-writing 4 fields. Polish the `recursive init` flow
to support `--provider`/`--model`/`--api-key` non-interactive flags and to
auto-detect `key_env` env vars.

Bundled the TUI bypass regression fix (3 files reading env vars directly,
bypassing `Config::from_env`) so the preset chain works uniformly in CLI and
TUI.

**Files touched**:
- `src/providers.rs` — added `find_preset_by_api_base()` helper
- `src/config_file.rs` — added `preset: Option<String>` to `ProviderSection`;
  made `config_file_path()` honor `RECURSIVE_HOME` for tests
- `src/config.rs` — preset resolution in `from_env`; documented asymmetric
  api_key chain; consolidated env-mutating tests via `PinnedRecursiveHome`
- `src/cli/init.rs` — `resolve_preset_choice` extracted; env-var prefill for
  API key; manual-mode defaults use catalog lookup; `detect_current_preset`
  for re-run pre-selection
- `src/main.rs` — `Cmd::Init` is now a struct with `--provider`/`--model`/
  `--api-key`; `ConfigCmd::Show` prints `preset:` and resolved catalog line;
  `SessionCmd::Show` prints preset when set; `SessionWriter::create_with_tools`
  threaded through
- `src/session.rs` — `SessionFile.preset` and `SessionMeta.preset` added with
  `#[serde(default, skip_serializing_if = "Option::is_none")]` for back-compat
- `src/cli/session.rs`, `src/cli/resume.rs` — pass `preset` through
- `src/multi.rs`, `src/tools/team_manage.rs` — added `preset: None` to literals
- `src/tui/cost.rs` — `detect_model_name` now uses `Config::from_env()` (no
  more direct env reads)
- `src/tui/ui/modal.rs` — `render_model_body` uses `Config::from_env()` and
  `find_preset_by_api_base` (replaces `model.starts_with("deepseek")` heuristic)
- `src/tui/app/state.rs` — updated test to expect `claude-sonnet-4-6` and
  added Part D for preset resolution
- `tests/agui_e2e.rs`, `tests/agent_team_integration.rs`, `tests/http.rs`,
  `tests/v050_integration.rs`, `tests/resume_by_id.rs` — added `preset: None`
  to test config literals / `create_with_tools` call sites

**Tests added**:
- `src/providers.rs::find_preset_by_api_base_known` and `_unknown`
- `src/config_file.rs::parse_provider_section_with_preset` and
  `set_value_preset_round_trips`
- `src/config.rs::provider_preset_resolution_chain` (6 sub-cases under one
  PinnedRecursiveHome lock, per AGENTS.md:107-117)
- `src/cli/init.rs::resolve_preset_choice` (6 unit tests)
- `src/session.rs::session_meta_preserves_preset_field` (round-trip +
  skip_serializing_if check)
- `src/tui/app/state.rs::detect_model_name_falls_back_to_config_file` Part D
  (preset resolution)

**Notes**:

- **Asymmetric api_key chain**: `RECURSIVE_API_KEY`/`OPENAI_API_KEY` (generic)
  rank ABOVE file explicit; preset's `key_env` (e.g. `DEEPSEEK_API_KEY`) ranks
  BELOW file explicit. Inverting step 3 would silently override a user's
  `api_key = "sk-old"` whenever `DEEPSEEK_API_KEY` happened to be in their
  shell. Documented in `from_env`'s doc comment.
- **TUI bypass regression**: 3 files read env vars directly via `std::env::var`,
  bypassing `Config::from_env`. Fixed `tui/cost.rs::detect_model_name` and
  `tui/ui/modal.rs::render_model_body`. The third hit in `tui/mod.rs:60` was
  a false positive (it was `enable_raw_mode()`, not an env read).
- **Test isolation gotcha**: `dirs::home_dir()` on macOS uses
  `getpwuid_r` after `HOME` env, so `PinnedHome` (sets HOME) races with
  tests that mutate HOME directly without the env lock. Fix: also honor
  `RECURSIVE_HOME` in `config_file_path()` and switch the consolidated
  config test to `PinnedRecursiveHome`. Tracked in code comment.
- **`SessionMeta.preset` write-once-at-create**: the preset is recorded when
  the session is created, but subsequent config-file edits are not propagated
  mid-session (consistent with `model` / `provider_type` behavior). The
  field is `#[serde(default, skip_serializing_if = "Option::is_none")]` so
  pre-preset-config sessions round-trip cleanly.
- **Quality gates**: `cargo test --workspace`, `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo fmt --all -- --check` — all clean.
  One flaky session test re-ran cleanly after explicitly dropping the first
  writer before creating the second.
- **Manual smoke**: `RECURSIVE_HOME=<tmp> recursive init --provider deepseek
  --api-key sk-test` writes `preset = "deepseek"`, `model = "deepseek-chat"`,
  `api_key = "sk-test"`. `recursive config show` then prints the catalog
  resolution line.

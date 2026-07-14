# Manual edit: tui-offline-status

**Date**: 2026-07-14
**Goal**: Fix the misleading TUI state when no LLM provider is configured.
When `~/.recursive/config.toml`'s `[provider]` is empty, the TUI status bar
stayed stuck at `starting…` forever and showed the hardcoded model fallback
`deepseek-v4-flash` — implying the agent was configured when in fact the
runtime had built as `Offline` (no API key) and could never run. The user had
no in-TUI signal of the real state or what to do next.

**Root cause**:
- `model` fallback in `src/config.rs::Config::from_env` (step 4) hardcodes
  `find_preset("deepseek").default_model` = `deepseek-v4-flash` when nothing
  is configured. This is display-only; the runtime can't actually run.
- `UiEvent::RuntimeReady` was only emitted in the `RuntimeBuild::Ready`
  branch of `backend.rs::worker_loop` init. The `RuntimeBuild::Offline`
  branch was silent, so `App::connected` never flipped and the status bar's
  only two states (`starting…` / `local`) never left `starting…`.

**Fix** — introduce a first-class offline connection state and an actionable
setup path:
- New `UiEvent::RuntimeOffline { reason }`; emitted at worker init when the
  runtime builds as `Offline` (mirrors the existing `RuntimeReady` send).
- `App::offline_reason: Option<String>`; set by `RuntimeOffline`, cleared by
  `RuntimeReady`.
- Status bar (`ui/status.rs`) connection label is now three-state:
  `local` (green) / `offline` (red) / `starting…` (yellow, transient). When
  offline, the model slot shows `no provider` (red) instead of the misleading
  `deepseek-v4-flash` fallback. Assertions target specific spans, not joined
  text, because the workspace path can itself contain "offline".
- Empty-state chat area (`ui/chat.rs::render_empty_state`) renders an
  actionable setup hint when offline (recursive init wizard, or
  `recursive config set provider.preset` + `recursive config set-secret`,
  then `/exit` and restart) in place of the "Type a message to start" splash.
  Deliberately NOT pushed as transcript System blocks — that would suppress
  the boot splash and pollute the transcript.
- `runtime_builder.rs`: extracted `offline_no_provider_reason()` so the
  init-time `RuntimeOffline` and the send-while-offline `UiEvent::Error`
  share one actionable message that points to `recursive init` first.
- `commands.rs::build_model_lines` (`/model`): shows a "Not configured — no
  API key set" banner when `Config::from_env` resolves no key, instead of
  presenting the hardcoded fallback as a live configuration.

**Design principle**: `App::model_name` keeps "the model that would be used"
semantics (used for session naming + cost lookup) and is NOT changed; the
connection/offline state is a separate axis owned by `offline_reason`. This
keeps pricing/session logic uncontaminated.

**Files touched**:
- crates/recursive-tui/src/events.rs
- crates/recursive-tui/src/app/mod.rs
- crates/recursive-tui/src/app/state.rs
- crates/recursive-tui/src/app/event_loop.rs
- crates/recursive-tui/src/backend.rs
- crates/recursive-tui/src/runtime_builder.rs
- crates/recursive-tui/src/ui/status.rs
- crates/recursive-tui/src/ui/chat.rs
- crates/recursive-tui/src/commands.rs
- crates/recursive-tui/tests/pty_regression.rs

**Tests added**:
- status.rs: `status_bar_shows_offline_when_offline_reason_set`,
  `status_bar_offline_resolves_to_local_after_runtime_ready`
- event_loop.rs: `runtime_offline_sets_reason_without_polluting_transcript`,
  `runtime_ready_clears_offline_reason`
- runtime_builder.rs: `offline_backend_emits_runtime_offline_at_init`,
  `offline_no_provider_reason_mentions_init_and_config`; updated
  `offline_mode_and_config_file_resolution` assertions for the new reason
  text (now mentions `recursive init`).
- commands.rs: `build_model_lines_warns_when_no_api_key_configured`
- chat.rs: `render_empty_state_shows_offline_setup_guidance`
- pty_regression.rs: `pty_boot_renders_splash` now accepts either the online
  splash or the offline setup guidance (the test boots the real binary
  against the real `~/.recursive/config.toml`, which may be either).

**Notes**:
- Worktree: `.worktrees/tui-offline-status` on branch `tui-offline-status`.
- The PTY boot test previously asserted "Type a message to start" unconditionally,
  which would have broken on any empty-config environment (e.g. CI) after this
  change; loosened to accept the offline guidance as a valid non-blank boot.
- Quality gates run clean: `cargo test --workspace`, `cargo clippy
  --all-targets --all-features -- -D warnings`, `cargo fmt --all`,
  `tui-test-presence.sh`. `tui-mutants.sh --jobs 4` (copy mode, since the
  tree has uncommitted changes) run separately.

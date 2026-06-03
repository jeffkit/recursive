# Manual edit: WeChat iLink channel adapter

**Date**: 2026-06-03  
**Goal**: Implement a WeChat ClawBot channel for Recursive that bridges the iLink protocol with the agent runtime. Supports three operating modes (TUI+WeChat, TUI slash command, headless daemon) and workspace-level multi-session management from WeChat.

**Files touched**:
- `Cargo.toml` — added `wechatbot = { version = "0.3.2", optional = true }`, `qrcode = { version = "0.14", optional = true }`, and `weixin` feature gate
- `src/weixin/mod.rs` — new module root with documentation
- `src/weixin/commands.rs` — WeChat command parser (`/l`, `/s`, `/c N`, `/r`, `/help`)
- `src/weixin/daemon.rs` — `WeixinDaemon` workspace singleton, QR-login via `wechatbot::WeChatBot`, session listing, iLink polling + message dispatch
- `src/lib.rs` — exposed `pub mod weixin` behind `#[cfg(feature = "weixin")]`
- `src/tui/events.rs` — added `UiEvent::WeixinMessage` variant and `WeixinBackendRequest` side-channel struct
- `src/tui/backend.rs` — added `weixin_tx` to `Backend`, extended `worker_loop` to `tokio::select!` on both action_rx and weixin_rx
- `src/tui/app/event_loop.rs` — handled `UiEvent::WeixinMessage` → push `TranscriptBlock::WeixinMessage`
- `src/tui/model.rs` — added `TranscriptBlock::WeixinMessage { user_id, text }` variant
- `src/tui/ui/transcript.rs` — added `render_weixin_message()` with 📱 green prefix
- `src/tui/mod.rs` — exposed `run_with_backend(backend)` alongside existing `run()` for pre-wired backends
- `src/main.rs` — added `--weixin`, `--weixin-base-url`, `--weixin-cred-path` flags; added `weixin-daemon` subcommand; added `run_tui_with_weixin()` and `run_weixin_headless_daemon()` helpers

**Tests added**:
- `src/weixin/commands.rs` — unit tests for all command parsing variants (list, sessions, change, reset, unknown, regular messages)

**Notes**:
- WeChat messages are processed sequentially after the current agent turn completes — same as the existing loop/timer event queue model.
- The `WeixinDaemon` stores credentials at `~/.recursive/<workspace_hash>/weixin_creds.json` by default; auto-reconnects on subsequent starts.
- `wechatbot::BotOptions::base_url` allows transparent swap to `ilink-hub` proxy by setting `--weixin-base-url`.
- Multi-session management (`/s`, `/c N`) is scaffolded with stub replies; full session-switch support depends on integrating the HTTP session API and will be a follow-up.
- The `Either<L, R>` enum is used only in `worker_loop` for the select! fan-out; non-weixin builds type-annotate it as `Either<UserAction, ()>` to satisfy the compiler.

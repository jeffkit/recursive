//! WeChat iLink channel adapter for Recursive.
//!
//! This module provides a workspace-level WeChat channel that bridges the
//! iLink protocol with Recursive's agent runtime. It supports three operating
//! modes:
//!
//! 1. **TUI + WeChat** (`recursive --weixin`): TUI starts normally and a
//!    WeChat daemon runs in the background. WeChat messages are displayed in
//!    the TUI with a 📱 prefix.
//!
//! 2. **TUI slash command** (`/weixin`): WeChat daemon starts on demand from
//!    within a running TUI session.
//!
//! 3. **Headless daemon** (`recursive weixin-daemon`): Agent runs without a
//!    TUI, driven entirely by WeChat messages.
//!
//! # Session multiplexer (issue #105 §3)
//!
//! Every WeChat user gets their own session binding, persisted across
//! daemon restarts (`weixin_sessions.json` under the workspace user dir —
//! see [`session_map`]). Control commands operate on the caller's
//! binding:
//! - `/l [N]` — last N turns of **your** session
//! - `/s`     — list workspace sessions (numbered, `/c`-compatible)
//! - `/c N`   — rebind yourself to session N
//! - `/r`     — drop your binding; next message starts a fresh session

pub mod commands;
pub mod daemon;
pub mod session_map;

pub use daemon::{WeixinDaemon, WeixinDaemonOptions, WeixinRequest};
pub use session_map::WeixinSessionMap;

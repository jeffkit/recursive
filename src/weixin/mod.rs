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
//! # Session multiplexer
//!
//! A single WeChat account is shared across all sessions. Users can:
//! - `/l`  — list last 10 turns of the current session
//! - `/s`  — list all sessions in the workspace
//! - `/c N`— switch to session N
//! - `/r`  — reset current session (start fresh)

pub mod commands;
pub mod daemon;

pub use daemon::{WeixinDaemon, WeixinDaemonOptions, WeixinRequest};

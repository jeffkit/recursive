//! `recursive-tui` — standalone TUI entry-point for the Recursive agent.
//!
//! This crate re-exports the public API of `recursive::tui` so that external
//! users can depend on `recursive-tui` directly without pulling in the entire
//! `recursive-agent` crate as a direct dependency.
//!
//! ## Usage
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     recursive_tui::run().await
//! }
//! ```

// Re-export the main TUI entry points.
pub use recursive::tui::run;
pub use recursive::tui::run_with_backend;

// Re-export public types used by embedders.
pub use recursive::tui::app;
pub use recursive::tui::backend;
pub use recursive::tui::commands;
pub use recursive::tui::events;
pub use recursive::tui::model;
pub use recursive::tui::skill_commands;
pub use recursive::tui::ui;

// Re-export convenience types from the tui module root.
pub use recursive::tui::AppScreen;
pub use recursive::tui::DiffHunk;
pub use recursive::tui::DiffLine;
pub use recursive::tui::DiffLineKind;
pub use recursive::tui::InputMode;
pub use recursive::tui::PromptInputState;
pub use recursive::tui::TranscriptBlock;
pub use recursive::tui::UsageStats;

// WeChat integration (when weixin feature is enabled on the underlying crate).
#[cfg(feature = "weixin")]
pub use recursive::tui::events::WeixinBackendRequest;

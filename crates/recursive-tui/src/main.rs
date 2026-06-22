//! `recursive-tui` — interactive terminal UI for the Recursive agent.
//!
//! Run `recursive-tui` to start the TUI. Alternatively, use
//! `recursive repl` if you have the CLI installed.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    recursive_tui::run().await
}

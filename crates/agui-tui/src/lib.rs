//! Library half of `agui-tui`.
//!
//! Splitting the App and the UI rendering out of `main.rs` lets the
//! tests import them without trying to pull in the binary's main
//! function. The binary itself just wires these together to a real
//! terminal and `AguiClient`.

pub mod app;
pub mod ui;

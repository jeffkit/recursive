//! Library surface of the Recursive TUI.
//!
//! The binary at `src/main.rs` is intentionally tiny — most of the
//! application logic lives in these modules so it can be unit- and
//! integration-tested.

pub mod app;
pub mod backend;
pub mod events;
pub mod keymap;
pub mod ui;

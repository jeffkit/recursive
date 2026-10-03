//! Execution-domain tools: direct file / shell manipulation.
//!
//! # Sub-modules
//!
//! Moved out of the flat `src/tools/` directory (issue #82, part 3/3 of the
//! #60 domain split):
//!
//! - [`fs`] — `Read` / `Write` with session-scoped sandbox roots.
//! - [`edit`] — `Edit` (exact / fuzzy string replacement, diff output).
//! - [`shell`] — `Bash` (`RunShell`), routed through the [`ToolTransport`]
//!   abstraction (`crate::tools::transport_layer::transport`).
//! - [`glob`] — `Glob` (pattern-based file listing).
//!
//! `src/tools/mod.rs` keeps `pub use` re-exports so existing paths
//! (`crate::tools::shell`, …) keep resolving.

pub mod edit;
pub mod fs;
pub mod glob;
pub mod shell;

//! Knowledge & retrieval domain: persistent memory, facts, episodic recall,
//! and the read-only inspection helpers (token estimation, line counting,
//! workspace file search).
//!
//! # Sub-modules
//!
//! Moved out of the flat `src/tools/` directory (issue #80, part 2/3 of the
//! #60 domain split):
//!
//! - [`facts`] — Memory Layer 2: structured semantic facts with full-text
//!   search, dedup, eviction (`remember`/`recall`/`forget`/`update_fact`).
//! - [`memory`] — Memory Layer 1: working-memory scratchpad KV plus the
//!   legacy note-store tools (`remember`/`recall`/`forget`,
//!   `scratchpad_*`).
//! - [`episodic_recall`] — Memory Layer 3: search past session transcripts.
//! - [`session_query`] — the session retrieval family (issue #131): a derived
//!   FTS5 index (`session_search` / `session_event_search` / `session_trace` /
//!   `session_event_trace` / `session_event_read`).
//! - [`estimate_tokens`] / [`count_lines`] — read-only inspection tools.
//! - [`search`] — the `Grep` tool (workspace substring/regex search).
//!
//! `src/tools/mod.rs` keeps `pub use` re-exports so existing paths
//! (`crate::tools::facts`, …) keep resolving.

pub mod count_lines;
pub mod episodic_recall;
pub mod estimate_tokens;
pub mod facts;
pub mod memory;
pub mod search;
#[cfg(feature = "session-index")]
pub mod session_query;

//! Multi-tenant workspace registry (issue #135).
//!
//! Recursive sessions are flat: there is no grouping above a session, no
//! archive lifecycle, and no "what is still running here" gate before a
//! project is put away. This module is the organisational base layer for that:
//! a per-user registry of workspaces (projects) keyed by the realpath of their
//! directory, carrying a *header-only* session index and an archive lifecycle.
//! It is the sibling of the tenant-identity work in the HTTP layer — that
//! layer answers *who* is calling; this one answers *what* they are organising.
//!
//! Borrowed from DSH's `packages/workspace/`:
//!
//! - **header-only index** — only session headers (cwd, status, timestamps,
//!   prompts) are indexed; event bodies are never loaded, so listing stays cheap
//!   for large histories ([`WorkspaceRegistry::index_sessions`]).
//! - **realpath canon** — a directory's identity is its `realpath`, so a
//!   symlink pointing at an already-owned directory is a conflict, not a second
//!   entry ([`WorkspaceRegistry::create`]).
//! - **two-write mutations + crash recovery** — create/delete/archive write a
//!   pending marker before the record and clear it after; startup completes any
//!   interrupted operation, and an inconsistency with no marker fails loud
//!   ([`WorkspaceRegistry::open`]).
//! - **archive-admission capability seam** — before archiving, an
//!   [`ActivityProbe`] is asked what is still running (turn / job / subagent /
//!   scheduled wakeup). Under [`ArchivePolicy::StopThenArchive`] the archive
//!   write lands *before* the stop is issued — the archive gate precedes the
//!   work it would wake. Removing a project never deletes its directory or
//!   history.
//!
//! ```no_run
//! use recursive::workspace::{ArchivePolicy, NoActivityProbe, WorkspaceRegistry};
//!
//! let registry = WorkspaceRegistry::open()?;
//! let record = registry.create(std::path::Path::new("/some/project"), None)?;
//! registry.index_sessions(&record.id)?;
//! registry.archive(&record.id, &NoActivityProbe, ArchivePolicy::Admit)?;
//! registry.remove(&record.id)?; // non-destructive: the directory stays
//! # Ok::<(), recursive::Error>(())
//! ```

mod activity;
mod registry;

pub use activity::{
    ActiveWork, ActiveWorkKind, ActivityProbe, NoActivityProbe, StoreActivityProbe,
};
pub use registry::{
    ArchiveOutcome, ArchivePolicy, RecoveryReport, SessionHeader, WorkspaceRecord,
    WorkspaceRegistry,
};

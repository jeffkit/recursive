//! Policy domain: permission checks, audit metadata and URL guarding.
//!
//! # Sub-modules
//!
//! Moved out of the flat `src/tools/` directory (issue #82, part 3/3 of the
//! #60 domain split):
//!
//! - [`policy`] — documentation anchor for the L1 policy sandbox split
//!   (`PolicyToolSetProvider` lives in [`crate::tool_set_provider`]).
//! - [`policy_sandbox`] — L1 policy types (`FsPolicy`, `ShellPolicy`,
//!   `PolicyConfig`) validated at the Rust layer (no OS isolation).
//! - [`permission_pipeline`] — 7-stage permission orchestration
//!   ([`permission_pipeline::PermissionPipeline`]).
//! - [`audit`] — tool-call audit metadata ([`audit::AuditMeta`]).
//! - [`url_guard`] — SSRF guard for web/http tools
//!   ([`url_guard::validate_url`], [`url_guard::is_private_ip`]).
//!
//! `src/tools/mod.rs` keeps `pub use` re-exports so existing paths
//! (`crate::tools::audit`, …) keep resolving.

pub mod audit;
pub mod permission_pipeline;
pub mod policy;
pub mod policy_sandbox;
pub(crate) mod url_guard;

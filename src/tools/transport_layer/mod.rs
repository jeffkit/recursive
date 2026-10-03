//! Transport layer: pluggable execution backends and their providers.
//!
//! The [`transport::ToolTransport`] trait decouples tools from direct
//! filesystem/shell access; the provider modules build standard tool sets
//! bound to a specific backend tier (local / Docker container / E2B microVM).
//!
//! # Sub-modules
//!
//! Moved out of the flat `src/tools/` directory (issue #82, part 3/3 of the
//! #60 domain split):
//!
//! - [`transport`] — the [`transport::ToolTransport`] trait, local + SSH
//!   implementations, capabilities and walk helpers.
//! - [`container_transport`] — Goal-403 container-tier transport
//!   (feature `cloud-runtime`).
//! - [`container_provider`] — container-tier [`crate::tools::ToolRegistry`]
//!   builder (feature `cloud-runtime`).
//! - [`docker_sandbox`] / [`docker_provider`] — L2 Docker sandbox shell tool
//!   and its provider (feature `cloud-runtime`).
//! - [`e2b_provider`] — L3 E2B Firecracker microVM transport + provider
//!   (feature `e2b-sandbox`).
//!
//! `src/tools/mod.rs` keeps `pub use` re-exports so existing paths
//! (`crate::tools::transport`, …) keep resolving.

#[cfg(feature = "cloud-runtime")]
pub mod container_provider;
#[cfg(feature = "cloud-runtime")]
pub mod container_transport;
#[cfg(feature = "cloud-runtime")]
pub mod docker_provider;
#[cfg(feature = "cloud-runtime")]
pub mod docker_sandbox;
#[cfg(feature = "e2b-sandbox")]
pub mod e2b_provider;
pub mod transport;

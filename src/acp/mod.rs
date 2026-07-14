//! ACP (Agent Client Protocol) v1 server support.
//!
//! Wire types live in [`protocol`] (re-exported from
//! `agent-client-protocol-schema`). The [`server`] module runs the stdio
//! JSON-RPC transport loop. [`bridge`] translates agent events into ACP
//! `session/update` notifications. [`session`] tracks per-session
//! [`AgentRuntime`] state, cwd, turn counter, and transcript.

pub mod bridge;
pub mod permission;
pub mod protocol;
pub mod server;
pub mod session;
pub mod tool_kind;

pub use tool_kind::ToolKind;

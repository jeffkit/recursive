//! ACP (Agent Client Protocol) v1 server support.
//!
//! Wire types live in [`protocol`] (re-exported from
//! `agent-client-protocol-schema`). Server loop, session bridge, fs/permission
//! adapters will be added in subsequent sprints (see
//! `.dev/goals/325-acp-protocol-support.md`).

pub mod protocol;

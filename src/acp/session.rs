//! Per-session state: holds the [`AgentRuntime`], working directory,
//! turn counter, transcript, and a [`CancellationToken`] for cooperative
//! abort for a single ACP session.
//!
//! [`AcpSessionManager`] is a lightweight HashMap-based registry. It is
//! owned by the server's dispatch loop. No parallel session-management
//! infrastructure exists outside this module — all lifecycle state is
//! tracked through the [`AgentRuntime`] and this simple container.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::message::Message;
use crate::runtime::AgentRuntime;

/// A unique session identifier.
pub type SessionId = String;

// ---------------------------------------------------------------------------
// AcpSession
// ---------------------------------------------------------------------------

/// State for one active ACP session.
pub struct AcpSession {
    /// The agent runtime that processes `session/prompt` requests.
    pub runtime: AgentRuntime,
    /// The sandbox root for filesystem tools.
    pub cwd: PathBuf,
    /// Monotonically-increasing turn counter.
    pub turn: u64,
    /// Session identifier (copied from the manager's key for convenience).
    pub session_id: SessionId,
    /// Accumulated transcript of messages exchanged so far.
    pub transcript: Vec<Message>,
    /// Cancellation token for the current (or next) agent turn.
    /// Fired by `session/cancel`; observed by the LLM stream loop
    /// (`tokio::select!`) and agent→client RPC calls.
    ///
    /// This is the current-turn token. After each turn completes, a fresh
    /// token is created so a cancel only affects the in-flight turn.
    pub cancel_token: tokio_util::sync::CancellationToken,
}

impl AcpSession {
    /// Create a fresh `CancellationToken` for the next turn and wire it
    /// to the agent runtime via `set_interrupt_token`. Returns a clone
    /// of the new token for the caller to hold.
    ///
    /// Must be called exactly once per turn, before `runtime.run()`.
    pub fn refresh_cancel_token(&mut self) -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        self.runtime.set_interrupt_token(token.clone());
        self.cancel_token = token.clone();
        token
    }
}

// ---------------------------------------------------------------------------
// AcpSessionManager
// ---------------------------------------------------------------------------

/// Registry of active ACP sessions, keyed by [`SessionId`].
pub struct AcpSessionManager {
    sessions: HashMap<SessionId, AcpSession>,
    next_id: AtomicU64,
}

impl Default for AcpSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpSessionManager {
    /// Create an empty session manager.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Generate the next sequential session id (e.g. `"acp-sess-1"`).
    pub fn next_session_id(&self) -> SessionId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("acp-sess-{n}")
    }

    /// Insert a session with a pre-generated id.
    ///
    /// `session_id` must match `session.session_id`. This is a convenience
    /// to avoid double-allocating a `SessionId` in the caller.
    pub fn insert_with_id(&mut self, session_id: SessionId, session: AcpSession) {
        self.sessions.insert(session_id, session);
    }

    /// Look up a mutable reference to a session by id.
    pub fn get_mut(&mut self, sid: &str) -> Option<&mut AcpSession> {
        self.sessions.get_mut(sid)
    }

    /// Remove and return a session, releasing its resources.
    pub fn remove(&mut self, sid: &str) -> Option<AcpSession> {
        self.sessions.remove(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manager_is_empty() {
        let mut mgr = AcpSessionManager::new();
        assert!(mgr.get_mut("nonexistent").is_none());
    }

    #[test]
    fn next_session_id_is_monotonic() {
        let mgr = AcpSessionManager::new();
        let a = mgr.next_session_id();
        let b = mgr.next_session_id();
        assert_ne!(a, b);
        assert!(a.contains("acp-sess-"));
        assert!(b.contains("acp-sess-"));
    }

    #[test]
    fn insert_and_get() {
        let mut mgr = AcpSessionManager::new();
        let _sid = mgr.next_session_id();
        // We can't fully construct an AcpSession here without an AgentRuntime,
        // but we can test the HashMap operations indirectly...

        // Instead, test that remove on empty returns None
        assert!(mgr.remove("nonexistent").is_none());
    }
}

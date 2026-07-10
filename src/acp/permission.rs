//! ACP Sprint-1 permission bridge.
//!
//! Defines [`PermissionOutcome`], [`PermissionDecision`], and
//! a complete permission flow connecting the ACP `session/request_permission`
//! notification/response protocol to the agent's [`PermissionHook`].
//!
//! # Architecture
//!
//! 1. When a tool call requires permission, [`AcpPermissionHook::check`]
//!    sends a `session/request_permission` JSON-RPC notification to the
//!    client with `permission_id`, `tool_name`, and argument details.
//! 2. The client receives the notification (as a `session/update` with
//!    `sessionUpdate: "request_permission"`) and sends back a
//!    `session/request_permission` JSON-RPC request with the outcome.
//! 3. The ACP server's dispatch loop routes this to
//!    [`handle_request_permission`] which resolves the pending
//!    oneshot channel.
//! 4. [`AcpPermissionHook::check`] receives the outcome and returns
//!    `Allow` or `Deny`.
//! 5. A 30-second timeout defaults to `Deny`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::oneshot;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::PermissionDecision;
use crate::tools::PermissionHook;

/// The outcome of a client permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Client allowed the tool call.
    Allowed,
    /// Client denied the tool call.
    Denied,
    /// Client did not respond within the timeout.
    Timeout,
}

/// Translate a [`PermissionOutcome`] to [`PermissionDecision`].
pub fn translate(outcome: PermissionOutcome) -> PermissionDecision {
    match outcome {
        PermissionOutcome::Allowed => PermissionDecision::Allow,
        PermissionOutcome::Denied | PermissionOutcome::Timeout => {
            PermissionDecision::Deny("permission denied".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// PendingPermissionStore
// ---------------------------------------------------------------------------

/// Thread-safe store of pending permission requests, keyed by permission ID.
///
/// Shared between the [`AcpPermissionHook`] (which inserts entries when it
/// sends a notification) and the ACP server's dispatch loop (which resolves
/// entries when the client responds via `session/request_permission`).
#[derive(Clone, Default)]
pub struct PendingPermissionStore {
    inner: Arc<StdMutex<HashMap<String, oneshot::Sender<PermissionOutcome>>>>,
}

impl PendingPermissionStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Insert a pending permission request.
    pub fn insert(&self, id: String, tx: oneshot::Sender<PermissionOutcome>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(id, tx);
        }
    }

    /// Remove and return the sender for a given permission ID.
    pub fn remove(&self, id: &str) -> Option<oneshot::Sender<PermissionOutcome>> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(id))
    }

    /// Check if a permission ID is pending.
    #[allow(dead_code)]
    pub fn contains(&self, id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .is_some_and(|guard| guard.contains_key(id))
    }
}

// ---------------------------------------------------------------------------
// AcpPermissionHook
// ---------------------------------------------------------------------------

/// A [`PermissionHook`] that communicates permission requests to the ACP client.
///
/// When `check()` is called:
/// 1. Sends a `session/update` notification with `sessionUpdate: "request_permission"`
///    via the provided notification sender.
/// 2. Stores the oneshot sender in the shared [`PendingPermissionStore`].
/// 3. Waits for the client's response (or 30-second timeout).
/// 4. Returns `Allow` or `Deny` based on the outcome.
pub struct AcpPermissionHook {
    /// Shared store of pending requests.
    store: PendingPermissionStore,
    /// Session ID for ACP notification routing.
    session_id: String,
    /// Sender for JSON-RPC notifications to the client.
    notif_tx: tokio::sync::mpsc::UnboundedSender<Value>,
}

impl AcpPermissionHook {
    /// Create a new ACP permission hook.
    pub fn new(
        store: PendingPermissionStore,
        session_id: String,
        notif_tx: tokio::sync::mpsc::UnboundedSender<Value>,
    ) -> Self {
        Self {
            store,
            session_id,
            notif_tx,
        }
    }
}

#[async_trait]
impl PermissionHook for AcpPermissionHook {
    async fn check(&self, tool_name: &str, args: &Value) -> PermissionDecision {
        let perm_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();

        self.store.insert(perm_id.clone(), tx);

        // Build the request_permission notification
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "request_permission",
                    "permissionId": perm_id,
                    "toolName": tool_name,
                    "arguments": args,
                }
            }
        });

        let _ = self.notif_tx.send(notif);

        // Wait for client response with 30-second timeout
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(translate)
            .unwrap_or(PermissionDecision::Deny("permission timeout".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Handle session/request_permission response from client
// ---------------------------------------------------------------------------

/// Handle a `session/request_permission` request from the client.
///
/// The client sends back the outcome (granted=true/false) for a specific
/// permission_id. We look up the pending request in the store and resolve it.
///
/// Returns a JSON-RPC response indicating success or an error if the
/// permission_id was not found.
pub fn handle_request_permission(
    id: &Value,
    params: Option<&Value>,
    store: &PendingPermissionStore,
) -> Value {
    let params = match params {
        Some(p) => p,
        None => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Missing params",
                },
            });
        }
    };

    let perm_id = match params.get("permissionId").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Missing required field 'permissionId'",
                },
            });
        }
    };

    let outcome = match params.get("outcome").and_then(|v| v.as_str()) {
        Some("allowed") => PermissionOutcome::Allowed,
        Some("denied") | Some("rejected") => PermissionOutcome::Denied,
        _ => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Invalid or missing 'outcome' field (expected 'allowed' or 'denied')",
                },
            });
        }
    };

    match store.remove(perm_id) {
        Some(tx) => {
            let _ = tx.send(outcome);
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {},
            })
        }
        None => {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": format!("Permission request not found: {perm_id}"),
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── translate ─────────────────────────────────────────────────────────

    #[test]
    fn translate_allowed_to_allow() {
        assert_eq!(
            translate(PermissionOutcome::Allowed),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn translate_denied_to_deny() {
        assert_eq!(
            translate(PermissionOutcome::Denied),
            PermissionDecision::Deny("permission denied".to_string())
        );
    }

    #[test]
    fn translate_timeout_to_deny() {
        assert_eq!(
            translate(PermissionOutcome::Timeout),
            PermissionDecision::Deny("permission denied".to_string())
        );
    }

    // ── PendingPermissionStore ────────────────────────────────────────────

    #[test]
    fn store_insert_and_remove() {
        let store = PendingPermissionStore::new();
        let (tx, _rx) = oneshot::channel();
        store.insert("perm-1".into(), tx);
        assert!(store.contains("perm-1"));
        let removed = store.remove("perm-1");
        assert!(removed.is_some());
        assert!(!store.contains("perm-1"));
    }

    #[test]
    fn store_remove_nonexistent_returns_none() {
        let store = PendingPermissionStore::new();
        assert!(store.remove("nonexistent").is_none());
    }

    #[test]
    fn store_default_is_empty() {
        let store = PendingPermissionStore::default();
        assert!(!store.contains("anything"));
    }

    // ── handle_request_permission ─────────────────────────────────────────

    #[test]
    fn handle_permission_missing_params() {
        let store = PendingPermissionStore::new();
        let result = handle_request_permission(&Value::from(1), None, &store);
        assert!(result["error"].is_object());
        assert_eq!(result["error"]["code"], -32602);
    }

    #[test]
    fn handle_permission_missing_permission_id() {
        let store = PendingPermissionStore::new();
        let params = serde_json::json!({"outcome": "allowed"});
        let result = handle_request_permission(&Value::from(1), Some(&params), &store);
        assert!(result["error"].is_object());
        assert_eq!(result["error"]["code"], -32602);
    }

    #[test]
    fn handle_permission_invalid_outcome() {
        let store = PendingPermissionStore::new();
        let params = serde_json::json!({
            "permissionId": "p1",
            "outcome": "maybe"
        });
        let result = handle_request_permission(&Value::from(1), Some(&params), &store);
        assert!(result["error"].is_object());
        assert_eq!(result["error"]["code"], -32602);
    }

    #[test]
    fn handle_permission_nonexistent_id() {
        let store = PendingPermissionStore::new();
        let params = serde_json::json!({
            "permissionId": "nonexistent",
            "outcome": "allowed"
        });
        let result = handle_request_permission(&Value::from(1), Some(&params), &store);
        assert!(result["error"].is_object());
        assert_eq!(result["error"]["code"], -32000);
    }

    #[test]
    fn handle_permission_allowed_resolves() {
        let store = PendingPermissionStore::new();
        let (tx, mut rx) = oneshot::channel();
        store.insert("p1".into(), tx);

        let params = serde_json::json!({
            "permissionId": "p1",
            "outcome": "allowed"
        });
        let result = handle_request_permission(&Value::from(1), Some(&params), &store);
        assert!(result["result"].is_object());
        // The oneshot should have been resolved
        let outcome = rx.try_recv().unwrap();
        assert_eq!(outcome, PermissionOutcome::Allowed);
    }

    #[test]
    fn handle_permission_denied_resolves() {
        let store = PendingPermissionStore::new();
        let (tx, mut rx) = oneshot::channel();
        store.insert("p2".into(), tx);

        let params = serde_json::json!({
            "permissionId": "p2",
            "outcome": "denied"
        });
        let result = handle_request_permission(&Value::from(1), Some(&params), &store);
        assert!(result["result"].is_object());
        let outcome = rx.try_recv().unwrap();
        assert_eq!(outcome, PermissionOutcome::Denied);
    }
}

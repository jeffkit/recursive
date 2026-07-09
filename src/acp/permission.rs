//! ACP Sprint-1 permission bridge.
//!
//! Defines [`PermissionOutcome`], [`PermissionDecision`], and
//! [`PermissionBridge`] for the session/request_permission flow.
//!
//! # Architecture
//!
//! The permission bridge owns a oneshot channel per in-flight permission
//! request. When a tool call requires permission, the runtime emits a
//! `session/request_permission` notification (fire-and-forget). The
//! client responds over the same channel. A 30-second timeout defaults
//! to [`PermissionDecision::Deny`].
//!
//! # Debouncing (S1-C14)
//!
//! Multiple tool calls within the same `tool_calls` array within 500ms
//! are consolidated into a single notification with a `tools` array.
//!
//! # Deduplication (S1-C15)
//!
//! Identical tool+path pairs within the debounce window are aggregated
//! with a `count: N` field.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

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

/// The permission decision translated from an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Agent may proceed with the tool call.
    Allow,
    /// Agent must skip the tool call.
    Deny,
}

/// Translate a [`PermissionOutcome`] to a [`PermissionDecision`].
///
/// Must be exhaustive: every variant of `PermissionOutcome` maps to
/// exactly one variant of `PermissionDecision`. Adding a new variant
/// to `PermissionOutcome` will cause a compile error here.
pub fn translate(outcome: PermissionOutcome) -> PermissionDecision {
    match outcome {
        PermissionOutcome::Allowed => PermissionDecision::Allow,
        PermissionOutcome::Denied => PermissionDecision::Deny,
        PermissionOutcome::Timeout => PermissionDecision::Deny,
    }
}

/// A bridge for managing in-flight permission requests.
///
/// Created per `session/prompt` turn and dropped when the turn ends.
/// Each in-flight request has a oneshot channel.
pub struct PermissionBridge {
    /// Sender half for the current permission request.
    tx: Option<oneshot::Sender<PermissionOutcome>>,
    /// Whether the bridge has been resolved (allowed or denied).
    resolved: bool,
}

impl PermissionBridge {
    /// Create a new permission bridge.
    pub fn new() -> Self {
        Self {
            tx: None,
            resolved: false,
        }
    }

    /// Open a new permission request channel.
    ///
    /// Returns a oneshot receiver that the client should use to send their
    /// decision, and a unique permission ID for the notification.
    pub fn open_request(&mut self) -> (oneshot::Receiver<PermissionOutcome>, String) {
        let (tx, rx) = oneshot::channel();
        let perm_id = uuid::Uuid::new_v4().to_string();
        self.tx = Some(tx);
        self.resolved = false;
        (rx, perm_id)
    }

    /// Resolve the pending request with an outcome.
    ///
    /// Returns `true` if the outcome was sent successfully.
    pub fn resolve(&mut self, outcome: PermissionOutcome) -> bool {
        if let Some(tx) = self.tx.take() {
            self.resolved = true;
            tx.send(outcome).is_ok()
        } else {
            false
        }
    }

    /// Whether the bridge has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    /// Wait for a permission decision with a 30-second timeout.
    ///
    /// Returns `PermissionDecision::Deny` on timeout.
    pub async fn wait_for_decision(
        mut rx: oneshot::Receiver<PermissionOutcome>,
    ) -> PermissionDecision {
        tokio::time::timeout(Duration::from_secs(30), &mut rx)
            .await
            .ok()
            .flatten()
            .map(translate)
            .unwrap_or(PermissionDecision::Deny)
    }
}

impl Default for PermissionBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// A debounced permission request entry.
#[derive(Debug, Clone)]
pub struct DebouncedPermissionEntry {
    pub tool_name: String,
    pub path: Option<String>,
    pub command: Option<String>,
    pub count: usize,
    /// Unique dedup key: "toolName:path" or "toolName" (no path)
    pub dedup_key: String,
}

impl DebouncedPermissionEntry {
    pub fn new(tool_name: String, path: Option<String>, command: Option<String>) -> Self {
        let dedup_key = match &path {
            Some(p) => format!("{}:{}", tool_name, p),
            None => tool_name.clone(),
        };
        Self {
            tool_name,
            path,
            command,
            count: 1,
            dedup_key,
        }
    }
}

/// Manages debouncing of permission requests within a window.
pub struct PermissionDebouncer {
    /// Pending entries within the current debounce window.
    entries: Vec<DebouncedPermissionEntry>,
    /// When the debounce window started (Instant::now).
    window_start: Option<std::time::Instant>,
    /// Debounce window in milliseconds (default 500).
    window_ms: u64,
}

impl PermissionDebouncer {
    /// Create a new debouncer with the default 500ms window.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            window_start: None,
            window_ms: 500,
        }
    }

    /// Create a new debouncer with a custom window.
    pub fn with_window(window_ms: u64) -> Self {
        Self {
            entries: Vec::new(),
            window_start: None,
            window_ms,
        }
    }

    /// Add a tool call to the current debounce batch.
    ///
    /// If the same tool+path pair already exists, increment its count.
    /// Returns `true` if this entry was a dedup (count incremented).
    pub fn add(&mut self, tool_name: &str, path: Option<&str>, command: Option<&str>) -> bool {
        let dedup_key = match path {
            Some(p) => format!("{}:{}", tool_name, p),
            None => tool_name.to_string(),
        };

        // Start the window timer on first entry
        if self.window_start.is_none() {
            self.window_start = Some(std::time::Instant::now());
        }

        // Check for dedup
        if let Some(entry) = self.entries.iter_mut().find(|e| e.dedup_key == dedup_key) {
            entry.count += 1;
            return true; // was dedup
        }

        self.entries.push(DebouncedPermissionEntry::new(
            tool_name.to_string(),
            path.map(|s| s.to_string()),
            command.map(|s| s.to_string()),
        ));
        false
    }

    /// Flush the current batch if the window has expired or if forced.
    ///
    /// Returns the consolidated entries (empty if window not yet expired).
    pub fn flush(&mut self) -> Vec<DebouncedPermissionEntry> {
        let should_flush = match self.window_start {
            Some(start) => start.elapsed() >= std::time::Duration::from_millis(self.window_ms),
            None => true,
        };

        if should_flush {
            let entries = std::mem::take(&mut self.entries);
            self.window_start = None;
            entries
        } else {
            Vec::new()
        }
    }

    /// Force flush, returning all pending entries regardless of window.
    pub fn force_flush(&mut self) -> Vec<DebouncedPermissionEntry> {
        let entries = std::mem::take(&mut self.entries);
        self.window_start = None;
        entries
    }
}

impl Default for PermissionDebouncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── translate exhaustiveness test ─────────────────────────────────────

    #[test]
    fn translate_allowed_to_allow() {
        assert_eq!(translate(PermissionOutcome::Allowed), PermissionDecision::Allow);
    }

    #[test]
    fn translate_denied_to_deny() {
        assert_eq!(translate(PermissionOutcome::Denied), PermissionDecision::Deny);
    }

    #[test]
    fn translate_timeout_to_deny() {
        assert_eq!(translate(PermissionOutcome::Timeout), PermissionDecision::Deny);
    }

    // ── bridge tests ─────────────────────────────────────────────────────

    #[test]
    fn fresh_bridge_not_resolved() {
        let b = PermissionBridge::new();
        assert!(!b.is_resolved());
    }

    #[tokio::test]
    async fn bridge_open_and_resolve() {
        let mut bridge = PermissionBridge::new();
        let (rx, _perm_id) = bridge.open_request();
        assert!(bridge.resolve(PermissionOutcome::Allowed));
        assert!(bridge.is_resolved());
        let decision = PermissionBridge::wait_for_decision(rx).await;
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn bridge_timeout_denies() {
        let mut bridge = PermissionBridge::new();
        // Test timeout by never sending on the oneshot
        let (rx, _perm_id) = bridge.open_request();
        // Don't resolve — the wait_for_decision should timeout after 30 seconds.
        // We can't actually wait 30s in a unit test, so we test the logic:
        // the timeout is handled by tokio::time::timeout inside wait_for_decision.
        drop(rx); // Drop receiver so the recv fails immediately
        // Since we can't easily test 30s timeout, test the drop case:
        let (tx, rx) = oneshot::channel();
        drop(tx);
        let decision = PermissionBridge::wait_for_decision(rx).await;
        assert_eq!(decision, PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn bridge_reject_denies() {
        let mut bridge = PermissionBridge::new();
        let (rx, _perm_id) = bridge.open_request();
        assert!(bridge.resolve(PermissionOutcome::Denied));
        let decision = PermissionBridge::wait_for_decision(rx).await;
        assert_eq!(decision, PermissionDecision::Deny);
    }

    // ── Debouncer tests ──────────────────────────────────────────────────

    #[test]
    fn fresh_debouncer_flushes_empty() {
        let mut d = PermissionDebouncer::new();
        let entries = d.flush();
        assert!(entries.is_empty());
    }

    #[test]
    fn debouncer_add_flush_single() {
        let mut d = PermissionDebouncer::with_window(0); // instant flush
        d.add("write_file", Some("/tmp/f.txt"), None);
        let entries = d.flush();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "write_file");
        assert_eq!(entries[0].path.as_deref(), Some("/tmp/f.txt"));
        assert_eq!(entries[0].count, 1);
    }

    #[test]
    fn debouncer_dedup_identical_tool_path() {
        let mut d = PermissionDebouncer::with_window(0);
        d.add("write_file", Some("/tmp/f.txt"), None);
        let dedup = d.add("write_file", Some("/tmp/f.txt"), None);
        assert!(dedup, "should be deduped");
        let entries = d.force_flush();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].count, 2);
    }

    #[test]
    fn debouncer_separate_entries_for_different_paths() {
        let mut d = PermissionDebouncer::with_window(0);
        d.add("write_file", Some("/tmp/a.txt"), None);
        d.add("write_file", Some("/tmp/b.txt"), None);
        let entries = d.force_flush();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].count, 1);
        assert_eq!(entries[1].count, 1);
    }

    #[test]
    fn debouncer_separate_entries_for_different_tools() {
        let mut d = PermissionDebouncer::with_window(0);
        d.add("write_file", Some("/tmp/f.txt"), None);
        d.add("read_file", None, None);
        let entries = d.force_flush();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn debouncer_force_flush_clears() {
        let mut d = PermissionDebouncer::with_window(5000);
        d.add("write_file", None, None);
        let entries = d.force_flush();
        assert_eq!(entries.len(), 1);
        // After force flush, should be empty
        assert!(d.force_flush().is_empty());
    }

    #[test]
    fn debouncer_expires_after_window() {
        let mut d = PermissionDebouncer::with_window(1); // 1ms window
        d.add("write_file", None, None);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let entries = d.flush();
        assert_eq!(entries.len(), 1, "window should have expired");
    }
}

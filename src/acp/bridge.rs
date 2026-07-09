//! ACP EventSink bridge: translates [`AgentEvent`]s into `session/update`
//! notifications pushed through an mpsc channel.
//!
//! The server creates one `AcpBridge` per session turn, injects it as the
//! [`EventSink`] for that session's [`AgentRuntime`], and drains the
//! receiver after each `session/prompt` call to write notifications to
//! stdout.
//!
//! # MessageId strategy (S2-C6, S2-C8)
//!
//! The bridge buffers [`AgentEvent::PartialToken`] deltas internally.
//! When [`AgentEvent::TurnFinished`] arrives, it computes a content-hash
//! `messageId` (SHA-256 first 12 hex chars of the *full* accumulated text)
//! and emits every buffered chunk with the *same* `messageId`.  The final
//! notification carries `stopReason`, `messageId`, and the completed
//! message content.  This satisfies:
//!
//! - **S2-C6**: all chunks share one `messageId`.
//! - **S2-C8**: `messageId` is a stable content hash.
//! - **S2-C7**: the final notification includes the completed message.
//!
//! # Tool call notifications (Sprint 3)
//!
//! Tool calls follow the lifecycle:
//!   1. `tool_call` with status `pending` (inc. locations)
//!   2. `tool_call_update` with status `in_progress` (synthesised by bridge)
//!   3. `tool_call_update` with status `completed` or `failed` (on ToolResult)
//!
//! Per-call state tracking supports concurrent tool calls arriving and
//! completing out-of-order (S3-C7).

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::acp::ToolKind;
use crate::event::{AgentEvent, EventSink};
use serde_json::Value;

// ---------------------------------------------------------------------------
// SHA-256 helper
// ---------------------------------------------------------------------------

/// Compute the SHA-256 hash of `text` and return the first 12 hex characters.
/// This is stable: identical input always produces the same output.
pub fn sha256_first_12(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let result = hasher.finalize();
    // First 6 bytes → 12 hex chars
    hex_encode(&result[..6])
}

/// Encode a byte slice as lowercase hex.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// ToolCallState — per-call tracking
// ---------------------------------------------------------------------------

/// Tracks the lifecycle state of one in-flight tool call so the bridge can
/// emit the correct sequence of notifications and handle out-of-order results.
#[derive(Debug)]
#[allow(dead_code)]
struct ToolCallState {
    /// The tool name (used as `title` in notifications).
    name: String,
    /// The ACP tool kind for this call.
    kind: ToolKind,
}

// ---------------------------------------------------------------------------
// extract_locations (S3-C5)
// ---------------------------------------------------------------------------

/// Extract `ToolCallLocation` entries from tool arguments.
///
/// - Read    → `path`
/// - Edit    → `file_path` + optional `start_line`
/// - Write   → `file_path`
/// - Glob    → `pattern`
/// - WebFetch → `url`
/// - Bash    → `cwd` when present
///
/// Malformed or missing arguments produce an empty `Vec` (never panics).
pub fn extract_locations(tool_name: &str, arguments: &Value) -> Vec<Value> {
    match tool_name {
        "Read" => {
            let mut locs = Vec::new();
            if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                locs.push(serde_json::json!({"path": path}));
            }
            locs
        }
        "Edit" => {
            let mut locs = Vec::new();
            if let Some(file_path) = arguments.get("file_path").and_then(|v| v.as_str()) {
                let mut loc = serde_json::json!({"path": file_path});
                if let Some(line) = arguments.get("start_line").and_then(|v| v.as_u64()) {
                    loc["line"] = serde_json::json!(line);
                }
                locs.push(loc);
            }
            locs
        }
        "Write" => {
            let mut locs = Vec::new();
            if let Some(file_path) = arguments.get("file_path").and_then(|v| v.as_str()) {
                locs.push(serde_json::json!({"path": file_path}));
            }
            locs
        }
        "Glob" => {
            let mut locs = Vec::new();
            if let Some(pattern) = arguments.get("pattern").and_then(|v| v.as_str()) {
                locs.push(serde_json::json!({"path": pattern}));
            }
            locs
        }
        "WebFetch" => {
            let mut locs = Vec::new();
            if let Some(url) = arguments.get("url").and_then(|v| v.as_str()) {
                locs.push(serde_json::json!({"path": url}));
            }
            locs
        }
        "Bash" => {
            let mut locs = Vec::new();
            if let Some(cwd) = arguments.get("cwd").and_then(|v| v.as_str()) {
                locs.push(serde_json::json!({"path": cwd}));
            }
            locs
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// AcpBridge
// ---------------------------------------------------------------------------

/// An [`EventSink`] that translates agent events into ACP `session/update`
/// JSON-RPC notification [`Value`]s and pushes them into an mpsc channel.
///
/// The receiver half is drained by the server's dispatch after each
/// `session/prompt` completes.
///
/// **Buffering contract:** text deltas are buffered internally; all
/// notifications (chunks + final) are emitted only when
/// [`AgentEvent::TurnFinished`] arrives, so every chunk can carry the
/// stable, content-derived `messageId`.
pub struct AcpBridge {
    session_id: String,
    tx: mpsc::UnboundedSender<Value>,
    /// Buffered [(delta_text, step)] tuples from `PartialToken` events.
    partials: tokio::sync::Mutex<Vec<(String, usize)>>,
    /// Tool name → ToolKind mapping (S3-C4). Used to populate the `kind`
    /// field in `tool_call` notifications.
    kind_map: HashMap<String, ToolKind>,
    /// Per-call state for in-flight tool calls (S3-C7). Keyed by toolCallId.
    pending_calls: tokio::sync::Mutex<HashMap<String, ToolCallState>>,
    /// Turn counter used for `turnId` in `end_turn` notifications.
    turn: u64,
    /// Sprint-3 gate: when false, tool_call / tool_call_update notifications
    /// are suppressed (Sprint 2 contract: only agent_message_chunk + end_turn).
    tool_call_notifications_enabled: bool,
}

impl AcpBridge {
    /// Create a new bridge and return it together with the receiver half.
    ///
    /// `kind_map` maps tool registry names to their ACP `ToolKind` values,
    /// populated from `ToolRegistry::build_kind_map()` at bridge construction
    /// time (S3-C4).
    pub fn new(
        session_id: String,
        kind_map: HashMap<String, ToolKind>,
        turn: u64,
        tool_call_notifications_enabled: bool,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = Arc::new(Self {
            session_id,
            tx,
            partials: tokio::sync::Mutex::new(Vec::new()),
            kind_map,
            pending_calls: tokio::sync::Mutex::new(HashMap::new()),
            turn,
            tool_call_notifications_enabled,
        });
        (bridge, rx)
    }
}

#[async_trait::async_trait]
impl EventSink for AcpBridge {
    async fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::PartialToken { text, step } => {
                // Buffer; don't emit yet — we need the full text for messageId.
                let mut partials = self.partials.lock().await;
                partials.push((text, step));
            }
            AgentEvent::AssistantText { text, step } => {
                // Non-streaming fallback: treat the full text as a single chunk.
                let mut partials = self.partials.lock().await;
                if partials.is_empty() {
                    partials.push((text, step));
                }
            }
            AgentEvent::TurnFinished { reason, steps: _ } => {
                // 1. Drain buffered deltas and compute the full text.
                let partials: Vec<(String, usize)> = {
                    let mut guard = self.partials.lock().await;
                    std::mem::take(&mut *guard)
                };

                let full_text: String = partials.iter().map(|(t, _)| t.as_str()).collect();
                let message_id = sha256_first_12(&full_text);

                // 2. Emit one agent_message_chunk per buffered delta,
                //    all sharing the same content-hash messageId.
                for (delta, _step) in &partials {
                    let notif = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": self.session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {
                                    "type": "text",
                                    "text": delta,
                                },
                                "messageId": message_id,
                            }
                        }
                    });
                    let _ = self.tx.send(notif);
                }

                // 3. Emit the terminal end_turn notification with stopReason,
                //    messageId, completed content, and turnId.
                let stop_reason = match reason.as_str() {
                    "no_more_tool_calls" => "end_turn",
                    "cancelled" => "cancelled",
                    "budget_exceeded" => "max_turns",
                    s if s.starts_with("stuck:")
                        || s.starts_with("transcript_limit:")
                        || s.starts_with("provider_stop:")
                        || s == "permission_denial_limit" =>
                    {
                        "error"
                    }
                    _ => "end_turn",
                };

                let mut notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": self.session_id,
                        "update": {
                            "sessionUpdate": "end_turn",
                            "stopReason": stop_reason,
                            "messageId": message_id,
                            "turnId": self.turn.to_string(),
                        }
                    }
                });

                if !full_text.is_empty() {
                    notif["params"]["update"]["content"] = serde_json::json!({
                        "type": "text",
                        "text": full_text,
                    });
                }

                let _ = self.tx.send(notif);
            }

            // ── Sprint 3: Tool call notifications (S3-C1, S3-C2, S3-C6, S3-C7, S3-C8) ──
            AgentEvent::ToolCall {
                name,
                id,
                arguments,
                step: _step,
            } => {
                // Sprint 2 contract: suppress tool_call notifications.
                if !self.tool_call_notifications_enabled {
                    return;
                }
                // Parse arguments to extract locations (S3-C5).
                let args: Value = serde_json::from_str(&arguments).unwrap_or_default();
                let kind = self.kind_map.get(&name).copied().unwrap_or(ToolKind::Other);
                let locations = extract_locations(&name, &args);

                // Register per-call state (S3-C7).
                {
                    let mut pending = self.pending_calls.lock().await;
                    pending.insert(
                        id.clone(),
                        ToolCallState {
                            name: name.clone(),
                            kind,
                        },
                    );
                }

                // 1. Emit pending tool_call notification with locations (S3-C1, S3-C6).
                let mut pending_notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": self.session_id,
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": id,
                            "title": name,
                            "kind": kind,
                            "status": "pending",
                        }
                    }
                });
                if !locations.is_empty() {
                    pending_notif["params"]["update"]["locations"] = serde_json::json!(locations);
                }
                let _ = self.tx.send(pending_notif);

                // 2. Synthesise in_progress tool_call_update (S3-C2).
                let in_progress_notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": self.session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": id,
                            "status": "in_progress",
                        }
                    }
                });
                let _ = self.tx.send(in_progress_notif);
            }

            AgentEvent::ToolResult {
                id,
                name: _name,
                output,
                step: _step,
                is_error,
            } => {
                // Sprint 2 contract: suppress tool_call_update notifications.
                if !self.tool_call_notifications_enabled {
                    return;
                }
                // Look up (and remove) per-call state so concurrent calls don't
                // interfere (S3-C7).
                let _state = {
                    let mut pending = self.pending_calls.lock().await;
                    pending.remove(&id)
                };

                // 3. Emit completed or failed tool_call_update (S3-C2).
                let status = if is_error { "failed" } else { "completed" };
                let mut result_notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": self.session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": id,
                            "status": status,
                        }
                    }
                });

                // Include content if there is output.
                if !output.is_empty() {
                    result_notif["params"]["update"]["content"] = serde_json::json!({
                        "type": "content",
                        "content": {
                            "type": "text",
                            "text": output,
                        }
                    });
                }
                let _ = self.tx.send(result_notif);
            }

            // Silence other events: Latency, Usage, PlanMode*, etc.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── SHA-256 ──────────────────────────────────────────────────────────

    #[test]
    fn sha256_first_12_known_value() {
        let result = sha256_first_12("Hello, world!");
        assert_eq!(result, "315f5bdb76d0");
    }

    #[test]
    fn sha256_first_12_consistent() {
        let a = sha256_first_12("test message");
        let b = sha256_first_12("test message");
        assert_eq!(a, b, "same input must produce same output");
    }

    #[test]
    fn sha256_first_12_different_inputs_differ() {
        let a = sha256_first_12("hello");
        let b = sha256_first_12("world");
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_first_12_empty_string() {
        let result = sha256_first_12("");
        assert_eq!(result, "e3b0c44298fc");
    }

    // ── extract_locations (S3-C5) ────────────────────────────────────────

    fn make_bridge(
        kind_map: HashMap<String, ToolKind>,
    ) -> (Arc<AcpBridge>, mpsc::UnboundedReceiver<Value>) {
        AcpBridge::new("sess-1".into(), kind_map, 1, true)
    }

    fn default_kind_map() -> HashMap<String, ToolKind> {
        HashMap::new()
    }

    #[test]
    fn extract_locations_read_extracts_path() {
        let locs = extract_locations("Read", &serde_json::json!({"path": "/tmp/f.txt"}));
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/tmp/f.txt");
    }

    #[test]
    fn extract_locations_edit_extracts_file_path() {
        let locs = extract_locations(
            "Edit",
            &serde_json::json!({"file_path": "/tmp/f.txt", "old_string": "x", "new_string": "y"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/tmp/f.txt");
    }

    #[test]
    fn extract_locations_edit_with_start_line() {
        let locs = extract_locations(
            "Edit",
            &serde_json::json!({"file_path": "/tmp/f.txt", "start_line": 42, "old_string": "x", "new_string": "y"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/tmp/f.txt");
        assert_eq!(locs[0]["line"], 42);
    }

    #[test]
    fn extract_locations_write_extracts_file_path() {
        let locs = extract_locations(
            "Write",
            &serde_json::json!({"file_path": "/tmp/new.txt", "contents": "hello"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/tmp/new.txt");
    }

    #[test]
    fn extract_locations_glob_extracts_pattern() {
        let locs = extract_locations("Glob", &serde_json::json!({"pattern": "/src/**/*.rs"}));
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/src/**/*.rs");
    }

    #[test]
    fn extract_locations_web_fetch_extracts_url() {
        let locs = extract_locations(
            "WebFetch",
            &serde_json::json!({"url": "https://example.com"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "https://example.com");
    }

    #[test]
    fn extract_locations_bash_with_cwd() {
        let locs = extract_locations(
            "Bash",
            &serde_json::json!({"command": "ls", "cwd": "/tmp/work"}),
        );
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "/tmp/work");
    }

    #[test]
    fn extract_locations_bash_without_cwd() {
        let locs = extract_locations("Bash", &serde_json::json!({"command": "ls"}));
        assert!(locs.is_empty());
    }

    #[test]
    fn extract_locations_malformed_args_no_panic() {
        let locs = extract_locations("Read", &serde_json::json!({}));
        assert!(locs.is_empty());
        let locs = extract_locations("Read", &serde_json::json!({"path": 42}));
        assert!(locs.is_empty());
    }

    #[test]
    fn extract_locations_unknown_tool_returns_empty() {
        let locs = extract_locations("UnknownTool", &serde_json::json!({"some_arg": "value"}));
        assert!(locs.is_empty());
    }

    // ── Bridge: text buffering (Sprint 2 regression) ─────────────────────

    #[tokio::test]
    async fn bridge_buffers_partial_tokens_and_emits_on_turn_finished() {
        let (bridge, mut rx) = make_bridge(default_kind_map());

        bridge
            .emit(AgentEvent::PartialToken {
                text: "Hello".into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::PartialToken {
                text: ", world!".into(),
                step: 0,
            })
            .await;

        // No notifications yet
        assert!(rx.try_recv().is_err(), "partials are buffered");

        bridge
            .emit(AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 3,
            })
            .await;

        // First chunk: "Hello"
        let notif1 = rx.try_recv().expect("chunk 1");
        assert_eq!(notif1["method"], "session/update");
        assert_eq!(notif1["params"]["sessionId"], "sess-1");
        assert_eq!(
            notif1["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(notif1["params"]["update"]["content"]["type"], "text");
        assert_eq!(notif1["params"]["update"]["content"]["text"], "Hello");

        // Second chunk: ", world!"
        let notif2 = rx.try_recv().expect("chunk 2");
        assert_eq!(notif2["params"]["update"]["content"]["text"], ", world!");

        // Both chunks share the same messageId (S2-C6)
        let mid1 = notif1["params"]["update"]["messageId"]
            .as_str()
            .unwrap()
            .to_string();
        let mid2 = notif2["params"]["update"]["messageId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(mid1, mid2, "all chunks must share same messageId");
        assert_eq!(mid1, "315f5bdb76d0");

        // Final notification: stopReason + messageId + completed content
        let notif3 = rx.try_recv().expect("final notification");
        assert_eq!(notif3["method"], "session/update");
        assert_eq!(notif3["params"]["update"]["stopReason"], "end_turn");
        assert_eq!(
            notif3["params"]["update"]["messageId"].as_str().unwrap(),
            "315f5bdb76d0"
        );
        assert_eq!(
            notif3["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap(),
            "Hello, world!"
        );

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn bridge_emits_stop_reason_on_turn_finished_empty() {
        let (bridge, mut rx) = make_bridge(default_kind_map());
        bridge
            .emit(AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 3,
            })
            .await;

        let notif = rx.try_recv().expect("should receive notification");
        assert_eq!(notif["method"], "session/update");
        assert_eq!(notif["params"]["update"]["stopReason"], "end_turn");
        assert_eq!(
            notif["params"]["update"]["messageId"].as_str().unwrap(),
            "e3b0c44298fc"
        );
        assert!(
            notif["params"]["update"]["content"].is_null()
                || notif["params"]["update"].get("content").is_none()
        );
    }

    #[tokio::test]
    async fn bridge_message_id_is_content_hash_of_full_text() {
        let (bridge, mut rx) = make_bridge(default_kind_map());
        bridge
            .emit(AgentEvent::PartialToken {
                text: "Hello".into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::PartialToken {
                text: ", world!".into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 2,
            })
            .await;

        let _n1 = rx.try_recv().unwrap();
        let _n2 = rx.try_recv().unwrap();
        let n3 = rx.try_recv().unwrap();

        let mid = n3["params"]["update"]["messageId"].as_str().unwrap();
        assert_eq!(mid, "315f5bdb76d0");
        assert_eq!(
            n3["params"]["update"]["content"]["text"].as_str().unwrap(),
            "Hello, world!"
        );
    }

    #[tokio::test]
    async fn bridge_message_id_stable_across_sessions_same_content() {
        let (bridge1, mut rx1) = make_bridge(default_kind_map());
        let (bridge2, mut rx2) = make_bridge(default_kind_map());

        for bridge in [&bridge1, &bridge2] {
            bridge
                .emit(AgentEvent::PartialToken {
                    text: "Hello".into(),
                    step: 0,
                })
                .await;
            bridge
                .emit(AgentEvent::TurnFinished {
                    reason: "no_more_tool_calls".into(),
                    steps: 2,
                })
                .await;
        }

        let _c1 = rx1.try_recv().unwrap();
        let f1 = rx1.try_recv().unwrap();
        let _c2 = rx2.try_recv().unwrap();
        let f2 = rx2.try_recv().unwrap();

        assert_eq!(
            f1["params"]["update"]["messageId"], f2["params"]["update"]["messageId"],
            "identical content must produce identical messageId across sessions"
        );
    }

    #[tokio::test]
    async fn bridge_assistant_text_fallback() {
        let (bridge, mut rx) = make_bridge(default_kind_map());
        bridge
            .emit(AgentEvent::AssistantText {
                text: "full response".into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 1,
            })
            .await;

        let chunk = rx.try_recv().expect("chunk");
        assert_eq!(
            chunk["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap(),
            "full response"
        );
        assert_eq!(
            chunk["params"]["update"]["messageId"].as_str().unwrap(),
            sha256_first_12("full response")
        );

        let final_notif = rx.try_recv().expect("final");
        assert_eq!(final_notif["params"]["update"]["stopReason"], "end_turn");
        assert_eq!(
            final_notif["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap(),
            "full response"
        );
    }

    // ── Sprint 3: Tool call notifications ────────────────────────────────

    #[tokio::test]
    async fn tool_call_emits_pending_with_kind_and_locations() {
        // S3-C1: ToolCall → pending tool_call notification with kind + locations.
        let mut km = HashMap::new();
        km.insert("Read".to_string(), ToolKind::Read);
        let (bridge, mut rx) = make_bridge(km);

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Read".into(),
                id: "tc-1".into(),
                arguments: r#"{"path":"/tmp/f.txt"}"#.into(),
                step: 0,
            })
            .await;

        // First: pending tool_call (S3-C1)
        let notif = rx.try_recv().expect("pending tool_call");
        assert_eq!(notif["method"], "session/update");
        assert_eq!(notif["params"]["sessionId"], "sess-1");
        assert_eq!(notif["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(notif["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(notif["params"]["update"]["title"], "Read");
        assert_eq!(notif["params"]["update"]["kind"], "read");
        assert_eq!(notif["params"]["update"]["status"], "pending");
        // S3-C6: locations present in pending notification
        assert_eq!(
            notif["params"]["update"]["locations"][0]["path"],
            "/tmp/f.txt"
        );

        // Second: in_progress (synthesised)
        let notif2 = rx.try_recv().expect("in_progress tool_call_update");
        assert_eq!(
            notif2["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(notif2["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(notif2["params"]["update"]["status"], "in_progress");
        // S3-C6: in_progress has NO locations
        assert!(notif2["params"]["update"].get("locations").is_none());
    }

    #[tokio::test]
    async fn tool_call_unknown_tool_uses_other_kind() {
        // S3-C4: bridge with empty kind_map uses "other"
        let (bridge, mut rx) = make_bridge(default_kind_map());

        bridge
            .emit(AgentEvent::ToolCall {
                name: "UnknownTool".into(),
                id: "tc-x".into(),
                arguments: "{}".into(),
                step: 0,
            })
            .await;

        let notif = rx.try_recv().expect("pending");
        assert_eq!(notif["params"]["update"]["kind"], "other");
    }

    #[tokio::test]
    async fn tool_call_lifecycle_pending_in_progress_completed() {
        // S3-C2: full lifecycle pending → in_progress → completed
        let mut km = HashMap::new();
        km.insert("Read".to_string(), ToolKind::Read);
        let (bridge, mut rx) = make_bridge(km);

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Read".into(),
                id: "tc-1".into(),
                arguments: r#"{"path":"/tmp/f.txt"}"#.into(),
                step: 0,
            })
            .await;

        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-1".into(),
                name: "Read".into(),
                output: "file contents".into(),
                step: 0,
                is_error: false,
            })
            .await;

        // 1. pending
        let n1 = rx.try_recv().expect("pending");
        assert_eq!(n1["params"]["update"]["status"], "pending");
        assert_eq!(n1["params"]["update"]["sessionUpdate"], "tool_call");

        // 2. in_progress
        let n2 = rx.try_recv().expect("in_progress");
        assert_eq!(n2["params"]["update"]["status"], "in_progress");
        assert_eq!(n2["params"]["update"]["sessionUpdate"], "tool_call_update");

        // 3. completed with content
        let n3 = rx.try_recv().expect("completed");
        assert_eq!(n3["params"]["update"]["status"], "completed");
        assert_eq!(n3["params"]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(n3["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(n3["params"]["update"]["content"]["type"], "content");
        assert_eq!(
            n3["params"]["update"]["content"]["content"]["text"],
            "file contents"
        );

        // S3-C6: completed has NO locations
        assert!(n3["params"]["update"].get("locations").is_none());
    }

    #[tokio::test]
    async fn tool_call_lifecycle_failed_on_is_error() {
        // S3-C2: is_error=true → status "failed"
        let mut km = HashMap::new();
        km.insert("Bash".to_string(), ToolKind::Execute);
        let (bridge, mut rx) = make_bridge(km);

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Bash".into(),
                id: "tc-err".into(),
                arguments: r#"{"command":"bad"}"#.into(),
                step: 0,
            })
            .await;

        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-err".into(),
                name: "Bash".into(),
                output: "command not found".into(),
                step: 0,
                is_error: true,
            })
            .await;

        let _n1 = rx.try_recv().expect("pending");
        let _n2 = rx.try_recv().expect("in_progress");

        let n3 = rx.try_recv().expect("failed");
        assert_eq!(n3["params"]["update"]["status"], "failed");
        assert_eq!(n3["params"]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(n3["params"]["update"]["toolCallId"], "tc-err");
        assert_eq!(
            n3["params"]["update"]["content"]["content"]["text"],
            "command not found"
        );
    }

    #[tokio::test]
    async fn concurrent_tool_calls_independent_lifecycle() {
        // S3-C7: Two parallel tool calls with out-of-order results.
        let mut km = HashMap::new();
        km.insert("Read".to_string(), ToolKind::Read);
        km.insert("Bash".to_string(), ToolKind::Execute);
        let (bridge, mut rx) = make_bridge(km);

        // Emit two ToolCalls back-to-back
        bridge
            .emit(AgentEvent::ToolCall {
                name: "Read".into(),
                id: "tc-1".into(),
                arguments: r#"{"path":"/tmp/a.txt"}"#.into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::ToolCall {
                name: "Bash".into(),
                id: "tc-2".into(),
                arguments: r#"{"command":"echo hi"}"#.into(),
                step: 0,
            })
            .await;

        // Results arrive out of order (tc-2 first, tc-1 second)
        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-2".into(),
                name: "Bash".into(),
                output: "hi".into(),
                step: 0,
                is_error: false,
            })
            .await;
        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-1".into(),
                name: "Read".into(),
                output: "content".into(),
                step: 0,
                is_error: true,
            })
            .await;

        // Expected order: tc-1 pending, tc-1 in_progress, tc-2 pending, tc-2 in_progress,
        //                 tc-2 completed, tc-1 failed
        let n1 = rx.try_recv().expect("tc-1 pending");
        assert_eq!(n1["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(n1["params"]["update"]["status"], "pending");

        let n2 = rx.try_recv().expect("tc-1 in_progress");
        assert_eq!(n2["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(n2["params"]["update"]["status"], "in_progress");

        let n3 = rx.try_recv().expect("tc-2 pending");
        assert_eq!(n3["params"]["update"]["toolCallId"], "tc-2");
        assert_eq!(n3["params"]["update"]["status"], "pending");

        let n4 = rx.try_recv().expect("tc-2 in_progress");
        assert_eq!(n4["params"]["update"]["toolCallId"], "tc-2");
        assert_eq!(n4["params"]["update"]["status"], "in_progress");

        let n5 = rx.try_recv().expect("tc-2 completed");
        assert_eq!(n5["params"]["update"]["toolCallId"], "tc-2");
        assert_eq!(n5["params"]["update"]["status"], "completed");

        let n6 = rx.try_recv().expect("tc-1 failed");
        assert_eq!(n6["params"]["update"]["toolCallId"], "tc-1");
        assert_eq!(n6["params"]["update"]["status"], "failed");
    }

    #[tokio::test]
    async fn non_tool_events_are_silenced() {
        // S3-C8: Latency, Usage events produce no notifications.
        let (bridge, mut rx) = make_bridge(default_kind_map());

        bridge
            .emit(AgentEvent::Latency {
                step: 0,
                llm_ms: 100,
            })
            .await;
        bridge
            .emit(AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
                step: 0,
            })
            .await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tool_call_without_locations() {
        // ToolCall with no path-like args → no locations in pending
        let mut km = HashMap::new();
        km.insert("Bash".to_string(), ToolKind::Execute);
        let (bridge, mut rx) = make_bridge(km);

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Bash".into(),
                id: "tc-noloc".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                step: 0,
            })
            .await;

        let n1 = rx.try_recv().expect("pending");
        assert_eq!(n1["params"]["update"]["toolCallId"], "tc-noloc");
        assert!(n1["params"]["update"]
            .get("locations")
            .map_or(true, |l| l.is_null()
                || l.as_array().map_or(true, |a| a.is_empty())));
    }

    #[tokio::test]
    async fn tool_result_empty_output_no_content_block() {
        // ToolResult with empty output → no content field in notification.
        let (bridge, mut rx) = make_bridge(default_kind_map());

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Read".into(),
                id: "tc-empty".into(),
                arguments: "{}".into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-empty".into(),
                name: "Read".into(),
                output: "".into(),
                step: 0,
                is_error: false,
            })
            .await;

        let _n1 = rx.try_recv().expect("pending");
        let _n2 = rx.try_recv().expect("in_progress");
        let n3 = rx.try_recv().expect("completed");
        assert_eq!(n3["params"]["update"]["status"], "completed");
        assert!(
            n3["params"]["update"].get("content").is_none()
                || n3["params"]["update"]["content"].is_null()
        );
    }

    #[tokio::test]
    async fn pending_notification_arrives_before_turn_finished() {
        // S3-C1: tool_call pending notification arrives before TurnFinished.
        let mut km = HashMap::new();
        km.insert("Read".to_string(), ToolKind::Read);
        let (bridge, mut rx) = make_bridge(km);

        bridge
            .emit(AgentEvent::ToolCall {
                name: "Read".into(),
                id: "tc-before".into(),
                arguments: r#"{"path":"/tmp/f.txt"}"#.into(),
                step: 0,
            })
            .await;
        bridge
            .emit(AgentEvent::ToolResult {
                id: "tc-before".into(),
                name: "Read".into(),
                output: "data".into(),
                step: 0,
                is_error: false,
            })
            .await;
        bridge
            .emit(AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 1,
            })
            .await;

        // First notification must be the pending tool_call
        let first = rx.try_recv().expect("first");
        assert_eq!(first["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(first["params"]["update"]["status"], "pending");

        // Drain remaining tool notifications
        let _in_prog = rx.try_recv().expect("in_progress");
        let _completed = rx.try_recv().expect("completed");

        // Final notification: either an agent_message_chunk (with stopReason=end_turn)
        // or a SessionUpdate::End_turn notification (carries stopReason at top level).
        // The bridge's exact sequence depends on whether TurnFinished synthesises a
        // chunk; accept either form.
        let final_notif = rx.try_recv().expect("turn finished");
        let update = &final_notif["params"]["update"];
        let session_update = update["sessionUpdate"].as_str().unwrap_or("");
        assert!(
            session_update == "agent_message_chunk" || session_update == "end_turn",
            "unexpected final sessionUpdate: {session_update}"
        );
        assert_eq!(update["stopReason"], "end_turn");

        assert!(rx.try_recv().is_err());
    }
}

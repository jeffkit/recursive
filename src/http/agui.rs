//! AG-UI server-side session layer (Issue #56).
//!
//! Everything AG-UI lives here, not in `handlers.rs`: the AgentEvent→Event
//! converter, the thread↔session-directory mapping, the persisted
//! open-interrupt store, the client-tool / test-interrupt permission hooks,
//! and the resume/interrupt state machine (input parsing → seed transcript).
//!
//! The only axum-aware piece is [`super::handlers::agui_run`], which parses
//! the JSON body, calls [`prepare_run`] (this module, HTTP-free), drives the
//! runtime, and maps the event stream onto SSE frames. All logic below is
//! unit-testable without an HTTP server.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::event::AgentEvent;

// ── AgentEvent → AG-UI Event converter ────────────────────────────────────

/// State machine that the AG-UI converter uses to coordinate
/// `TextMessageStart/Content/End` framing across multiple AgentEvents.
///
/// We open a TextMessage on the first `AssistantText`/`PartialToken` we see
/// after every "neutral" point (run start, after `TextMessageEnd`, after
/// tool-call events) and close it explicitly when we emit a fully-formed
/// `AssistantText`, when a `ToolCall` arrives, or when the run finishes.
#[derive(Default)]
pub(crate) struct AguiConverter {
    /// `Some(message_id)` when a TextMessageStart has been emitted but no
    /// TextMessageEnd yet. Used as the `messageId` for streaming
    /// `PartialToken` deltas and as the `parentMessageId` for tool calls.
    open_message_id: Option<String>,
    /// Last fully-emitted (or currently-open) assistant message id. Used as
    /// the `parent_message_id` on ToolCallStart even after the message has
    /// been closed, so a client can attribute the tool call back to the
    /// triggering assistant turn.
    last_assistant_message_id: Option<String>,
}

impl AguiConverter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Translate one [`AgentEvent`] into zero or more AG-UI events,
    /// updating internal framing state as a side effect.
    pub(crate) fn convert(&mut self, ev: &AgentEvent) -> Vec<agui_protocol::Event> {
        use agui_protocol as ag;
        let mut out = Vec::new();
        match ev {
            AgentEvent::AssistantText { text, .. } => {
                // Close any in-flight streamed message first.
                if let Some(id) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: id,
                        base: ag::BaseEvent::default(),
                    }));
                }
                let id = uuid::Uuid::new_v4().to_string();
                out.push(ag::Event::TextMessageStart(ag::TextMessageStart {
                    message_id: id.clone(),
                    role: Some("assistant".into()),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::TextMessageContent(ag::TextMessageContent {
                    message_id: id.clone(),
                    delta: text.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                    message_id: id.clone(),
                    base: ag::BaseEvent::default(),
                }));
                self.last_assistant_message_id = Some(id);
                self.open_message_id = None;
            }
            AgentEvent::PartialToken { text, .. } => {
                let id = if let Some(id) = self.open_message_id.clone() {
                    id
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    out.push(ag::Event::TextMessageStart(ag::TextMessageStart {
                        message_id: id.clone(),
                        role: Some("assistant".into()),
                        base: ag::BaseEvent::default(),
                    }));
                    self.open_message_id = Some(id.clone());
                    self.last_assistant_message_id = Some(id.clone());
                    id
                };
                out.push(ag::Event::TextMessageContent(ag::TextMessageContent {
                    message_id: id,
                    delta: text.clone(),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                // Close any in-flight streamed assistant message first; the
                // assistant turn is "done" the moment a tool call lands.
                if let Some(open) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: open,
                        base: ag::BaseEvent::default(),
                    }));
                }
                out.push(ag::Event::ToolCallStart(ag::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                    parent_message_id: self.last_assistant_message_id.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::ToolCallArgs(ag::ToolCallArgs {
                    tool_call_id: id.clone(),
                    delta: arguments.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::ToolCallEnd(ag::ToolCallEnd {
                    tool_call_id: id.clone(),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::ToolResult { id, output, .. } => {
                // AG-UI requires a `messageId` on ToolCallResult; reuse the
                // most recent assistant message id as the conversational
                // anchor (mirrors what OpenAI's tool message shape does).
                let message_id = self
                    .last_assistant_message_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                out.push(ag::Event::ToolCallResult(ag::ToolCallResult {
                    tool_call_id: id.clone(),
                    message_id,
                    content: output.clone(),
                    role: Some("tool".into()),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::TurnFinished { .. } => {
                // Close any in-flight streamed message before signalling
                // run completion to the client.
                if let Some(open) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: open,
                        base: ag::BaseEvent::default(),
                    }));
                }
                // Actual RunFinished is emitted by the caller (it knows
                // the thread/run ids); we just flush state here.
            }
            // Hook lifecycle events: forward as Custom so AG-UI clients can
            // render hook progress and system messages in real time.
            AgentEvent::HookStarted {
                hook_event,
                hook_name,
                status_message,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_started".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "statusMessage": status_message,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookProgress {
                hook_event,
                hook_name,
                last_line,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_progress".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "lastLine": last_line,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookFinished {
                hook_event,
                hook_name,
                outcome,
                duration_ms,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_finished".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "outcome": outcome,
                        "durationMs": duration_ms,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookSystemMessage { text, .. } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_system_message".into(),
                    value: serde_json::json!({ "text": text }),
                    base: ag::BaseEvent::default(),
                }));
            }
            // Task checklist updates: forward so clients can render live todo state.
            AgentEvent::TodoUpdated { todos, .. } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/todo_updated".into(),
                    value: serde_json::json!({ "todos": todos }),
                    base: ag::BaseEvent::default(),
                }));
            }
            // checkpoint_post is emitted directly by the driver task (not via
            // AguiConverter) after RunFinished so it lands last.
            // heartbeat is emitted as an SSE comment at the HTTP layer.
            // permission_request and file_artifact require new AgentEvent variants
            // (tracked as g141/g140) before they can be mapped here.
            // Other variants (Latency, Usage, Compacted, PlanProposed,
            // PlanConfirmed, PlanRejected, etc.) have no AG-UI standard
            // equivalent and are intentionally dropped.
            _ => {}
        }
        out
    }
}

/// Stateless wrapper: maps a single [`AgentEvent`] to AG-UI events
/// using a fresh converter. Useful in tests; production code uses
/// [`AguiConverter::convert`] directly so framing state survives
/// across the whole run.
#[cfg(test)]
pub(crate) fn agui_events_for(ev: &AgentEvent) -> Vec<agui_protocol::Event> {
    AguiConverter::new().convert(ev)
}

// ── Thread ↔ session directory mapping ────────────────────────────────────

/// Map an arbitrary AG-UI thread id onto a checkpoint session id that
/// satisfies `validate_session_id` in the checkpoint module
/// (alphanumerics + `-` `_` `.`, no leading dot, no `..`, no path
/// separators). Disallowed chars become `-`.
pub(crate) fn sanitize_thread_id_for_session(thread: &str) -> String {
    let mut out: String = thread
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Drop a leading dot so we don't produce a hidden dir.
    while out.starts_with('.') {
        out.replace_range(..1, "-");
    }
    // Collapse `..` so we don't produce ref-traversal sequences.
    while out.contains("..") {
        out = out.replace("..", "-.");
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

/// Path to the JSONL session directory for an AG-UI thread.
pub(crate) fn agui_session_dir(workspace: &Path, thread_id: &str) -> Option<PathBuf> {
    let session_id = sanitize_thread_id_for_session(thread_id);
    crate::user_sessions_dir(workspace)
        .ok()
        .map(|d| d.join(format!("agui-{session_id}")))
}

// ── Open-interrupt persistence (resume state machine's backing store) ─────

/// One open interrupt persisted in the session metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OpenInterrupt {
    pub interrupt_id: String,
    pub tool_call_id: String,
    pub reason: String,
    pub message: Option<String>,
    pub created_at: String,
}

/// Load open interrupts from session metadata.
pub(crate) fn load_open_interrupts(session_dir: &Path) -> Vec<OpenInterrupt> {
    let path = session_dir.join(".interrupts.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Save open interrupts to session metadata. Written atomically before
/// emitting RunFinished with Interrupt outcome.
pub(crate) fn save_open_interrupts(session_dir: &Path, interrupts: &[OpenInterrupt]) {
    if let Ok(json) = serde_json::to_string_pretty(interrupts) {
        let path = session_dir.join(".interrupts.json");
        let _ = std::fs::create_dir_all(session_dir);
        crate::atomic::atomic_write(&path, json.as_bytes()).ok();
    }
}

/// Clear open interrupts (called after a successful resume that consumed
/// all pending interrupts).
pub(crate) fn clear_open_interrupts(session_dir: &Path) {
    let path = session_dir.join(".interrupts.json");
    let _ = std::fs::remove_file(&path);
}

/// Which interrupt mechanism fired during an AG-UI run.
#[derive(Debug, Clone)]
pub(crate) enum AguiInterruptDetail {
    Test {
        tool_call_id: String,
        tool_name: String,
    },
    Client {
        tool_call_id: String,
        tool_name: String,
        parameters: serde_json::Value,
        args: serde_json::Value,
    },
}

// ── Interrupt / client-tool permission hooks ──────────────────────────────

/// Test-only permission hook: denies tools whose names appear in
/// `interrupt_before`, records the first such denial as an 'interrupt'.
/// This is the test-only trigger — the real permission_pipeline.Ask
/// integration is g325.
pub(crate) struct TestInterruptHook {
    pub interrupt_before: Vec<String>,
    /// Set to the first tool_call_id that was denied (if any).
    pub interrupted_tool_call_id: Mutex<Option<String>>,
    /// Set to the name of the first tool that was denied.
    pub interrupted_tool_name: Mutex<Option<String>>,
    /// Set to the arguments of the first tool that was denied.
    pub interrupted_arguments: Mutex<Option<serde_json::Value>>,
}

#[async_trait]
impl crate::tools::PermissionHook for TestInterruptHook {
    async fn check(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> crate::agent::PermissionDecision {
        if self
            .interrupted_tool_call_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            // Already interrupted — don't deny further tools.
            return crate::agent::PermissionDecision::Allow;
        }
        if self.interrupt_before.iter().any(|n| n == name) {
            // Record the interrupt — the tool_call_id is synthetic because
            // the actual tool_call hasn't been assigned an id yet at this
            // point. The drive task will capture the real id from the stream.
            *self
                .interrupted_tool_name
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
            *self
                .interrupted_arguments
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(args.clone());
            return crate::agent::PermissionDecision::Deny(
                "test interrupt trigger — tool blocked by interrupt_before".into(),
            );
        }
        crate::agent::PermissionDecision::Allow
    }
}

// ── AG-UI client tools bridge ─────────────────────────────────────────────
// `RunAgentInput.tools` are frontend-owned functions (CopilotKit
// useCopilotAction etc.). We register server-side stubs so the model can
// call them, and a permission hook that denies the call before dispatch —
// the deny produces an interrupt whose payload is the tool result the
// frontend sends back via `input.resume` (docs/copilot-agent-plan.md M2-1).

pub(crate) const CLIENT_TOOL_DENY_PREFIX: &str = "[frontend tool]";

pub(crate) struct ClientToolStub {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait]
impl crate::tools::Tool for ClientToolStub {
    fn spec(&self) -> crate::llm::chat::ToolSpec {
        crate::llm::chat::ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
    async fn execute(&self, _arguments: serde_json::Value) -> crate::error::Result<String> {
        // Defensive fallback: ClientToolHook denies client tools before
        // dispatch, so execute() should never run.
        Ok(format!(
            "{} {} not executed server-side",
            CLIENT_TOOL_DENY_PREFIX, self.name
        ))
    }
}

pub(crate) struct ClientToolHook {
    pub names: HashSet<String>,
    /// First denied call: (tool_name, args_json). The tool_call_id is not
    /// known at check() time; the driver locates it by scanning the
    /// transcript for the deny-reason marker.
    pub denied: Mutex<Option<(String, String)>>,
}

#[async_trait]
impl crate::tools::PermissionHook for ClientToolHook {
    async fn check(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> crate::agent::PermissionDecision {
        if !self.names.contains(name) {
            return crate::agent::PermissionDecision::Allow;
        }
        {
            let mut slot = self.denied.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some((name.to_string(), args.to_string()));
            }
        }
        crate::agent::PermissionDecision::Deny(format!(
            "{} {} awaiting client execution",
            CLIENT_TOOL_DENY_PREFIX, name
        ))
    }
}

// ── Resume / interrupt state machine (transport-free, Issue #56) ──────────

/// Transport-free input to the AG-UI run: a parsed [`RunAgentInput`] plus
/// the workspace root that anchors the thread's session directory.
pub(crate) struct AguiRunInput<'a> {
    pub workspace: &'a Path,
    pub input: agui_protocol::RunAgentInput,
}

/// The prepared run: what to ask the agent and what transcript to seed.
pub(crate) struct PreparedAguiRun {
    /// The goal (last user message, resume directive, or context fallback).
    pub goal: String,
    /// Transcript seed: resume-modified history or the client-resent
    /// `messages` history. `None` for a bare first turn.
    pub seed_transcript: Option<Vec<crate::message::Message>>,
}

/// Typed error for the prepare phase. The HTTP adapter maps these onto
/// status codes; this type itself is transport-free.
#[derive(Debug)]
pub(crate) enum PrepareAguiError {
    /// 409 — thread has open interrupts but the request carries no resume.
    InterruptBeforeConflict { thread_id: String, open: usize },
    /// 400 — client-side input problem.
    BadRequest(String),
    /// 500 — server-side storage problem.
    Internal(String),
}

/// The neutral continuation directive used for resume turns. Resume turns
/// must NOT re-append the original user message (the seeded transcript
/// already contains it plus the injected tool result; a duplicate makes the
/// model re-issue the same tool call).
pub(crate) const RESUME_GOAL_DIRECTIVE: &str =
    "[frontend tool result received] 客户端工具结果已注入对话，请基于该结果继续回答用户最初的问题。";

/// Map AG-UI `input.messages` into a seed transcript for a NON-resume run.
///
/// Standard AG-UI clients (CopilotKit, `@ag-ui/client`) send the FULL
/// `messages` array on every turn and expect the agent to see all of it —
/// the server keeps no other per-thread context. We map plain-text
/// user/assistant messages verbatim. Tool-related messages (`tool`-role
/// results, and assistant messages carrying `tool_calls`) are SKIPPED
/// wholesale: seeding either half without its pair would orphan a tool
/// result or a tool call and violate invariant #8 (HTTP 400 from the
/// provider).
pub(crate) fn agui_seed_from_messages(
    msgs: &[&agui_protocol::Message],
) -> Vec<crate::message::Message> {
    msgs.iter()
        .filter(|m| m.tool_call_id.is_none() && m.tool_calls.is_none())
        .filter_map(|m| {
            let content = m.content.clone()?.trim().to_string();
            if content.is_empty() {
                return None;
            }
            match m.role.as_str() {
                "user" => Some(crate::message::Message::user(content)),
                "assistant" => Some(crate::message::Message::assistant(content)),
                _ => None,
            }
        })
        .collect()
}

/// Steps 1-3 of the old monolithic handler: derive the goal and the seed
/// transcript from the input, applying the resume / interrupt-before state
/// machine. Touches no axum types — unit-testable with just a temp dir.
pub(crate) fn prepare_run(
    AguiRunInput { workspace, input }: AguiRunInput<'_>,
) -> Result<PreparedAguiRun, PrepareAguiError> {
    use agui_protocol as ag;

    // If `input.resume` is present and non-empty, process the interrupt
    // resolutions before building the runtime.
    let resume_items: Vec<ag::Resume> = input.resume.clone().unwrap_or_default();

    // Derive the user goal: prefer the last user message, else fall back
    // to the first context item value. Resume turns use a neutral
    // continuation directive (see RESUME_GOAL_DIRECTIVE).
    let goal = if resume_items.is_empty() {
        input
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.clone())
            .or_else(|| input.context.first().map(|c| c.value.clone()))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                PrepareAguiError::BadRequest(
                    "RunAgentInput must contain at least one user \
                     message or a non-empty context item"
                        .into(),
                )
            })?
    } else {
        RESUME_GOAL_DIRECTIVE.to_string()
    };

    if resume_items.is_empty() {
        // Issue #62: standard AG-UI clients resend the FULL `messages`
        // array every turn and expect the agent to see the whole history.
        // Seed it so multi-turn context works without tool side effects.
        // (Tool-related messages are skipped inside — invariant #8.)
        // The LAST user message is dropped from the seed: it becomes the
        // `goal` and `runtime.run()` re-appends it as a fresh user turn —
        // seeding it too would duplicate it (the resume branch notes a
        // duplicate makes the model re-issue the same request).
        let goal_user_idx = input.messages.iter().rposition(|m| {
            m.role == "user" && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
        });
        let seeded = input
            .messages
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != goal_user_idx)
            .map(|(_, m)| m)
            .collect::<Vec<_>>();
        let seeded = agui_seed_from_messages(&seeded);
        if seeded.is_empty() {
            return Ok(PreparedAguiRun {
                goal,
                seed_transcript: None,
            });
        }
        // Interrupt-before check (spec rule 4): if the thread has open
        // interrupts and no resume is provided, reject.
        if let Some(session_dir) = agui_session_dir(workspace, &input.thread_id) {
            let open = load_open_interrupts(&session_dir);
            if !open.is_empty() {
                return Err(PrepareAguiError::InterruptBeforeConflict {
                    thread_id: input.thread_id.clone(),
                    open: open.len(),
                });
            }
        }
        return Ok(PreparedAguiRun {
            goal,
            seed_transcript: Some(seeded),
        });
    }

    // ── Resume handling ────────────────────────────────────────────────
    let session_dir = agui_session_dir(workspace, &input.thread_id)
        .ok_or_else(|| PrepareAguiError::Internal("cannot resolve session directory for resume".into()))?;

    if !session_dir.join("transcript.jsonl").is_file() {
        return Err(PrepareAguiError::BadRequest(format!(
            "no prior run found for thread '{}'; cannot resume",
            input.thread_id
        )));
    }

    // Load open interrupts from session metadata.
    let open_interrupts = load_open_interrupts(&session_dir);
    if open_interrupts.is_empty() {
        return Err(PrepareAguiError::BadRequest(format!(
            "thread '{}' has no open interrupts; nothing to resume",
            input.thread_id
        )));
    }

    // Spec rule 3: a single resume must address EVERY open interrupt.
    let resume_ids: HashSet<String> = resume_items.iter().map(|r| r.interrupt_id.clone()).collect();
    for open_int in &open_interrupts {
        if !resume_ids.contains(&open_int.interrupt_id) {
            return Err(PrepareAguiError::BadRequest(format!(
                "resume must cover all open interrupts; missing '{}'",
                open_int.interrupt_id
            )));
        }
    }

    // Load the transcript from the session. The AG-UI run persists one
    // `Message` JSON per line (see the write side in the driver task).
    let transcript_path = session_dir.join("transcript.jsonl");
    let loaded_messages: Vec<crate::message::Message> =
        std::fs::read_to_string(&transcript_path)
            .map_err(|e| PrepareAguiError::Internal(format!("failed to load session transcript: {e}")))?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

    // Build an index of resume items by interrupt_id.
    let resume_by_id: HashMap<&str, &ag::Resume> = resume_items
        .iter()
        .map(|r| (r.interrupt_id.as_str(), r))
        .collect();

    // For each resolved interrupt, find the matching tool result in the
    // transcript and replace/inject the content. The tool was denied by
    // the TestInterruptHook, so we look for the tool result whose
    // `tool_call_id` matches the interrupt's bound tool_call_id.
    let mut modified = loaded_messages;
    for open_int in &open_interrupts {
        let Some(resume) = resume_by_id.get(open_int.interrupt_id.as_str()) else {
            continue;
        };
        let tool_call_id = &open_int.tool_call_id;

        if resume.status == ag::ResumeStatus::Cancelled {
            // For cancelled interrupts, inject a sentinel tool result.
            let sentinel = crate::message::Message::tool_result(
                tool_call_id,
                "[interrupt cancelled by user]",
            );
            modified.push(sentinel);
        } else if let Some(ref payload) = resume.payload {
            // Resolved: replace the denied tool result content with the
            // resume payload, or inject a new tool result if none exists.
            let payload_str = serde_json::to_string(payload).unwrap_or_default();
            let replaced = modified.iter_mut().any(|msg| {
                if msg.tool_call_id.as_deref() == Some(tool_call_id.as_str()) {
                    msg.content = payload_str.clone();
                    true
                } else {
                    false
                }
            });
            if !replaced {
                // No existing tool result found — inject one.
                modified.push(crate::message::Message::tool_result(
                    tool_call_id,
                    &payload_str,
                ));
            }
        }
    }

    // Clear the open interrupts now that they've been consumed.
    clear_open_interrupts(&session_dir);

    Ok(PreparedAguiRun {
        goal,
        seed_transcript: Some(modified),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_input(thread: &str, resume: Option<Vec<agui_protocol::Resume>>) -> agui_protocol::RunAgentInput {
        agui_protocol::RunAgentInput {
            thread_id: thread.into(),
            run_id: "r1".into(),
            messages: vec![agui_protocol::Message {
                id: "m1".into(),
                role: "user".into(),
                content: Some("hi".into()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            context: vec![],
            tools: vec![],
            resume,
            ..Default::default()
        }
    }

    fn write_interrupts(dir: &Path, interrupts: &[OpenInterrupt]) {
        save_open_interrupts(dir, interrupts);
    }

    // ── converter ─────────────────────────────────────────────────────────

    #[test]
    fn agui_events_for_assistant_text_emits_start_content_end() {
        use agui_protocol as ag;
        let ev = AgentEvent::AssistantText {
            text: "hi".into(),
            step: 0,
        };
        let out = agui_events_for(&ev);
        assert_eq!(out.len(), 3, "got {out:?}");
        assert!(matches!(out[0], ag::Event::TextMessageStart(_)));
        assert!(matches!(out[1], ag::Event::TextMessageContent(_)));
        assert!(matches!(out[2], ag::Event::TextMessageEnd(_)));
    }

    #[test]
    fn agui_events_for_partial_token_then_tool_call_closes_stream() {
        use agui_protocol as ag;
        // Use AguiConverter directly so open_message_id state spans events.
        let mut conv = AguiConverter::new();
        let t1 = conv.convert(&AgentEvent::PartialToken {
            text: "a".into(),
            step: 0,
        });
        assert_eq!(t1.len(), 2, "Start+Content expected: {t1:?}");
        assert!(matches!(t1[0], ag::Event::TextMessageStart(_)));
        assert!(matches!(t1[1], ag::Event::TextMessageContent(_)));
        let t2 = conv.convert(&AgentEvent::PartialToken {
            text: "b".into(),
            step: 0,
        });
        assert_eq!(t2.len(), 1, "Content-only expected: {t2:?}");
        assert!(matches!(t2[0], ag::Event::TextMessageContent(_)));
        let t3 = conv.convert(&AgentEvent::ToolCall {
            id: "tc-1".into(),
            name: "Bash".into(),
            arguments: "{}".into(),
            step: 0,
        });
        assert!(
            t3.iter().any(|e| matches!(e, ag::Event::TextMessageEnd(_))),
            "ToolCall must close open text stream: {t3:?}"
        );
        assert!(
            t3.iter().any(|e| matches!(e, ag::Event::ToolCallStart(_))),
            "ToolCall must emit ToolCallStart: {t3:?}"
        );
        assert!(
            t3.iter().any(|e| matches!(e, ag::Event::ToolCallArgs(_))),
            "ToolCall must emit ToolCallArgs: {t3:?}"
        );
        assert!(
            t3.iter().any(|e| matches!(e, ag::Event::ToolCallEnd(_))),
            "ToolCall must emit ToolCallEnd: {t3:?}"
        );
    }

    // ── sanitize_thread_id_for_session ──────────────────────────────────────

    #[test]
    fn sanitize_thread_id_valid_passthrough() {
        assert_eq!(sanitize_thread_id_for_session("abc-123"), "abc-123");
        assert_eq!(sanitize_thread_id_for_session("foo_bar.baz"), "foo_bar.baz");
    }

    #[test]
    fn sanitize_thread_id_replaces_special_chars() {
        let out = sanitize_thread_id_for_session("a/b:c");
        assert!(!out.contains('/'), "slash must be replaced");
        assert!(!out.contains(':'), "colon must be replaced");
    }

    #[test]
    fn sanitize_thread_id_leading_dot_replaced() {
        let out = sanitize_thread_id_for_session(".hidden");
        assert!(
            !out.starts_with('.'),
            "leading dot must be replaced; got {out}"
        );
    }

    #[test]
    fn sanitize_thread_id_double_dot_collapsed() {
        let out = sanitize_thread_id_for_session("a..b");
        assert!(
            !out.contains(".."),
            "double dot must be collapsed; got {out}"
        );
    }

    #[test]
    fn sanitize_thread_id_empty_becomes_default() {
        assert_eq!(sanitize_thread_id_for_session(""), "default");
    }

    // ── agui_seed_from_messages ─────────────────────────────────────────────

    /// Unit-level: tool-related messages are skipped (invariant #8 —
    /// seeding an assistant tool_call or a tool result without its pair
    /// orphans the other half and gets HTTP 400 from the provider), and
    /// plain user/assistant text passes through in order.
    #[test]
    fn agui_seed_from_messages_skips_tool_roles_and_maps_text() {
        use agui_protocol as ag;
        let mk = |role: &str, content: Option<&str>| ag::Message {
            id: format!("m-{role}"),
            role: role.into(),
            content: content.map(Into::into),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let mut tool_result = mk("tool", Some("result"));
        tool_result.tool_call_id = Some("tc-1".into());
        let mut assistant_with_calls = mk("assistant", Some("calling"));
        assistant_with_calls.tool_calls = Some(serde_json::json!([]));

        let msgs: Vec<ag::Message> = vec![
            mk("user", Some("turn one")),
            assistant_with_calls,
            tool_result,
            mk("assistant", Some("answer one")),
            mk("user", Some("turn two")),
            mk("user", None),          // empty content → dropped
            mk("system", Some("sys")), // non-user/assistant → dropped
        ];
        let msgs: Vec<&ag::Message> = msgs.iter().collect();
        let seed = agui_seed_from_messages(&msgs);
        let rendered: Vec<(crate::message::Role, String)> =
            seed.into_iter().map(|m| (m.role, m.content)).collect();
        use crate::message::Role;
        assert_eq!(
            rendered,
            vec![
                (Role::User, "turn one".into()),
                (Role::Assistant, "answer one".into()),
                (Role::User, "turn two".into()),
            ],
            "tool roles / empty / system messages must be skipped, rest mapped in order"
        );
    }

    // ── resume / interrupt state machine (Issue #56: no HTTP needed) ───────

    #[test]
    fn prepare_run_rejects_empty_goal() {
        let ws = tempfile::tempdir().unwrap();
        let mut input = run_input("t", None);
        input.messages.clear();
        match prepare_run(AguiRunInput {
            workspace: ws.path(),
            input,
        }) {
            Err(PrepareAguiError::BadRequest(m)) => assert!(m.contains("user message"), "{m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn prepare_run_interrupt_before_conflict_without_resume() {
        let ws = tempfile::tempdir().unwrap();
        let dir = agui_session_dir(ws.path(), "t-conflict").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        write_interrupts(
            &dir,
            &[OpenInterrupt {
                interrupt_id: "i1".into(),
                tool_call_id: "tc-1".into(),
                reason: "tool_call".into(),
                message: Some("need input".into()),
                created_at: "now".into(),
            }],
        );
        match prepare_run(AguiRunInput {
            workspace: ws.path(),
            input: run_input("t-conflict", None),
        }) {
            Err(PrepareAguiError::InterruptBeforeConflict { open, .. }) => assert_eq!(open, 1),
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn prepare_run_resume_without_prior_run_is_bad_request() {
        let ws = tempfile::tempdir().unwrap();
        let resume = vec![agui_protocol::Resume {
            interrupt_id: "i1".into(),
            status: agui_protocol::ResumeStatus::Resolved,
            payload: Some(serde_json::json!({"ok": true})),
        }];
        match prepare_run(AguiRunInput {
            workspace: ws.path(),
            input: run_input("t-fresh", Some(resume)),
        }) {
            Err(PrepareAguiError::BadRequest(m)) => assert!(m.contains("no prior run"), "{m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn prepare_run_resume_must_cover_all_open_interrupts() {
        let ws = tempfile::tempdir().unwrap();
        let dir = agui_session_dir(ws.path(), "t-cover").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        write_interrupts(
            &dir,
            &[
                OpenInterrupt {
                    interrupt_id: "i1".into(),
                    tool_call_id: "tc-1".into(),
                    reason: "tool_call".into(),
                    message: None,
                    created_at: "now".into(),
                },
                OpenInterrupt {
                    interrupt_id: "i2".into(),
                    tool_call_id: "tc-2".into(),
                    reason: "tool_call".into(),
                    message: None,
                    created_at: "now".into(),
                },
            ],
        );
        // transcript with the denied tool results for both interrupts
        let lines: Vec<String> = ["tc-1", "tc-2"]
            .into_iter()
            .map(|tc| {
                serde_json::to_string(&crate::message::Message::tool_result(tc, "denied")).unwrap()
            })
            .collect();
        std::fs::write(dir.join("transcript.jsonl"), lines.join("\n")).unwrap();

        let partial = vec![agui_protocol::Resume {
            interrupt_id: "i1".into(),
            status: agui_protocol::ResumeStatus::Resolved,
            payload: Some(serde_json::json!("done")),
        }];
        match prepare_run(AguiRunInput {
            workspace: ws.path(),
            input: run_input("t-cover", Some(partial)),
        }) {
            Err(PrepareAguiError::BadRequest(m)) => assert!(m.contains("missing 'i2'"), "{m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }

        // Full coverage resolves: replaces the matching tool result content
        // and clears the open-interrupt store.
        let full = vec![
            agui_protocol::Resume {
                interrupt_id: "i1".into(),
                status: agui_protocol::ResumeStatus::Resolved,
                payload: Some(serde_json::json!("from-client")),
            },
            agui_protocol::Resume {
                interrupt_id: "i2".into(),
                status: agui_protocol::ResumeStatus::Cancelled,
                payload: None,
            },
        ];
        let prepared = prepare_run(AguiRunInput {
            workspace: ws.path(),
            input: run_input("t-cover", Some(full)),
        })
        .expect("resume prepares");
        assert_eq!(prepared.goal, RESUME_GOAL_DIRECTIVE);
        let seed = prepared.seed_transcript.expect("seed");
        let tc1 = seed
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc-1"))
            .expect("tc-1 present");
        assert_eq!(tc1.content, "\"from-client\"");
        // cancelled → sentinel injected
        let tc2 = seed
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc-2"))
            .expect("tc-2 present");
        assert_eq!(tc2.content, "[interrupt cancelled by user]");
        // interrupts consumed
        assert!(
            load_open_interrupts(&dir).is_empty(),
            "open interrupts must be cleared after resume"
        );
    }

    #[test]
    fn prepare_run_resume_with_no_open_interrupts_is_bad_request() {
        let ws = tempfile::tempdir().unwrap();
        let dir = agui_session_dir(ws.path(), "t-none").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("transcript.jsonl"), "").unwrap();
        let resume = vec![agui_protocol::Resume {
            interrupt_id: "i1".into(),
            status: agui_protocol::ResumeStatus::Resolved,
            payload: None,
        }];
        match prepare_run(AguiRunInput {
            workspace: ws.path(),
            input: run_input("t-none", Some(resume)),
        }) {
            Err(PrepareAguiError::BadRequest(m)) => assert!(m.contains("no open interrupts"), "{m}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn prepare_run_first_turn_seeds_history_minus_goal() {
        let ws = tempfile::tempdir().unwrap();
        let mut input = run_input("t-first", None);
        input.messages = vec![
            agui_protocol::Message {
                id: "m0".into(),
                role: "user".into(),
                content: Some("turn one".into()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            agui_protocol::Message {
                id: "m1".into(),
                role: "assistant".into(),
                content: Some("answer".into()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            agui_protocol::Message {
                id: "m2".into(),
                role: "user".into(),
                content: Some("turn two".into()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let prepared = prepare_run(AguiRunInput {
            workspace: ws.path(),
            input,
        })
        .expect("prepare");
        assert_eq!(prepared.goal, "turn two");
        let seed = prepared.seed_transcript.expect("seed");
        let contents: Vec<&str> = seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["turn one", "answer"]);
    }
}

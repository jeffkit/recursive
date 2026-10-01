//! AG-UI server-side session layer (Issue #56).
//!
//! Everything AG-UI lives here, not in `handlers.rs`: the AgentEvent→Event
//! converter, the thread↔session-directory mapping, the persisted
//! open-interrupt store, the client-tool / test-interrupt permission hooks,
//! the resume/interrupt state machine (input parsing → seed transcript),
//! the runtime assembly for an AG-UI run, and the driver task that maps a
//! finished run onto AG-UI events (transcript persistence, interrupt
//! bookkeeping, checkpoint / RunFinished emission).
//!
//! The only axum-aware piece is [`super::handlers::agui_run`], which parses
//! the JSON body, maps admission/prepare errors onto status codes, and
//! frames the returned event stream as SSE. Everything below is
//! unit-testable without an HTTP server.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agui_protocol as ag;
use tokio::sync::mpsc;

use crate::event::{AgentEvent, ChannelSink, NullSink};
use crate::runtime::AgentRuntime;
use crate::tools::ToolRegistry;

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
    /// Issue #66: concatenation of the `PartialToken` deltas already emitted
    /// for the open message. When the step's final `AssistantText` arrives it
    /// usually equals this string — emitting it again as a fresh message
    /// would render the answer twice in AG-UI clients, so only the unsent
    /// remainder (if any) is flushed before `TextMessageEnd`.
    open_accumulated: String,
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
                // Close any in-flight streamed message first. Issue #66: the
                // deltas already carried (almost always) the whole text —
                // flush only the unsent remainder into the SAME message and
                // close it, instead of emitting a second full-text message
                // that would make clients render the answer twice.
                if let Some(id) = self.open_message_id.take() {
                    let accumulated = std::mem::take(&mut self.open_accumulated);
                    if let Some(rest) = text.strip_prefix(accumulated.as_str()) {
                        if !rest.is_empty() {
                            out.push(ag::Event::TextMessageContent(ag::TextMessageContent {
                                message_id: id.clone(),
                                delta: rest.to_string(),
                                base: ag::BaseEvent::default(),
                            }));
                        }
                        out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                            message_id: id.clone(),
                            base: ag::BaseEvent::default(),
                        }));
                        self.last_assistant_message_id = Some(id);
                    } else {
                        // Deltas diverged from the final text (misbehaving
                        // provider): keep the streamed message as-is and fall
                        // back to the historical full-message emission so no
                        // text is lost.
                        out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                            message_id: id,
                            base: ag::BaseEvent::default(),
                        }));
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
                    }
                } else {
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
                }
                self.open_message_id = None;
                self.open_accumulated.clear();
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
                self.open_accumulated.push_str(text);
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
                self.open_accumulated.clear();
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
                self.open_accumulated.clear();
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

/// Path to the session directory for an AG-UI thread.
///
/// Since issue #57 this is the native session layout
/// (`<sessions>/<workspace-slug>/agui-<thread-key>/`, see
/// [`crate::agui_session`]); pre-#57 flat thread directories are
/// migrated on resolve. The old lossy sanitiser survives only as
/// `legacy_sanitize_thread_id` inside [`crate::agui_session`] (kept for
/// migration lookups; its traversal tests live there too).
pub(crate) fn agui_session_dir(workspace: &Path, thread_id: &str) -> Option<PathBuf> {
    crate::agui_session::resolve_session_dir(workspace, thread_id)
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
    pub input: &'a agui_protocol::RunAgentInput,
}

/// The prepared run: what to ask the agent and what transcript to seed.
#[derive(Debug)]
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
        // Interrupt-before check (spec rule 4): if the thread has open
        // interrupts and no resume is provided, reject. Runs BEFORE any
        // early return — the conflict applies no matter what history the
        // client re-sent.
        if let Some(session_dir) = agui_session_dir(workspace, &input.thread_id) {
            let open = load_open_interrupts(&session_dir);
            if !open.is_empty() {
                return Err(PrepareAguiError::InterruptBeforeConflict {
                    thread_id: input.thread_id.clone(),
                    open: open.len(),
                });
            }
        }
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
        return Ok(PreparedAguiRun {
            goal,
            seed_transcript: Some(seeded),
        });
    }

    // ── Resume handling ────────────────────────────────────────────────
    let session_dir = agui_session_dir(workspace, &input.thread_id).ok_or_else(|| {
        PrepareAguiError::Internal("cannot resolve session directory for resume".into())
    })?;

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
    let resume_ids: HashSet<String> = resume_items
        .iter()
        .map(|r| r.interrupt_id.clone())
        .collect();
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
    let loaded_messages: Vec<crate::message::Message> = std::fs::read_to_string(&transcript_path)
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
    // The same splices must land on disk (issue #57): runs append, so
    // without this the persisted transcript keeps the deny-marker text
    // and a later resume re-seeds the wrong history.
    let mut disk_replacements: Vec<(String, String)> = Vec::new();
    let mut disk_injections: Vec<(String, String)> = Vec::new();
    for open_int in &open_interrupts {
        let Some(resume) = resume_by_id.get(open_int.interrupt_id.as_str()) else {
            continue;
        };
        let tool_call_id = &open_int.tool_call_id;

        if resume.status == ag::ResumeStatus::Cancelled {
            // For cancelled interrupts, replace the denied tool result with
            // a sentinel (or inject one if no result exists) so the model
            // sees exactly one tool message for the call.
            let sentinel =
                crate::message::Message::tool_result(tool_call_id, "[interrupt cancelled by user]");
            let replaced = modified.iter_mut().any(|msg| {
                if msg.tool_call_id.as_deref() == Some(tool_call_id.as_str()) {
                    *msg = sentinel.clone();
                    true
                } else {
                    false
                }
            });
            if replaced {
                disk_replacements.push((
                    tool_call_id.clone(),
                    "[interrupt cancelled by user]".to_string(),
                ));
            } else {
                modified.push(sentinel);
                disk_injections.push((
                    tool_call_id.clone(),
                    "[interrupt cancelled by user]".to_string(),
                ));
            }
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
                disk_injections.push((tool_call_id.clone(), payload_str));
            } else {
                disk_replacements.push((tool_call_id.clone(), payload_str));
            }
        }
    }

    if !disk_replacements.is_empty() || !disk_injections.is_empty() {
        crate::agui_session::apply_resume_tool_results(
            &session_dir,
            &disk_replacements,
            &disk_injections,
        );
    }

    // Clear the open interrupts now that they've been consumed.
    clear_open_interrupts(&session_dir);

    Ok(PreparedAguiRun {
        goal,
        seed_transcript: Some(modified),
    })
}

// ── Runtime assembly for an AG-UI run (transport-free) ────────────────────

/// Everything needed to turn a [`PreparedAguiRun`] into a running
/// [`AgentRuntime`]. Built by the HTTP adapter from `AppState`; consumed
/// by [`build_agui_runtime`].
pub(crate) struct AguiRuntimeDeps<'a> {
    /// LLM provider for the run (`AppState.provider` at the HTTP layer).
    pub llm: Arc<dyn crate::llm::ChatProvider>,
    /// Registry the per-run runtime should be built from (already
    /// tier-resolved via `AppState::session_tool_registry`).
    pub tool_registry: ToolRegistry,
    /// Fully assembled system prompt (project context + skill index).
    pub system_prompt: String,
    /// Prompt segments from the same assembly.
    pub prompt_segments: crate::system_prompt::PromptSegments,
    /// Step budget for the run (config.max_steps at the HTTP layer).
    pub max_steps: usize,
    /// Transcript seed from [`prepare_run`] (`Some` on resume / history
    /// seeding).
    pub seed_transcript: Option<Vec<crate::message::Message>>,
    /// Names of tools in `interrupt_before` (test-only interrupt trigger).
    pub interrupt_before: &'a [String],
    /// AG-UI client tools to register as stubs with a deny hook.
    pub client_tools: &'a [ag::Tool],
}

/// The hooks installed by [`build_agui_runtime`], handed back so the
/// driver can detect which mechanism (if any) fired during the run.
pub(crate) struct AguiHooks {
    pub interrupt_hook: Option<Arc<TestInterruptHook>>,
    pub client_hook: Option<Arc<ClientToolHook>>,
}

/// Build the runtime for an AG-UI run: registers client-tool stubs,
/// installs the interrupt / client-tool permission hook, seeds the
/// transcript, wires per-turn checkpoints under the thread's session
/// directory, and routes the hook into the runtime's TurnContext.
///
/// Transport-free: no axum types, no HTTP status mapping. Hook selection
/// rule (unchanged): client tools take the permission-hook slot;
/// `interrupt_before` is a test-only facility ignored when client tools
/// are present. Returns the hooks alongside the runtime so the driver
/// ([`spawn_agui_run`]) can detect which mechanism fired.
pub(crate) fn build_agui_runtime(
    workspace: &Path,
    thread_id: &str,
    deps: AguiRuntimeDeps<'_>,
) -> crate::error::Result<(AgentRuntime, AguiHooks)> {
    let interrupt_hook = if deps.interrupt_before.is_empty() {
        None
    } else {
        Some(Arc::new(TestInterruptHook {
            interrupt_before: deps.interrupt_before.to_vec(),
            interrupted_tool_call_id: Mutex::new(None),
            interrupted_tool_name: Mutex::new(None),
            interrupted_arguments: Mutex::new(None),
        }))
    };

    let mut tool_registry = deps.tool_registry;
    for t in deps.client_tools {
        tool_registry = tool_registry.register(Arc::new(ClientToolStub {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        }));
    }

    let client_tool_names: HashSet<String> =
        deps.client_tools.iter().map(|t| t.name.clone()).collect();
    let client_hook: Option<Arc<ClientToolHook>> = if client_tool_names.is_empty() {
        None
    } else {
        Some(Arc::new(ClientToolHook {
            names: client_tool_names,
            denied: Mutex::new(None),
        }))
    };
    if let Some(ref hook) = client_hook {
        tool_registry.set_permission_hook(hook.clone());
    } else if let Some(ref hook) = interrupt_hook {
        tool_registry.set_permission_hook(hook.clone());
    }

    let mut runtime_builder = super::handlers::build_session_runtime_parts(
        tool_registry,
        deps.system_prompt,
        deps.prompt_segments,
        deps.max_steps,
    )
    .llm(deps.llm)
    // Issue #66 §3.2: token-level streaming — RunCore only builds the
    // partial-token forwarder when `streaming` is set. The converter above
    // frames `PartialToken` deltas into TextMessageStart/Content/End and
    // suppresses the duplicate final full-text message.
    .streaming(true);
    if let Some(seed) = deps.seed_transcript {
        runtime_builder = runtime_builder.seed_transcript(seed);
    }
    let mut runtime = runtime_builder.build()?;

    // Route the permission hook into the runtime's TurnContext: client
    // tools must be denied at dispatch so they surface as interrupts.
    if let Some(ref hook) = client_hook {
        runtime.set_permission_hook(hook.clone());
    } else if let Some(ref hook) = interrupt_hook {
        runtime.set_permission_hook(hook.clone());
    }

    // Wire per-turn workspace checkpoints. The AG-UI thread IS the
    // session (issue #57): the checkpoint chain id is the thread's
    // session key and the log lives inside the thread's session
    // directory, next to transcript.jsonl. Failures (no git on PATH,
    // bad workspace path, etc.) only log a warning — the run still
    // proceeds without checkpoints.
    if let Ok(repo) = crate::ShadowRepo::open(workspace) {
        if let Some(session_dir) = crate::agui_session::session_dir(workspace, thread_id) {
            let session_id = crate::agui_session::thread_session_key(thread_id);
            let _ = std::fs::create_dir_all(&session_dir);
            let log_path = session_dir.join("checkpoints.jsonl");
            let touched = runtime.kernel().tools().touched_files();
            if let Err(e) =
                runtime.enable_checkpoints(Arc::new(repo), session_id, log_path, touched)
            {
                tracing::warn!("agui: enable_checkpoints failed, continuing without: {e}");
            }
        }
    } else {
        tracing::debug!("agui: shadow git unavailable, no per-turn checkpoints");
    }

    Ok((
        runtime,
        AguiHooks {
            interrupt_hook,
            client_hook,
        },
    ))
}

// ── Run driver: maps a finished run onto AG-UI events ─────────────────────

/// Inputs to [`spawn_agui_run`] beyond the runtime and goal. Bundled so the
/// spawn signature stays readable.
pub(crate) struct AguiRunContext {
    pub thread_id: String,
    pub run_id: String,
    pub client_tools: Vec<ag::Tool>,
    pub hooks: AguiHooks,
    pub workspace: PathBuf,
    pub metrics: Arc<super::Metrics>,
    /// Model/provider identity for `.meta.json` (issue #57), carried over
    /// from the deps that built the runtime.
    pub model: String,
    pub provider: String,
    pub preset: Option<String>,
    /// Issue #66 §3.3: the per-run cancellation token. `spawn_agui_run`
    /// installs it on the runtime and registers it under the thread id in
    /// `AppState::agui_active_runs`; the SSE body's disconnect guard and
    /// `POST /agui/{thread_id}/cancel` cancel it, and the driver removes
    /// the registry entry when the run finishes.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Issue #66: the admission permit for this run. Held by the driver
    /// task for the whole background run so `runs_in_flight` stays
    /// truthful and the slot is not released while the agent still runs.
    pub permit: crate::session_host::RunPermit,
    /// Issue #57 §④: the per-thread run fence guard. Held by the driver
    /// task so the fence stays closed for the whole background run — a
    /// second run for the same thread gets 409 until this drops.
    pub run_guard: crate::session_host::ActiveRunGuard,
    /// Issue #66: the AppState-level cancel registry (thread id → token).
    /// Borrowed only to insert/remove this run's own entry.
    pub active_runs: Arc<std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
}

/// Spawn the AG-UI run driver.
///
/// Returns the receiving end of a channel that yields the run's
/// [`ag::Event`]s in protocol order: `RunStarted` was already emitted by
/// the caller (see [`spawn_agui_run`]); the driver adds converter-forwarded
/// events, the optional `Custom("agui-tui/checkpoint_post")`, snapshot
/// events for interrupts, and finally `RunFinished` — always last.
///
/// The driver owns the runtime: it runs the agent, records metrics,
/// persists the transcript under the thread's session directory (so a
/// later `resume` can reload it), detects which interrupt mechanism fired
/// (client tools / `interrupt_before`), persists the open interrupt
/// before emitting it (crash safety), and destroys the run environment
/// unconditionally before `RunFinished` is emitted.
pub(crate) fn spawn_agui_run(
    mut runtime: AgentRuntime,
    goal: String,
    ctx: AguiRunContext,
) -> mpsc::UnboundedReceiver<ag::Event> {
    let AguiRunContext {
        thread_id,
        run_id,
        client_tools,
        hooks,
        workspace,
        metrics,
        model,
        provider,
        preset,
        cancel,
        permit,
        run_guard,
        active_runs,
    } = ctx;
    let (sse_tx, sse_rx) = mpsc::unbounded_channel::<ag::Event>();

    // Issue #66 §3.3: install the per-run cancellation token on the
    // runtime (the kernel checks it between steps and mid-LLM-call) and
    // register it under the thread id — the SSE body's disconnect guard
    // and `POST /agui/{thread_id}/cancel` both cancel through this
    // registry. The driver task removes the entry when the run finishes;
    // cancelling a finished run is a no-op.
    runtime.set_interrupt_token(cancel.clone());
    active_runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(thread_id.clone(), cancel.clone());

    // Emit RunStarted up front so clients can render the run shell
    // before the first model token arrives.
    let _ = sse_tx.send(ag::Event::RunStarted(ag::RunStarted {
        thread_id: thread_id.clone(),
        run_id: run_id.clone(),
        base: ag::BaseEvent::default(),
    }));

    let (sink, mut event_rx) = ChannelSink::new();
    runtime.set_event_sink(Arc::new(sink));

    // Converter task: forward AgentEvents → AG-UI Events. Owns the
    // AguiConverter so framing state survives across the whole run.
    // It does NOT emit RunFinished — the driver task does that after
    // it can also surface the optional checkpoint_post Custom event.
    let conv_tx = sse_tx.clone();
    let converter_handle = tokio::spawn(async move {
        let mut conv = AguiConverter::new();
        while let Some(agent_event) = event_rx.recv().await {
            for ev in conv.convert(&agent_event) {
                if conv_tx.send(ev).is_err() {
                    return;
                }
            }
        }
    });

    // Drive the agent on a background task so the response stream can
    // flush bytes to the client incrementally. Order of events emitted
    // by the driver after run() returns:
    //   1. Wait for the converter to drain all AgentEvents.
    //   2. If a checkpoint id was produced, emit
    //      Custom("agui-tui/checkpoint_post").
    //   3. Emit RunFinished — always last.
    let drv_thread = thread_id;
    let drv_run = run_id;
    let drv_workspace = workspace;
    // Session identity for `.meta.json` (issue #57): the config that is
    // about to serve this run. The provider the runtime was built with is
    // not nameable from a `dyn ChatProvider`, so the record keeps the
    // provider that assembled the runtime (`deps.provider_name`).
    let drv_model = model;
    let drv_provider = provider;
    let drv_preset = preset;
    // Transcript length before the run: everything after this index is
    // THIS run's contribution — only that gets appended to the session
    // (on resume runs the transcript starts with the seeded history;
    // appending it again would duplicate it).
    let drv_pre_run_len = runtime.transcript().len();
    // Issue #66: cancel-registry bookkeeping — the driver removes its
    // thread's token when the run finishes so a later run registers a
    // fresh one.
    let drv_thread_key = drv_thread.clone();

    let driver_handle = tokio::spawn(async move {
        // Issue #66: hold the admission permit for the whole background run.
        let _permit = permit;
        let outcome = runtime.run(&goal).await;

        // Locate the interrupted tool call after the run:
        // - test hook: transcript contains the fixed deny marker
        // - client tools: transcript contains CLIENT_TOOL_DENY_PREFIX
        // Both markers land in the Tool-role message that the registry
        // writes for a denied call, and carry the real tool_call_id.
        let find_denied_tool_call = |transcript: &[crate::message::Message], marker: &str| {
            transcript
                .iter()
                .rev()
                .find(|msg| msg.role == crate::message::Role::Tool && msg.content.contains(marker))
                .and_then(|msg| msg.tool_call_id.clone())
        };

        let client_denied: Option<(String, String)> = hooks.client_hook.as_ref().and_then(|h| {
            let guard = h.denied.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        });
        let test_was_interrupted = hooks
            .interrupt_hook
            .as_ref()
            .and_then(|hook| {
                hook.interrupted_tool_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .is_some();

        // Replace the sink so the converter task's recv() sees a closed
        // channel and exits cleanly.
        runtime.set_event_sink(Arc::new(NullSink));

        // Snapshot what we need from the outcome before metrics consume it.
        let (checkpoint_id, finished_turn): (Option<String>, Option<usize>) = match &outcome {
            Ok(o) => (
                o.checkpoint_id.as_ref().map(|c| c.0.clone()),
                runtime.turn_index().checked_sub(1),
            ),
            Err(_) => (None, None),
        };

        match &outcome {
            Ok(o) => super::handlers::record_run_success(&metrics, o.steps, &o.total_usage),
            Err(_) => super::handlers::record_run_failed(&metrics),
        }

        // Persist the run into the thread's native session (issue #57):
        // SessionWriter appends this run's messages (uuid chain, msg ids,
        // timestamps) and updates `.meta.json` — status, prompts, message
        // count and cumulative token cost — so the thread is a first-class
        // session (`sessions list`, `episodic_recall`, resume picker).
        // `CostTracker` adds `cost.json` + the `cost_usd` block. Only the
        // messages this run produced are appended (drv_pre_run_len skips
        // the seeded history on resume runs).
        {
            let status = if client_denied.is_some() || test_was_interrupted {
                crate::session::SessionStatus::Interrupted
            } else {
                match &outcome {
                    Ok(o) => crate::session::SessionStatus::for_finish(&o.finish_reason),
                    Err(_) => crate::session::SessionStatus::Crashed,
                }
            };
            let transcript = runtime.transcript();
            let new_messages = transcript
                .get(drv_pre_run_len.min(transcript.len())..)
                .unwrap_or(&[]);
            let record = crate::agui_session::RunRecord {
                workspace: &drv_workspace,
                thread_id: &drv_thread,
                messages: new_messages,
                goal: &goal,
                model: &drv_model,
                provider: &drv_provider,
                preset: drv_preset.as_deref(),
                status,
                usage: outcome.as_ref().ok().map(|o| o.total_usage),
                llm_latency_ms: outcome.as_ref().ok().map(|o| o.llm_latency_ms).unwrap_or(0),
            };
            if let Err(e) = crate::agui_session::persist_run(record) {
                tracing::warn!("agui: session persist failed: {e}");
            }
        }

        // Interrupt details for whichever mechanism fired. `parameters`
        // carries the client tool's input schema so the frontend knows how
        // to execute it; `args` echoes the model's arguments.
        let interrupt_details: Option<AguiInterruptDetail> =
            if let Some((name, args)) = client_denied {
                let transcript = runtime.transcript();
                find_denied_tool_call(transcript, CLIENT_TOOL_DENY_PREFIX).map(|tc_id| {
                    let parameters = client_tools
                        .iter()
                        .find(|t| t.name == name)
                        .map(|t| t.parameters.clone())
                        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                    AguiInterruptDetail::Client {
                        tool_call_id: tc_id,
                        tool_name: name,
                        parameters,
                        args: serde_json::Value::String(args),
                    }
                })
            } else if test_was_interrupted {
                let transcript = runtime.transcript();
                let denied_tool_name = hooks.interrupt_hook.as_ref().and_then(|h| {
                    h.interrupted_tool_name
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                });
                find_denied_tool_call(transcript, "test interrupt trigger").map(|tc_id| {
                    AguiInterruptDetail::Test {
                        tool_call_id: tc_id,
                        tool_name: denied_tool_name.unwrap_or_else(|| "unknown".into()),
                    }
                })
            } else {
                None
            };

        // Wait for the converter task to translate the last AgentEvent
        // before we emit anything else, so checkpoint_post and
        // RunFinished are guaranteed to arrive last.
        let _ = converter_handle.await;

        // Issue #31 §B: the AG-UI run's environment dies with the run —
        // on success AND error (the outcome match above already recorded
        // metrics; teardown is unconditional here, before RunFinished is
        // emitted, so the SSE stream stays the last observer).
        runtime.destroy_environment().await;

        if let (Some(cp), Some(turn)) = (checkpoint_id, finished_turn) {
            let _ = sse_tx.send(ag::Event::Custom(ag::Custom {
                name: "agui-tui/checkpoint_post".into(),
                value: serde_json::json!({
                    "turn": turn,
                    "postId": cp,
                }),
                base: ag::BaseEvent::default(),
            }));
        }

        // Emit RunFinished — with Interrupt outcome if a test trigger or a
        // client-tool call fired.
        if let Some(detail) = interrupt_details {
            let (tc_id, _tc_name, response_schema, metadata, message) = match detail {
                AguiInterruptDetail::Test {
                    tool_call_id,
                    tool_name,
                } => (
                    tool_call_id,
                    tool_name.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": { "approved": { "type": "boolean" } }
                    }),
                    serde_json::json!({ "testTrigger": true, "toolName": tool_name }),
                    format!("Test interrupt: tool '{tool_name}' needs user input to proceed"),
                ),
                AguiInterruptDetail::Client {
                    tool_call_id,
                    tool_name,
                    parameters,
                    args,
                } => (
                    tool_call_id,
                    tool_name.clone(),
                    parameters,
                    serde_json::json!({ "frontendTool": true, "toolName": tool_name, "args": args }),
                    format!("Frontend tool '{tool_name}' needs client execution to proceed"),
                ),
            };
            let tool_call_id = tc_id.clone();
            let interrupt_message = message.clone();

            // Build the interrupt and persist it.
            let interrupt_id = uuid::Uuid::new_v4().to_string();
            let open_interrupt = OpenInterrupt {
                interrupt_id: interrupt_id.clone(),
                tool_call_id: tool_call_id.clone(),
                reason: "tool_call".into(),
                message: Some(interrupt_message.clone()),
                created_at: crate::session::chrono_lite_now(),
            };

            // Persist before emitting (crash safety).
            if let Some(session_dir) = agui_session_dir(&drv_workspace, &drv_thread) {
                let _ = std::fs::create_dir_all(&session_dir);
                save_open_interrupts(&session_dir, std::slice::from_ref(&open_interrupt));

                // Emit StateSnapshot and MessagesSnapshot before RunFinished
                // per spec requirement (snapshots must precede the interrupting
                // RunFinished event).
                if let Ok(state_val) = serde_json::to_value(runtime.transcript()) {
                    let _ = sse_tx.send(ag::Event::StateSnapshot(ag::StateSnapshot {
                        snapshot: state_val,
                        base: ag::BaseEvent::default(),
                    }));
                }
                let messages_json: Vec<serde_json::Value> = runtime
                    .transcript()
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                let _ = sse_tx.send(ag::Event::MessagesSnapshot(ag::MessagesSnapshot {
                    messages: messages_json,
                    base: ag::BaseEvent::default(),
                }));
            }

            let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
                thread_id: drv_thread,
                run_id: drv_run,
                outcome: Some(ag::RunFinishedOutcome::Interrupt {
                    interrupts: vec![ag::Interrupt {
                        id: interrupt_id,
                        reason: "tool_call".into(),
                        message: Some(interrupt_message),
                        tool_call_id: Some(tool_call_id),
                        response_schema: Some(response_schema),
                        expires_at: None,
                        metadata: Some(metadata),
                    }],
                }),
                result: None,
                base: ag::BaseEvent::default(),
            }));
        } else {
            // Report the run's real outcome. When runtime.run() returned
            // Err (LLM failure, tool failure, provider down, ...), the
            // RunFinished must carry an Error outcome so the client can
            // distinguish a failed run from a successful one with no
            // result — otherwise the failure is silently swallowed at
            // the SSE boundary. `code` is reserved for a follow-up goal
            // that maps Error::Cancelled / RateLimited / etc. to codes.
            let (run_outcome, result_msg) = match &outcome {
                Ok(o) => (
                    ag::RunFinishedOutcome::Success,
                    o.final_text.clone().map(serde_json::Value::String),
                ),
                Err(e) => (
                    ag::RunFinishedOutcome::Error {
                        message: e.to_string(),
                        code: None,
                    },
                    None,
                ),
            };
            let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
                thread_id: drv_thread,
                run_id: drv_run,
                outcome: Some(run_outcome),
                result: result_msg,
                base: ag::BaseEvent::default(),
            }));
        }

        // Issue #66: drop the cancel-registry entry so a later run on the
        // same thread registers a fresh token; cancelling a finished run
        // must be a no-op.
        active_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&drv_thread_key);
        // Release the per-thread run fence (issue #57 §④): the next run
        // for this thread may start once RunFinished is out. On panic the
        // guard's Drop unwinds it free.
        drop(run_guard);
    });

    // Monitor the driver task so panics are surfaced in logs rather than
    // silently swallowed by the dropped JoinHandle.
    tokio::spawn(async move {
        if let Err(e) = driver_handle.await {
            tracing::error!("agui: driver task panicked: {e}");
        }
    });

    sse_rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_input(
        thread: &str,
        resume: Option<Vec<agui_protocol::Resume>>,
    ) -> agui_protocol::RunAgentInput {
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
            state: None,
            interrupt_before: None,
            forwarded_props: None,
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
            input: &input,
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
            input: &run_input("t-conflict", None),
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
            input: &run_input("t-fresh", Some(resume)),
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
            input: &run_input("t-cover", Some(partial)),
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
            input: &run_input("t-cover", Some(full)),
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
            input: &run_input("t-none", Some(resume)),
        }) {
            Err(PrepareAguiError::BadRequest(m)) => {
                assert!(m.contains("no open interrupts"), "{m}")
            }
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
            input: &input,
        })
        .expect("prepare");
        assert_eq!(prepared.goal, "turn two");
        let seed = prepared.seed_transcript.expect("seed");
        let contents: Vec<&str> = seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["turn one", "answer"]);
    }

    // ── build_agui_runtime (Issue #56: runtime assembly, no HTTP) ──────────

    use crate::tools::PermissionHook;

    fn runtime_deps<'a>(
        registry: ToolRegistry,
        client_tools: &'a [ag::Tool],
        interrupt_before: &'a [String],
    ) -> AguiRuntimeDeps<'a> {
        AguiRuntimeDeps {
            llm: Arc::new(crate::llm::MockProvider::new(vec![])),
            tool_registry: registry,
            system_prompt: "sys".into(),
            prompt_segments: crate::system_prompt::PromptSegments::default(),
            max_steps: 8,
            seed_transcript: None,
            interrupt_before,
            client_tools,
        }
    }

    /// Pin a temp `RECURSIVE_HOME` so the checkpoints wiring inside
    /// `build_agui_runtime` cannot touch the developer's real user dir.
    /// (Env is process-global; the lock prevents parallel tests from
    /// racing on it.)
    fn pinned_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let tmp = tempfile::tempdir().unwrap();
        let guard = crate::test_util::env_lock();
        std::env::set_var("RECURSIVE_HOME", tmp.path());
        (tmp, guard)
    }

    /// Client tools must be registered as callable stubs AND take the
    /// registry's permission-hook slot (deny-before-dispatch).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn build_agui_runtime_registers_client_tool_stubs_with_deny_hook() {
        let (_home, _guard) = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let client_tools = vec![ag::Tool {
            name: "get_weather".into(),
            description: "frontend-owned".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let (runtime, hooks) = build_agui_runtime(
            ws.path(),
            "t-hooks",
            runtime_deps(ToolRegistry::local(), &client_tools, &[]),
        )
        .expect("runtime builds");
        let registry = runtime.kernel().tools();
        assert!(
            registry.get("get_weather").is_some(),
            "client tool stub must be registered"
        );
        let hook = hooks.client_hook.expect("client hook installed");
        assert!(hooks.interrupt_hook.is_none(), "no interrupt_before given");
        let decision = hook
            .check("get_weather", &serde_json::json!({"city": "sf"}))
            .await;
        assert!(
            matches!(decision, crate::agent::PermissionDecision::Deny(_)),
            "client tools must be denied at dispatch, got {decision:?}"
        );
        let denied = hook.denied.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            denied.as_ref().map(|(n, _)| n.as_str()),
            Some("get_weather"),
            "first denied call must be recorded for the driver"
        );
    }

    /// `interrupt_before` installs the test-interrupt hook when no client
    /// tools are present; unrelated tools stay allowed.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn build_agui_runtime_installs_test_interrupt_hook_for_interrupt_before() {
        let (_home, _guard) = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let interrupt_before = vec!["Bash".to_string()];
        let (runtime, hooks) = build_agui_runtime(
            ws.path(),
            "t-interrupt",
            runtime_deps(ToolRegistry::local(), &[], &interrupt_before),
        )
        .expect("runtime builds");
        let _ = runtime;
        assert!(
            hooks.client_hook.is_none(),
            "no client tools → no client hook"
        );
        let hook = hooks.interrupt_hook.expect("interrupt hook installed");
        let deny = hook.check("Bash", &serde_json::json!({})).await;
        assert!(matches!(deny, crate::agent::PermissionDecision::Deny(_)));
        let allow = hook.check("Read", &serde_json::json!({})).await;
        assert!(matches!(allow, crate::agent::PermissionDecision::Allow));
    }

    /// With neither client tools nor interrupt_before, no hook is installed
    /// and the runtime still builds (checkpoints wiring is best-effort —
    /// no git repo in the temp workspace must not fail the build). Uses the
    /// standard tool set so a real tool name (Read) is present.
    #[test]
    fn build_agui_runtime_no_hooks_still_builds_without_git() {
        let (_home, _guard) = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let registry = crate::tools::build_standard_tools(ws.path(), &[], 60);
        let (runtime, hooks) =
            build_agui_runtime(ws.path(), "t-plain", runtime_deps(registry, &[], &[]))
                .expect("runtime builds without hooks and without git");
        assert!(hooks.client_hook.is_none());
        assert!(hooks.interrupt_hook.is_none());
        assert!(runtime.kernel().tools().get("Read").is_some());
    }

    /// Seed transcript is handed to the runtime (resume path).
    #[test]
    fn build_agui_runtime_seeds_transcript() {
        let ws = tempfile::tempdir().unwrap();
        let seed = vec![crate::message::Message::user("prior turn")];
        let mut deps = runtime_deps(ToolRegistry::local(), &[], &[]);
        deps.seed_transcript = Some(seed);
        let (runtime, _) = build_agui_runtime(ws.path(), "t-seed", deps).expect("runtime builds");
        let transcript = runtime.transcript();
        assert!(
            transcript.iter().any(|m| m.content == "prior turn"),
            "seed transcript must be present, got {transcript:?}"
        );
    }
}

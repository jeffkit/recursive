//! Pure UI state machine for `agui-tui`.
//!
//! [`App`] owns all in-memory state. Its `apply_event` and
//! `handle_key` methods take the inputs and return a list of
//! [`Command`]s describing what the outer event loop should do
//! (e.g. start a new run, exit). Keeping the state pure means the
//! tests in this crate can drive the App headless without any
//! terminal or HTTP I/O.

use std::collections::HashMap;

use agui_protocol::{Custom, Event, Resume, ResumeStatus};
use serde_json::{json, Value};

/// Which pane currently consumes keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Input,
    Messages,
}

/// Author of a UI message bubble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// One message in the messages pane. Assistant messages accumulate
/// their text from `TextMessageContent` deltas keyed by `message_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    /// Tool call IDs whose parent message is this one — rendered
    /// inline below the message body.
    pub tool_calls: Vec<String>,
    /// True until the corresponding `TextMessageEnd` arrives.
    pub streaming: bool,
}

/// State machine tracking each individual tool call across its
/// `Start` / `Args` / `End` / `Result` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallState {
    StreamingArgs,
    AwaitingResult,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: String,
    pub result: Option<String>,
    pub state: ToolCallState,
    pub parent_message_id: Option<String>,
}

/// An active permission prompt awaiting user input. Populated by a
/// `agui-tui/permission_request` Custom event; cleared once the user
/// presses `y`, `n`, or `Esc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub interrupt_id: String,
    pub tool: String,
    pub args_preview: String,
}

/// Sidebar state: identifiers, counters, and last-seen heartbeat.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub thread_id: String,
    pub run_id: Option<String>,
    pub steps: u32,
    pub tokens: u64,
    pub tool_counts: HashMap<String, u32>,
    /// Most recent `agui-tui/checkpoint_post` (turn, post_id).
    pub last_checkpoint: Option<(u64, String)>,
    /// Most recent `agui-tui/heartbeat` elapsed-ms reading.
    pub last_heartbeat_ms: Option<u64>,
    /// True between RunStarted and RunFinished/RunError.
    pub running: bool,
}

/// Outbound action the outer event loop should take in response to
/// the most recent input. The App itself never performs I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Send a fresh `RunAgentInput` containing the user's text.
    SendUserMessage { text: String },
    /// Send a `RunAgentInput` whose `resume` array answers the
    /// previously-displayed permission prompt.
    Resume { interrupt_id: String, approve: bool },
    /// User pressed Ctrl-C — outer loop should drop the receiver and
    /// exit.
    Quit,
}

/// All UI state. Pure: no I/O, no async, no terminal handles.
#[derive(Debug, Clone)]
pub struct App {
    pub messages: Vec<UiMessage>,
    pub tool_calls: HashMap<String, ToolCall>,
    /// Insertion-order list of tool-call IDs so rendering is stable.
    pub tool_call_order: Vec<String>,
    pub pending_permission: Option<PermissionPrompt>,
    pub state: SessionState,
    pub input_buffer: String,
    pub focus: Pane,
    /// Cursor position (byte offset) within `input_buffer` when
    /// focus is on the Input pane.
    pub cursor_pos: usize,
    /// Lines scrolled off the top of the messages pane. Only respected
    /// when focus is on Messages — Input focus auto-scrolls to bottom.
    pub messages_scroll: u16,
    /// Last status / error string surfaced to the user.
    pub status: Option<String>,
    /// Set once the user presses Ctrl-C; outer loop should observe.
    pub should_quit: bool,
}
impl App {
    /// Build a fresh App for `thread_id`. The run hasn't started yet
    /// — the first `RunStarted` from the server fills `run_id` etc.
    pub fn new(thread_id: String) -> Self {
        Self {
            messages: Vec::new(),
            tool_calls: HashMap::new(),
            tool_call_order: Vec::new(),
            pending_permission: None,
            state: SessionState {
                thread_id,
                ..Default::default()
            },
            input_buffer: String::new(),
            cursor_pos: 0,
            focus: Pane::Input,
            messages_scroll: 0,
            status: None,
            should_quit: false,
        }
    }

    /// Apply one server event, mutating state in place.
    pub fn apply_event(&mut self, ev: Event) {
        match ev {
            Event::RunStarted(rs) => {
                self.state.thread_id = rs.thread_id;
                self.state.run_id = Some(rs.run_id);
                self.state.running = true;
                self.state.last_heartbeat_ms = None;
            }
            Event::RunFinished(_) => {
                self.state.running = false;
            }
            Event::RunError(e) => {
                self.state.running = false;
                self.status = Some(format!("run error: {}", e.message));
            }
            Event::StepStarted(_) => {
                self.state.steps = self.state.steps.saturating_add(1);
            }
            Event::StepFinished(_) => {}
            Event::TextMessageStart(s) => {
                let role = match s.role.as_deref() {
                    Some("user") => MessageRole::User,
                    Some("system") => MessageRole::System,
                    _ => MessageRole::Assistant,
                };
                self.messages.push(UiMessage {
                    id: s.message_id,
                    role,
                    text: String::new(),
                    tool_calls: Vec::new(),
                    streaming: true,
                });
            }
            Event::TextMessageContent(c) => {
                if let Some(msg) = self.messages.iter_mut().find(|m| m.id == c.message_id) {
                    msg.text.push_str(&c.delta);
                } else {
                    // Server omitted Start; tolerate it by synthesising one.
                    self.messages.push(UiMessage {
                        id: c.message_id,
                        role: MessageRole::Assistant,
                        text: c.delta,
                        tool_calls: Vec::new(),
                        streaming: true,
                    });
                }
            }
            Event::TextMessageEnd(e) => {
                if let Some(msg) = self.messages.iter_mut().find(|m| m.id == e.message_id) {
                    msg.streaming = false;
                }
            }
            Event::TextMessageChunk(c) => {
                // Treat Chunk as Start+Content for the indicated id, or
                // a degenerate "no-id" assistant scratch buffer.
                let id = c.message_id.clone().unwrap_or_else(|| "chunk".to_string());
                if let Some(msg) = self.messages.iter_mut().find(|m| m.id == id) {
                    msg.text.push_str(&c.delta);
                } else {
                    self.messages.push(UiMessage {
                        id,
                        role: MessageRole::Assistant,
                        text: c.delta,
                        tool_calls: Vec::new(),
                        streaming: true,
                    });
                }
            }
            Event::ToolCallStart(s) => {
                if !self.tool_calls.contains_key(&s.tool_call_id) {
                    self.tool_call_order.push(s.tool_call_id.clone());
                }
                if let Some(parent) = s.parent_message_id.as_deref() {
                    if let Some(msg) = self.messages.iter_mut().find(|m| m.id == parent) {
                        if !msg.tool_calls.contains(&s.tool_call_id) {
                            msg.tool_calls.push(s.tool_call_id.clone());
                        }
                    }
                }
                *self
                    .state
                    .tool_counts
                    .entry(s.tool_call_name.clone())
                    .or_insert(0) += 1;
                self.tool_calls.insert(
                    s.tool_call_id.clone(),
                    ToolCall {
                        id: s.tool_call_id,
                        name: s.tool_call_name,
                        args: String::new(),
                        result: None,
                        state: ToolCallState::StreamingArgs,
                        parent_message_id: s.parent_message_id,
                    },
                );
            }
            Event::ToolCallArgs(a) => {
                if let Some(tc) = self.tool_calls.get_mut(&a.tool_call_id) {
                    tc.args.push_str(&a.delta);
                }
            }
            Event::ToolCallEnd(e) => {
                if let Some(tc) = self.tool_calls.get_mut(&e.tool_call_id) {
                    tc.state = ToolCallState::AwaitingResult;
                }
            }
            Event::ToolCallResult(r) => {
                if let Some(tc) = self.tool_calls.get_mut(&r.tool_call_id) {
                    tc.result = Some(r.content);
                    tc.state = ToolCallState::Completed;
                }
            }
            Event::StateSnapshot(_) | Event::StateDelta(_) | Event::MessagesSnapshot(_) => {
                // We don't model server-side shared state in v1.
            }
            Event::Custom(c) => self.apply_custom(c),
            Event::Raw(_) => {
                tracing::debug!("ignoring Raw event");
            }
        }
    }

    fn apply_custom(&mut self, c: Custom) {
        match c.name.as_str() {
            "agui-tui/permission_request" => {
                let interrupt_id = string_field(&c.value, "interruptId").unwrap_or_default();
                let tool = string_field(&c.value, "tool").unwrap_or_default();
                let args_preview = string_field(&c.value, "argsPreview").unwrap_or_default();
                self.pending_permission = Some(PermissionPrompt {
                    interrupt_id,
                    tool,
                    args_preview,
                });
            }
            "agui-tui/checkpoint_post" => {
                let turn = c.value.get("turn").and_then(|v| v.as_u64()).unwrap_or(0);
                let post_id = string_field(&c.value, "postId").unwrap_or_default();
                self.state.last_checkpoint = Some((turn, post_id));
            }
            "agui-tui/heartbeat" => {
                let ms = c.value.get("elapsedMs").and_then(|v| v.as_u64());
                self.state.last_heartbeat_ms = ms;
            }
            "agui-tui/file_artifact" => {
                let path = string_field(&c.value, "path").unwrap_or_default();
                tracing::info!(path, "file artifact emitted");
            }
            other => {
                tracing::debug!(name = other, "ignoring unknown Custom event");
            }
        }
    }

    /// React to a single key press. Returns the actions the outer
    /// loop should perform (typically zero or one).
    pub fn handle_key(&mut self, key: KeyInput) -> Vec<Command> {
        // Ctrl-C always exits, regardless of focus or pending state.
        if key.ctrl && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return vec![Command::Quit];
        }

        // Ctrl-B / Ctrl-F cursor movement when the input pane is focused.
        if key.ctrl && self.focus == Pane::Input {
            match key.code {
                KeyCode::Char('b') => {
                    self.cursor_pos = self.cursor_pos.saturating_sub(1);
                    return vec![];
                }
                KeyCode::Char('f') => {
                    let len = self.input_buffer.len();
                    if self.cursor_pos < len {
                        self.cursor_pos += 1;
                    }
                    return vec![];
                }
                _ => {}
            }
        }
        // Permission prompt absorbs y/n/Esc *first* — these letters
        // must not also land in the input buffer.
        if let Some(prompt) = self.pending_permission.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.pending_permission = None;
                    return vec![Command::Resume {
                        interrupt_id: prompt.interrupt_id,
                        approve: true,
                    }];
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.pending_permission = None;
                    return vec![Command::Resume {
                        interrupt_id: prompt.interrupt_id,
                        approve: false,
                    }];
                }
                KeyCode::Esc => {
                    // Dismiss without responding — the run stays paused
                    // server-side until a future Enter triggers a new
                    // run that may resume or override.
                    self.pending_permission = None;
                    return vec![];
                }
                _ => {
                    // Fall through to normal input handling so the
                    // user can keep typing while a prompt is up.
                }
            }
        }

        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Pane::Input => Pane::Messages,
                    Pane::Messages => Pane::Input,
                };
                vec![]
            }
            KeyCode::Enter => {
                if self.input_buffer.trim().is_empty() {
                    return vec![];
                }
                let text = std::mem::take(&mut self.input_buffer);
                vec![Command::SendUserMessage { text }]
            }
            KeyCode::Backspace if self.focus == Pane::Input => {
                self.input_buffer.pop();
                vec![]
            }
            KeyCode::Char(ch) if self.focus == Pane::Input => {
                self.input_buffer.push(ch);
                vec![]
            }
            KeyCode::Up if self.focus == Pane::Messages => {
                self.messages_scroll = self.messages_scroll.saturating_add(1);
                vec![]
            }
            KeyCode::Down if self.focus == Pane::Messages => {
                self.messages_scroll = self.messages_scroll.saturating_sub(1);
                vec![]
            }
            _ => vec![],
        }
    }

    /// Push a synthetic UI message recording the user's prompt — done
    /// locally so we have something to render before the server's
    /// echoes (if any) come back.
    pub fn record_user_message(&mut self, id: String, text: String) {
        self.messages.push(UiMessage {
            id,
            role: MessageRole::User,
            text,
            tool_calls: Vec::new(),
            streaming: false,
        });
    }
}

/// Build the AG-UI `resume` payload for a permission decision.
pub fn resume_payload(interrupt_id: String, approve: bool) -> Vec<Resume> {
    vec![Resume {
        interrupt_id,
        status: ResumeStatus::Resolved,
        payload: Some(json!({ "approved": approve })),
    }]
}

fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|s| s.as_str()).map(|s| s.to_string())
}

// --- Key abstraction ---
//
// We avoid leaking `crossterm::event::KeyEvent` into the App so the
// keybinding tests don't have to construct full crossterm events
// (which carry `KeyEventKind`, modifiers, etc.). The outer loop maps
// raw crossterm keys into this shape.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub code: KeyCode,
    pub ctrl: bool,
}

impl KeyInput {
    pub fn new(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }
    pub fn ctrl(code: KeyCode) -> Self {
        Self { code, ctrl: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agui_protocol::{
        BaseEvent, RunStarted, TextMessageContent, TextMessageEnd, TextMessageStart, ToolCallArgs,
        ToolCallEnd, ToolCallResult, ToolCallStart,
    };
    use serde_json::json;

    fn app() -> App {
        App::new("t-1".into())
    }

    fn run_started(thread: &str, run: &str) -> Event {
        Event::RunStarted(RunStarted {
            thread_id: thread.into(),
            run_id: run.into(),
            base: BaseEvent::default(),
        })
    }

    fn text_start(id: &str) -> Event {
        Event::TextMessageStart(TextMessageStart {
            message_id: id.into(),
            role: Some("assistant".into()),
            base: BaseEvent::default(),
        })
    }
    fn text_content(id: &str, delta: &str) -> Event {
        Event::TextMessageContent(TextMessageContent {
            message_id: id.into(),
            delta: delta.into(),
            base: BaseEvent::default(),
        })
    }
    fn text_end(id: &str) -> Event {
        Event::TextMessageEnd(TextMessageEnd {
            message_id: id.into(),
            base: BaseEvent::default(),
        })
    }

    #[test]
    fn streamed_assistant_message_concatenates() {
        let mut a = app();
        a.apply_event(text_start("m1"));
        a.apply_event(text_content("m1", "hel"));
        a.apply_event(text_content("m1", "lo "));
        a.apply_event(text_content("m1", "world"));
        a.apply_event(text_end("m1"));
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].text, "hello world");
        assert!(!a.messages[0].streaming);
    }

    #[test]
    fn run_started_updates_state() {
        let mut a = app();
        a.apply_event(run_started("thr-1", "run-7"));
        assert_eq!(a.state.thread_id, "thr-1");
        assert_eq!(a.state.run_id.as_deref(), Some("run-7"));
        assert!(a.state.running);
    }

    #[test]
    fn tool_call_lifecycle_records_state_and_count() {
        let mut a = app();
        a.apply_event(Event::ToolCallStart(ToolCallStart {
            tool_call_id: "tc-1".into(),
            tool_call_name: "read_file".into(),
            parent_message_id: None,
            base: BaseEvent::default(),
        }));
        a.apply_event(Event::ToolCallArgs(ToolCallArgs {
            tool_call_id: "tc-1".into(),
            delta: "{\"path\":\"a\"}".into(),
            base: BaseEvent::default(),
        }));
        a.apply_event(Event::ToolCallEnd(ToolCallEnd {
            tool_call_id: "tc-1".into(),
            base: BaseEvent::default(),
        }));
        a.apply_event(Event::ToolCallResult(ToolCallResult {
            tool_call_id: "tc-1".into(),
            message_id: "m-tc-1".into(),
            content: "ok".into(),
            role: None,
            base: BaseEvent::default(),
        }));

        let tc = a.tool_calls.get("tc-1").expect("tool call recorded");
        assert_eq!(tc.name, "read_file");
        assert_eq!(tc.args, "{\"path\":\"a\"}");
        assert_eq!(tc.result.as_deref(), Some("ok"));
        assert_eq!(tc.state, ToolCallState::Completed);
        assert_eq!(a.state.tool_counts.get("read_file"), Some(&1));
    }

    fn permission_event() -> Event {
        Event::Custom(Custom {
            name: "agui-tui/permission_request".into(),
            value: json!({
                "interruptId": "i-1",
                "tool": "run_shell",
                "argsPreview": "cargo test",
            }),
            base: BaseEvent::default(),
        })
    }

    #[test]
    fn permission_request_populates_prompt() {
        let mut a = app();
        a.apply_event(permission_event());
        let p = a.pending_permission.as_ref().expect("prompt set");
        assert_eq!(p.interrupt_id, "i-1");
        assert_eq!(p.tool, "run_shell");
        assert_eq!(p.args_preview, "cargo test");
    }

    #[test]
    fn checkpoint_post_updates_sidebar() {
        let mut a = app();
        a.apply_event(Event::Custom(Custom {
            name: "agui-tui/checkpoint_post".into(),
            value: json!({"turn": 3, "postId": "abc123"}),
            base: BaseEvent::default(),
        }));
        assert_eq!(a.state.last_checkpoint, Some((3, "abc123".into())));
    }

    #[test]
    fn heartbeat_updates_running_indicator() {
        let mut a = app();
        a.apply_event(Event::Custom(Custom {
            name: "agui-tui/heartbeat".into(),
            value: json!({"elapsedMs": 1500}),
            base: BaseEvent::default(),
        }));
        assert_eq!(a.state.last_heartbeat_ms, Some(1500));
    }

    #[test]
    fn y_approves_permission_and_clears_prompt() {
        let mut a = app();
        a.apply_event(permission_event());
        let cmds = a.handle_key(KeyInput::new(KeyCode::Char('y')));
        assert!(a.pending_permission.is_none());
        assert_eq!(
            cmds,
            vec![Command::Resume {
                interrupt_id: "i-1".into(),
                approve: true
            }]
        );
    }

    #[test]
    fn n_rejects_permission_and_clears_prompt() {
        let mut a = app();
        a.apply_event(permission_event());
        let cmds = a.handle_key(KeyInput::new(KeyCode::Char('n')));
        assert!(a.pending_permission.is_none());
        assert_eq!(
            cmds,
            vec![Command::Resume {
                interrupt_id: "i-1".into(),
                approve: false
            }]
        );
    }

    #[test]
    fn esc_dismisses_permission_without_responding() {
        let mut a = app();
        a.apply_event(permission_event());
        let cmds = a.handle_key(KeyInput::new(KeyCode::Esc));
        assert!(a.pending_permission.is_none());
        assert!(cmds.is_empty());
    }

    #[test]
    fn enter_sends_input_and_clears_buffer() {
        let mut a = app();
        a.input_buffer = "hello".into();
        let cmds = a.handle_key(KeyInput::new(KeyCode::Enter));
        assert_eq!(a.input_buffer, "");
        assert_eq!(
            cmds,
            vec![Command::SendUserMessage {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn enter_with_blank_buffer_does_nothing() {
        let mut a = app();
        a.input_buffer = "   ".into();
        let cmds = a.handle_key(KeyInput::new(KeyCode::Enter));
        // Buffer is preserved; no command emitted.
        assert_eq!(a.input_buffer, "   ");
        assert!(cmds.is_empty());
    }

    #[test]
    fn typing_appends_to_input_buffer() {
        let mut a = app();
        a.handle_key(KeyInput::new(KeyCode::Char('h')));
        a.handle_key(KeyInput::new(KeyCode::Char('i')));
        assert_eq!(a.input_buffer, "hi");
    }

    #[test]
    fn backspace_removes_last_char_in_input() {
        let mut a = app();
        a.input_buffer = "abc".into();
        a.handle_key(KeyInput::new(KeyCode::Backspace));
        assert_eq!(a.input_buffer, "ab");
    }

    #[test]
    fn tab_toggles_focus() {
        let mut a = app();
        assert_eq!(a.focus, Pane::Input);
        a.handle_key(KeyInput::new(KeyCode::Tab));
        assert_eq!(a.focus, Pane::Messages);
        a.handle_key(KeyInput::new(KeyCode::Tab));
        assert_eq!(a.focus, Pane::Input);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut a = app();
        let cmds = a.handle_key(KeyInput::ctrl(KeyCode::Char('c')));
        assert!(a.should_quit);
        assert_eq!(cmds, vec![Command::Quit]);
    }

    #[test]
    fn typing_y_during_permission_does_not_reach_input() {
        let mut a = app();
        a.apply_event(permission_event());
        // 'y' goes to permission, not the input buffer.
        a.handle_key(KeyInput::new(KeyCode::Char('y')));
        assert_eq!(a.input_buffer, "");
    }

    #[test]
    fn resume_payload_serializes_with_interrupt_id_and_approved() {
        let payload = resume_payload("i-9".into(), true);
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0].interrupt_id, "i-9");
        assert_eq!(payload[0].status, ResumeStatus::Resolved);
        assert_eq!(payload[0].payload.as_ref().and_then(|v| v.get("approved")), Some(&json!(true)));
    }
}

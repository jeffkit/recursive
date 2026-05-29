//! Application state for the Recursive TUI.
//!
//! [`App`] owns everything visible to the user: the transcript blocks,
//! the input buffer, the current screen, scroll position, and bookkeeping
//! for streaming, usage, and per-turn timing.
//!
//! Rendering lives in [`crate::ui`]; this file is *only* state plus the
//! reducers that mutate it in response to [`UiEvent`]s and key events.

use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::events::{UiEvent, UserAction};

// ──────────────────────────────────────────────────────────────────────
// Screens
// ──────────────────────────────────────────────────────────────────────

/// Which top-level screen is currently rendered.
#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Splash,
    Chat,
    PlanReview { plan_text: String },
}

// ──────────────────────────────────────────────────────────────────────
// Transcript model
// ──────────────────────────────────────────────────────────────────────

/// One unit of context within a [`Diff`] block.
///
/// We model only the three line kinds we need to colour-code: addition,
/// removal, and unchanged context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Add,
    Remove,
    Context,
}

/// A single line inside a diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// A grouped sequence of diff lines belonging to one logical change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

/// One renderable transcript block.
///
/// The chat screen renders a `Vec<TranscriptBlock>` in order, with one
/// blank line between adjacent blocks. Each variant has a corresponding
/// renderer in [`crate::ui::transcript`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptBlock {
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
        latency_ms: Option<u64>,
    },
    ToolCall {
        id: String,
        name: String,
        args_preview: String,
    },
    ToolResult {
        id: String,
        name: String,
        success: bool,
        output: String,
        expanded: bool,
    },
    Diff {
        path: String,
        hunks: Vec<DiffHunk>,
    },
    Compacted {
        removed: usize,
        kept: usize,
    },
    System {
        text: String,
    },
    Error {
        text: String,
    },
}

// ──────────────────────────────────────────────────────────────────────
// Usage / turn telemetry
// ──────────────────────────────────────────────────────────────────────

/// Token usage and timing accumulated across the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageStats {
    /// Most recent per-turn input tokens.
    pub input_tokens: u64,
    /// Most recent per-turn output tokens.
    pub output_tokens: u64,
    /// Cumulative input tokens across all turns.
    pub total_input: u64,
    /// Cumulative output tokens across all turns.
    pub total_output: u64,
    /// Most recent LLM round-trip latency, in milliseconds.
    pub last_latency_ms: u64,
}

impl UsageStats {
    /// Fold a `Usage` event into the stats. Treats incoming numbers as
    /// per-turn deltas and accumulates them into the running totals.
    pub fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input = self.total_input.saturating_add(input_tokens);
        self.total_output = self.total_output.saturating_add(output_tokens);
    }
}

/// State of the currently in-flight turn (if any).
#[derive(Clone, Debug, PartialEq)]
pub struct TurnState {
    pub running: bool,
    pub started_at: Option<Instant>,
    pub spinner_verb: &'static str,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            running: false,
            started_at: None,
            spinner_verb: "Thinking",
        }
    }
}

impl TurnState {
    pub fn start(&mut self) {
        self.running = true;
        self.started_at = Some(Instant::now());
        self.spinner_verb = "Thinking";
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.started_at = None;
        self.spinner_verb = "Thinking";
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pricing table
// ──────────────────────────────────────────────────────────────────────

/// (input_per_1k, output_per_1k) USD prices for the four models the
/// goal explicitly calls out. Models not in this table render no `$…`
/// segment in the status bar.
pub fn default_pricing_table() -> HashMap<&'static str, (f64, f64)> {
    let mut m = HashMap::new();
    // Source: published list prices as of mid-2025; not authoritative.
    m.insert("deepseek-chat", (0.00027, 0.00110));
    m.insert("gpt-4o", (0.00250, 0.01000));
    m.insert("gpt-4o-mini", (0.00015, 0.00060));
    m.insert("glm-4-plus", (0.00050, 0.00150));
    m.insert("claude-sonnet", (0.00300, 0.01500));
    m
}

/// Return the model name to display in the status bar based on env vars.
pub fn detect_model_name() -> String {
    std::env::var("RECURSIVE_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

/// Compute estimated cost in USD given accumulated tokens and a
/// pricing table. Returns `None` when the model is not known.
pub fn estimate_cost(
    model: &str,
    total_input: u64,
    total_output: u64,
    pricing: &HashMap<&'static str, (f64, f64)>,
) -> Option<f64> {
    pricing.get(model).map(|(in_rate, out_rate)| {
        (total_input as f64) / 1000.0 * in_rate + (total_output as f64) / 1000.0 * out_rate
    })
}

// ──────────────────────────────────────────────────────────────────────
// Top-level App
// ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub input: String,
    pub blocks: Vec<TranscriptBlock>,
    pub should_quit: bool,
    pub session_id: Option<String>,
    pub connected: bool,
    pub scroll_offset: u16,
    pub screen: AppScreen,
    pub splash_start: Instant,
    pub usage: UsageStats,
    pub turn: TurnState,
    pub turn_count: u64,
    pub pending_latency_ms: Option<u64>,
    pub pricing: HashMap<&'static str, (f64, f64)>,
    pub model_name: String,
    pub spinner_frame: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            blocks: vec![TranscriptBlock::System {
                text: "Welcome to Recursive TUI. Type a message and press Enter.".into(),
            }],
            should_quit: false,
            session_id: None,
            connected: false,
            scroll_offset: 0,
            screen: AppScreen::Splash,
            splash_start: Instant::now(),
            usage: UsageStats::default(),
            turn: TurnState::default(),
            turn_count: 0,
            pending_latency_ms: None,
            pricing: default_pricing_table(),
            model_name: detect_model_name(),
            spinner_frame: 0,
        }
    }

    /// Process one key event. Returns an optional [`UserAction`] that
    /// the caller must forward to the backend worker.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        // ── PlanReview screen ────────────────────────────────────────
        if let AppScreen::PlanReview { ref plan_text } = self.screen {
            let plan_text = plan_text.clone();
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.blocks.push(TranscriptBlock::System {
                        text: "Plan approved".into(),
                    });
                    self.blocks.push(TranscriptBlock::Assistant {
                        text: plan_text,
                        streaming: false,
                        latency_ms: None,
                    });
                    self.screen = AppScreen::Chat;
                    self.scroll_to_bottom();
                    return Some(UserAction::ConfirmPlan);
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.blocks.push(TranscriptBlock::System {
                        text: "Plan rejected".into(),
                    });
                    self.screen = AppScreen::Chat;
                    self.scroll_to_bottom();
                    return Some(UserAction::RejectPlan(String::new()));
                }
                KeyCode::Char('e') => {
                    self.input = plan_text;
                    self.screen = AppScreen::Chat;
                    return None;
                }
                _ => return None,
            }
        }

        // ── Ctrl+E: toggle the most recent ToolResult / Diff block ──
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.toggle_last_expandable();
            return None;
        }

        // ── Chat screen ──────────────────────────────────────────────
        match key.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let msg = self.input.clone();
                    self.blocks
                        .push(TranscriptBlock::User { text: msg.clone() });
                    self.input.clear();
                    self.scroll_to_bottom();
                    self.start_turn();
                    Some(UserAction::SendMessage(msg))
                } else {
                    None
                }
            }
            KeyCode::Up if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                None
            }
            KeyCode::Down if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                None
            }
            KeyCode::PageUp if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                None
            }
            KeyCode::PageDown if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                None
            }
            KeyCode::Char('q') if self.input.is_empty() => {
                self.should_quit = true;
                None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Esc => {
                self.should_quit = true;
                None
            }
            _ => None,
        }
    }

    /// Apply an event coming from the backend worker.
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantPartial { text } => {
                self.append_streaming_assistant(&text);
            }
            UiEvent::AssistantMessage { content } => {
                let first_line = content.lines().next().unwrap_or("");
                let lower = first_line.to_lowercase();
                if lower.starts_with("plan:") || lower.starts_with("## plan") {
                    self.screen = AppScreen::PlanReview { plan_text: content };
                } else {
                    self.finalise_streaming_assistant(content);
                }
            }
            UiEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let preview = preview_args(&arguments);
                // Try to also synthesise a Diff block when the tool
                // looks like an edit. For apply_patch we'll create the
                // Diff alongside the ToolCall; for write_file we wait
                // for the ToolResult so the byte count is accurate.
                self.blocks.push(TranscriptBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args_preview: preview,
                });
                if name == "apply_patch" {
                    if let Some((path, hunks)) = parse_apply_patch_input(&arguments) {
                        self.blocks.push(TranscriptBlock::Diff { path, hunks });
                    }
                }
                // Refine spinner verb based on tool category.
                self.turn.spinner_verb = verb_for_tool(&name);
            }
            UiEvent::ToolResult {
                id,
                name,
                output,
                success,
            } => {
                // For write_file, render a synthesised Diff stub
                // ("Created/Updated path (N bytes)") instead of a
                // ToolResult block. apply_patch already emitted a
                // Diff block at ToolCall time, so we still want a
                // ToolResult for it (the success ✓/✗ marker).
                if name == "write_file" && success {
                    if let Some(path) = extract_write_file_path_from_result(&output) {
                        self.blocks.push(TranscriptBlock::Diff {
                            path,
                            hunks: vec![],
                        });
                        return;
                    }
                }
                self.blocks.push(TranscriptBlock::ToolResult {
                    id,
                    name,
                    success,
                    output,
                    expanded: false,
                });
            }
            UiEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.usage.record(input_tokens, output_tokens);
            }
            UiEvent::Latency { llm_ms } => {
                self.usage.last_latency_ms = llm_ms;
                self.pending_latency_ms = Some(llm_ms);
                // Stamp any in-flight streaming assistant block.
                if let Some(TranscriptBlock::Assistant {
                    streaming: true,
                    latency_ms,
                    ..
                }) = self.blocks.last_mut()
                {
                    *latency_ms = Some(llm_ms);
                }
            }
            UiEvent::Compacted { removed, kept } => {
                self.blocks
                    .push(TranscriptBlock::Compacted { removed, kept });
            }
            UiEvent::TurnFinished => {
                // Make sure the last streaming assistant block is
                // closed in case the provider never emitted a final
                // AssistantText (some providers stream tokens then
                // stop without a synthesised final).
                if let Some(TranscriptBlock::Assistant { streaming, .. }) = self.blocks.last_mut() {
                    *streaming = false;
                }
                self.turn.finish();
                self.pending_latency_ms = None;
            }
            UiEvent::Error { message } => {
                self.blocks.push(TranscriptBlock::Error {
                    text: format!("Error: {message}"),
                });
            }
        }
        self.scroll_to_bottom();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    fn start_turn(&mut self) {
        self.turn.start();
        self.turn_count = self.turn_count.saturating_add(1);
    }

    fn append_streaming_assistant(&mut self, chunk: &str) {
        if let Some(TranscriptBlock::Assistant {
            text,
            streaming: true,
            ..
        }) = self.blocks.last_mut()
        {
            text.push_str(chunk);
        } else {
            self.blocks.push(TranscriptBlock::Assistant {
                text: chunk.to_string(),
                streaming: true,
                latency_ms: self.pending_latency_ms,
            });
        }
    }

    fn finalise_streaming_assistant(&mut self, content: String) {
        if let Some(TranscriptBlock::Assistant {
            text,
            streaming,
            latency_ms,
        }) = self.blocks.last_mut()
        {
            if *streaming {
                *text = content;
                *streaming = false;
                if latency_ms.is_none() {
                    *latency_ms = self.pending_latency_ms;
                }
                return;
            }
        }
        self.blocks.push(TranscriptBlock::Assistant {
            text: content,
            streaming: false,
            latency_ms: self.pending_latency_ms,
        });
    }

    /// Toggle the most recent ToolResult or Diff block's expanded flag.
    fn toggle_last_expandable(&mut self) {
        for block in self.blocks.iter_mut().rev() {
            if let TranscriptBlock::ToolResult { expanded, .. } = block {
                *expanded = !*expanded;
                return;
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Produce a short preview of a JSON-encoded arguments string.
///
/// Picks up to two top-level fields, formats them as `k=v`, and clamps
/// to ~60 characters with an ellipsis.
pub fn preview_args(arguments: &str) -> String {
    let parsed: Result<Value, _> = serde_json::from_str(arguments);
    let Ok(Value::Object(map)) = parsed else {
        // Not JSON-y; just clamp the raw string.
        return clamp(arguments, 60);
    };

    let mut parts = Vec::new();
    for (k, v) in map.iter().take(2) {
        let v_str = match v {
            Value::String(s) => format!("\"{}\"", clamp(s, 30)),
            other => clamp(&other.to_string(), 30),
        };
        parts.push(format!("{k}={v_str}"));
    }
    clamp(&parts.join(" "), 60)
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Pick a spinner verb based on the tool name.
pub fn verb_for_tool(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" | "search_files" => "Reading",
        "apply_patch" | "write_file" => "Editing",
        "run_shell" => "Running",
        _ => "Calling tool",
    }
}

/// Parse a V4A patch envelope from an `apply_patch` arguments JSON.
///
/// Returns `(path, hunks)` for the first `*** Update File:` /
/// `*** Add File:` block found, or `None` if the input is not parseable
/// as a V4A patch.
pub fn parse_apply_patch_input(arguments: &str) -> Option<(String, Vec<DiffHunk>)> {
    let v: Value = serde_json::from_str(arguments).ok()?;
    let input = v.get("input")?.as_str()?;
    parse_v4a_patch(input)
}

/// Pure parser for a V4A patch string.
pub fn parse_v4a_patch(input: &str) -> Option<(String, Vec<DiffHunk>)> {
    let mut path: Option<String> = None;
    let mut current = Vec::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();

    for line in input.lines() {
        if let Some(rest) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
        {
            if path.is_some() {
                // Multiple update sections — only the first is used,
                // per goal scope.
                break;
            }
            path = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("*** Begin Patch")
            || line.starts_with("*** End Patch")
            || line.starts_with("*** End of File")
        {
            continue;
        }
        if path.is_none() {
            continue;
        }
        // @@ anchor lines start a new hunk.
        if let Some(stripped) = line.strip_prefix("@@") {
            if !current.is_empty() {
                hunks.push(DiffHunk {
                    lines: std::mem::take(&mut current),
                });
            }
            let text = stripped.trim_start().to_string();
            if !text.is_empty() {
                current.push(DiffLine {
                    kind: DiffLineKind::Context,
                    text,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            current.push(DiffLine {
                kind: DiffLineKind::Add,
                text: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            current.push(DiffLine {
                kind: DiffLineKind::Remove,
                text: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix(' ') {
            current.push(DiffLine {
                kind: DiffLineKind::Context,
                text: rest.to_string(),
            });
        }
    }
    if !current.is_empty() {
        hunks.push(DiffHunk { lines: current });
    }

    let path = path?;
    if hunks.is_empty() {
        return None;
    }
    Some((path, hunks))
}

/// Best-effort path extraction from a write_file ToolResult output.
///
/// The `WriteFile` tool emits something like
/// `"Wrote 42 bytes to crates/foo/bar.rs"`. We parse that pattern and
/// fall back to the entire trimmed output if it doesn't match.
fn extract_write_file_path_from_result(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if let Some(idx) = trimmed.rfind(" to ") {
        let candidate = &trimmed[idx + 4..];
        if !candidate.is_empty() {
            return Some(candidate.to_string());
        }
    }
    None
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    // ── construction ────────────────────────────────────────────────

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input.is_empty());
        assert!(!app.blocks.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn app_new_starts_in_splash_screen() {
        let app = App::new();
        assert_eq!(app.screen, AppScreen::Splash);
    }

    #[test]
    fn splash_auto_transitions_after_elapsed() {
        let app = App::new();
        assert!(app.splash_start.elapsed() < Duration::from_secs(2));
        assert_eq!(app.screen, AppScreen::Splash);
    }

    #[test]
    fn app_no_session_shows_system_message() {
        let app = App::new();
        assert!(app.session_id.is_none());
        assert!(
            matches!(&app.blocks[0], TranscriptBlock::System { text } if text.contains("Welcome"))
        );
    }

    // ── streaming assistant ────────────────────────────────────────

    #[test]
    fn transcript_apply_partial_token_appends_to_streaming_assistant() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantPartial { text: "hel".into() });
        app.handle_ui_event(UiEvent::AssistantPartial { text: "lo".into() });

        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "hello");
                assert!(*streaming);
            }
            other => panic!("expected streaming Assistant, got {other:?}"),
        }
    }

    #[test]
    fn transcript_apply_assistant_text_finalizes_streaming() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantPartial { text: "hel".into() });
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello world".into(),
        });

        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "hello world");
                assert!(!*streaming);
            }
            other => panic!("expected finalised Assistant, got {other:?}"),
        }
    }

    #[test]
    fn transcript_assistant_text_without_prior_stream_creates_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "single shot".into(),
        });
        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "single shot");
                assert!(!*streaming);
            }
            other => panic!("expected non-streaming Assistant, got {other:?}"),
        }
    }

    // ── tool call / result ─────────────────────────────────────────

    #[test]
    fn tool_call_and_result_pair_by_id() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::ToolCall {
            id: "abc".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/agent.rs"}"#.into(),
        });
        app.handle_ui_event(UiEvent::ToolResult {
            id: "abc".into(),
            name: "read_file".into(),
            output: "ok".into(),
            success: true,
        });

        let mut call_id = None;
        let mut result_id = None;
        for b in &app.blocks {
            match b {
                TranscriptBlock::ToolCall { id, .. } => call_id = Some(id.clone()),
                TranscriptBlock::ToolResult { id, .. } => result_id = Some(id.clone()),
                _ => {}
            }
        }
        assert_eq!(call_id.as_deref(), Some("abc"));
        assert_eq!(result_id.as_deref(), Some("abc"));
    }

    #[test]
    fn tool_call_args_preview_extracts_path() {
        let preview = preview_args(r#"{"path":"src/agent.rs"}"#);
        assert!(preview.contains("path"));
        assert!(preview.contains("src/agent.rs"));
    }

    #[test]
    fn apply_patch_call_emits_diff_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let patch = "*** Begin Patch\n*** Update File: src/foo.rs\n@@ pub fn bar()\n pub fn bar() {\n-    let x = 1;\n+    let x = 2;\n }\n*** End Patch";
        let arguments = serde_json::json!({"input": patch}).to_string();
        app.handle_ui_event(UiEvent::ToolCall {
            id: "1".into(),
            name: "apply_patch".into(),
            arguments,
        });
        let has_diff = app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::Diff { path, .. } if path == "src/foo.rs"));
        assert!(has_diff, "expected Diff block, got {:?}", app.blocks);
    }

    #[test]
    fn write_file_result_renders_diff_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"src/new.rs","contents":"x"}"#.into(),
        });
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "write_file".into(),
            output: "Wrote 42 bytes to src/new.rs".into(),
            success: true,
        });
        let has_diff = app.blocks.iter().any(
            |b| matches!(b, TranscriptBlock::Diff { path, .. } if path.contains("src/new.rs")),
        );
        assert!(has_diff, "expected Diff block from write_file");
    }

    // ── compacted ──────────────────────────────────────────────────

    #[test]
    fn compacted_event_creates_compacted_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Compacted {
            removed: 12,
            kept: 1,
        });
        assert!(matches!(
            app.blocks.last(),
            Some(TranscriptBlock::Compacted {
                removed: 12,
                kept: 1
            })
        ));
    }

    // ── usage stats ────────────────────────────────────────────────

    #[test]
    fn usage_stats_accumulate_across_turns() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Usage {
            input_tokens: 100,
            output_tokens: 50,
        });
        app.handle_ui_event(UiEvent::Usage {
            input_tokens: 30,
            output_tokens: 20,
        });
        assert_eq!(app.usage.total_input, 130);
        assert_eq!(app.usage.total_output, 70);
        assert_eq!(app.usage.input_tokens, 30);
        assert_eq!(app.usage.output_tokens, 20);
    }

    // ── Ctrl+E ─────────────────────────────────────────────────────

    #[test]
    fn ctrl_e_toggles_expanded_on_last_tool_result() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            output: "long output".into(),
            success: true,
        });
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolResult { expanded, .. }) => assert!(*expanded),
            other => panic!("expected ToolResult, got {other:?}"),
        }
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolResult { expanded, .. }) => assert!(!*expanded),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    // ── pricing / model detection ──────────────────────────────────

    #[test]
    fn pricing_table_includes_required_models() {
        let p = default_pricing_table();
        assert!(p.contains_key("deepseek-chat"));
        assert!(p.contains_key("gpt-4o"));
        assert!(p.contains_key("glm-4-plus"));
        assert!(p.contains_key("claude-sonnet"));
    }

    #[test]
    fn estimate_cost_for_known_model() {
        let p = default_pricing_table();
        let c = estimate_cost("gpt-4o-mini", 1000, 1000, &p).unwrap();
        // 1000 in @ 0.00015 + 1000 out @ 0.0006 = 0.00015 + 0.0006 = 0.00075
        assert!((c - 0.00075).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_unknown_model_is_none() {
        let p = default_pricing_table();
        assert!(estimate_cost("foo-9000", 1000, 1000, &p).is_none());
    }

    // ── chat key handling ──────────────────────────────────────────

    #[test]
    fn enter_moves_input_to_blocks() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hello".to_string();
        let action = app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::User { text } if text == "hello")));
        assert!(matches!(action, Some(UserAction::SendMessage(s)) if s == "hello"));
    }

    #[test]
    fn enter_starts_a_turn() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hi".to_string();
        let _ = app.handle_key(key(KeyCode::Enter));
        assert!(app.turn.running);
        assert_eq!(app.turn_count, 1);
    }

    #[test]
    fn turn_finished_stops_turn() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hi".into();
        let _ = app.handle_key(key(KeyCode::Enter));
        app.handle_ui_event(UiEvent::TurnFinished);
        assert!(!app.turn.running);
    }

    #[test]
    fn esc_sets_should_quit() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn char_appends_to_input() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Char('h')));
        let _ = app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hello".to_string();
        let _ = app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "hell");
    }

    #[test]
    fn scroll_up_increases_offset() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        for i in 0..30 {
            app.blocks.push(TranscriptBlock::System {
                text: format!("msg {i}"),
            });
        }
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 1);
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 2);
    }

    #[test]
    fn scroll_down_stops_at_zero() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 2;
        let _ = app.handle_key(key(KeyCode::Down));
        let _ = app.handle_key(key(KeyCode::Down));
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn page_up_scrolls_by_ten() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 10);
    }

    #[test]
    fn page_down_scrolls_by_ten() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 15;
        let _ = app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll_offset, 5);
    }

    #[test]
    fn scroll_keys_ignored_when_input_not_empty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "typing".to_string();
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn new_message_resets_scroll_to_bottom() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 5;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello".into(),
        });
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn error_event_pushes_error_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Error {
            message: "boom".into(),
        });
        assert!(matches!(
            app.blocks.last(),
            Some(TranscriptBlock::Error { text }) if text.contains("boom")
        ));
    }

    // ── Plan Mode ──────────────────────────────────────────────────

    #[test]
    fn plan_message_triggers_plan_review() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "## Plan\n1. Do thing A".into(),
        });
        assert!(matches!(app.screen, AppScreen::PlanReview { .. }));
    }

    #[test]
    fn plan_message_with_plan_colon_triggers_review() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Plan: refactor".into(),
        });
        assert!(matches!(app.screen, AppScreen::PlanReview { .. }));
    }

    #[test]
    fn non_plan_message_stays_in_chat() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Hello, I can help.".into(),
        });
        assert_eq!(app.screen, AppScreen::Chat);
    }

    #[test]
    fn plan_approve_returns_to_chat() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "## Plan\nDo X".into(),
        };
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { text } if text == "Plan approved")));
        assert!(app.blocks.iter().any(
            |b| matches!(b, TranscriptBlock::Assistant { text, .. } if text == "## Plan\nDo X")
        ));
    }

    #[test]
    fn plan_reject_returns_to_chat() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "Plan: do".into(),
        };
        let action = app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::RejectPlan(_))));
    }

    #[test]
    fn plan_edit_prefills_input() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "edit me".into(),
        };
        let action = app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.input, "edit me");
        assert!(action.is_none());
    }

    // ── verb / patch parser ────────────────────────────────────────

    #[test]
    fn verb_for_tool_categorises_tools() {
        assert_eq!(verb_for_tool("read_file"), "Reading");
        assert_eq!(verb_for_tool("apply_patch"), "Editing");
        assert_eq!(verb_for_tool("run_shell"), "Running");
        assert_eq!(verb_for_tool("custom_xyz"), "Calling tool");
    }

    #[test]
    fn parse_v4a_patch_extracts_path_and_pm_lines() {
        let patch = "*** Begin Patch\n*** Update File: src/foo.rs\n@@ pub fn bar()\n pub fn bar() {\n-    let x = 1;\n+    let x = 2;\n }\n*** End Patch";
        let (path, hunks) = parse_v4a_patch(patch).unwrap();
        assert_eq!(path, "src/foo.rs");
        assert!(!hunks.is_empty());
        let kinds: Vec<_> = hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.kind.clone()))
            .collect();
        assert!(kinds.contains(&DiffLineKind::Add));
        assert!(kinds.contains(&DiffLineKind::Remove));
    }
}

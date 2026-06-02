//! Application state for the Recursive TUI.
//!
//! [`App`] owns everything visible to the user: the transcript blocks,
//! the input buffer, the current screen, scroll position, and bookkeeping
//! for streaming, usage, and per-turn timing.
//!
//! Rendering lives in [`crate::ui`]; this file is *only* state plus the
//! reducers that mutate it in response to [`UiEvent`]s and key events.

use std::collections::{HashMap, HashSet};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::runtime::GoalState;
use crate::tui::events::{UiEvent, UserAction};

// ──────────────────────────────────────────────────────────────────────
// Screens
// ──────────────────────────────────────────────────────────────────────

/// Which top-level screen is currently rendered.
///
/// Goal 147 removed the `PlanReview` variant — the plan-mode
/// confirmation now lives on the modal stack as
/// [`crate::tui::ui::modal::Modal::PlanReview`], so we are down to two
/// screens: the brief splash and the chat surface.
#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Splash,
    Chat,
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
/// renderer in [`crate::tui::ui::transcript`].
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
    /// Goal-E: plan-mode proposal rendered inline in the transcript.
    /// Replaces the `Modal::PlanReview` pop-up so the plan is visible
    /// in the message stream without obscuring prior context.
    PlanProposal {
        plan_text: String,
        tool_calls: Vec<serde_json::Value>,
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
// Double-press tracker (Goal 147)
// ──────────────────────────────────────────────────────────────────────

/// Default window for double-press detection (Esc / Ctrl+C). The
/// runtime can override this via the `RECURSIVE_TUI_DOUBLE_MS` env
/// var — see [`double_press_window`].
pub const DOUBLE_PRESS_WINDOW: std::time::Duration = std::time::Duration::from_millis(2000);

/// Resolve the active double-press window. Reads
/// `RECURSIVE_TUI_DOUBLE_MS` once per call (cheap; this is hit only on
/// keypress) and falls back to [`DOUBLE_PRESS_WINDOW`] on parse
/// failure.
pub fn double_press_window() -> std::time::Duration {
    std::env::var("RECURSIVE_TUI_DOUBLE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(DOUBLE_PRESS_WINDOW)
}

/// Tracks when the user last pressed Esc / Ctrl+C. Goal 147 maps a
/// "double press within window" to a stronger action (real exit on
/// the second Ctrl+C) while a single press triggers the
/// context-dependent path (interrupt / clear buffer / pop modal).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoublePressTracker {
    pub last_esc_at: Option<Instant>,
    pub last_ctrl_c_at: Option<Instant>,
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

/// Return the model name to display in the status bar.
///
/// Honours the same priority chain `Backend::build_runtime` does:
/// `RECURSIVE_MODEL` / `OPENAI_MODEL` env vars, then
/// `~/.recursive/config.toml`'s `[provider].model`, then the
/// hardcoded `gpt-4o-mini` default. Without this fallback the
/// status bar would show "gpt-4o-mini" even when the runtime is
/// actually talking to DeepSeek/etc.
pub fn detect_model_name() -> String {
    if let Ok(m) = std::env::var("RECURSIVE_MODEL") {
        return m;
    }
    if let Ok(m) = std::env::var("OPENAI_MODEL") {
        return m;
    }
    if let Ok(Some(cfg)) = crate::config_file::FileConfig::load() {
        if let Some(m) = cfg.provider.and_then(|p| p.model) {
            if !m.is_empty() {
                return m;
            }
        }
    }
    "gpt-4o-mini".to_string()
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
// PromptInput (Goal 145)
// ──────────────────────────────────────────────────────────────────────

/// Which input mode the PromptInput is currently in.
///
/// Goal-145: the input box is mode-aware, with auto-detection from the
/// first character (`!`/`#`/`/`) and explicit cycling via Shift+Tab.
///
/// Goal-158: added `AtFile` mode — triggered by typing `@` in Prompt
/// mode, showing a file-completion popup. The query and suggestion
/// list are stored separately in [`App`] (`atfile_query`,
/// `atfile_suggestions`, `atfile_selected`) so this enum stays `Copy`.
///
/// Goal-160: added `HistorySearch` mode — triggered by `Ctrl+R` in
/// Prompt mode, showing a fuzzy-search popup over submission history.
/// Search state lives in `App` (`hsearch_query`, `hsearch_matches`,
/// `hsearch_selected`) so this enum stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    /// Default mode — submit goes to the LLM as a user message.
    Prompt,
    /// `!`-prefixed; submit dispatches `run_shell` directly,
    /// bypassing the LLM and the runtime transcript.
    Bash,
    /// `#`-prefixed; submit appends a `System` block locally only,
    /// nothing is sent to the backend.
    Note,
    /// `/`-prefixed; submit will eventually invoke a slash-command
    /// (Goal 146). For Step 3 we render a placeholder System block.
    Command,
    /// `@`-triggered (Goal 158): shows a file-path completion popup.
    /// The completion query and candidates live in the parent [`App`].
    AtFile,
    /// `Ctrl+R`-triggered (Goal 160): shows a fuzzy history-search
    /// popup. Search state lives in the parent [`App`].
    HistorySearch,
}

impl InputMode {
    /// Indicator character for the left of the input box.
    pub fn indicator(self) -> char {
        match self {
            InputMode::Prompt | InputMode::AtFile | InputMode::HistorySearch => '❯',
            InputMode::Bash => '!',
            InputMode::Note => '#',
            InputMode::Command => '/',
        }
    }

    /// Mode prefix used when storing entries in the history ring so
    /// that recalling them later restores the originating mode.
    pub fn history_prefix(self) -> &'static str {
        match self {
            InputMode::Prompt | InputMode::AtFile | InputMode::HistorySearch => "",
            InputMode::Bash => "!",
            InputMode::Note => "#",
            InputMode::Command => "/",
        }
    }

    /// Cycle Prompt → Bash → Note → Prompt. Skips `Command`, `AtFile`,
    /// and `HistorySearch` because those can only be reached by typing
    /// their trigger — matches fake-cc's behaviour.
    pub fn cycle_next(self) -> InputMode {
        match self {
            InputMode::Prompt => InputMode::Bash,
            InputMode::Bash => InputMode::Note,
            InputMode::Note => InputMode::Prompt,
            InputMode::Command | InputMode::AtFile | InputMode::HistorySearch => InputMode::Prompt,
        }
    }
}

/// Maximum number of history entries to retain in the ringbuffer.
pub const HISTORY_CAPACITY: usize = 200;

/// Mutable state of the multi-mode prompt input.
///
/// Owns the editing buffer, byte-cursor, in-session history, and a
/// stash slot for the user's draft when they walk back through
/// history. Rendering is in [`crate::tui::ui::input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptInputState {
    pub mode: InputMode,
    pub buffer: String,
    /// Byte offset into [`buffer`]. Always at a char boundary.
    pub cursor: usize,
    /// Submitted entries, oldest first. Capped at
    /// [`HISTORY_CAPACITY`].
    pub history: Vec<String>,
    /// Position when navigating history. `None` means "live draft".
    pub history_idx: Option<usize>,
    /// Stash slot: when the user starts walking history, the current
    /// buffer is preserved here and restored when they walk past the
    /// end.
    pub draft: String,
    /// Stash for the current mode while walking history. Restored
    /// alongside `draft`.
    pub draft_mode: InputMode,
}

impl Default for PromptInputState {
    fn default() -> Self {
        Self {
            mode: InputMode::Prompt,
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            draft_mode: InputMode::Prompt,
        }
    }
}

impl PromptInputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a single character at the cursor. Updates `cursor` to
    /// stay just past the inserted char.
    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_idx = None;
    }

    /// Delete the character to the left of the cursor (Backspace).
    /// Returns `true` if a char was deleted.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buffer.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_idx = None;
        true
    }

    /// Delete the character at the cursor (Delete key).
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let after = self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buffer.len());
        self.buffer.drain(self.cursor..after);
        self.history_idx = None;
    }

    /// Move cursor one char left.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Move cursor one char right.
    pub fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let step = self.buffer[self.cursor..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.cursor = (self.cursor + step).min(self.buffer.len());
    }

    /// Move to start of the current visual line (delimited by `\n`).
    pub fn move_home(&mut self) {
        self.cursor = self.buffer[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }

    /// Move to end of the current visual line.
    pub fn move_end(&mut self) {
        self.cursor = self.buffer[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.buffer.len());
    }

    /// True when the cursor sits on the **first** visual line.
    pub fn cursor_on_first_line(&self) -> bool {
        !self.buffer[..self.cursor].contains('\n')
    }

    /// True when the cursor sits on the **last** visual line.
    pub fn cursor_on_last_line(&self) -> bool {
        !self.buffer[self.cursor..].contains('\n')
    }

    /// Begin a history walk: stash the current buffer + mode and
    /// load the last entry. No-op when history is empty.
    fn enter_history_walk(&mut self) {
        if self.history_idx.is_none() {
            self.draft = self.buffer.clone();
            self.draft_mode = self.mode;
            self.history_idx = Some(self.history.len());
        }
    }

    /// Walk history one step back (older). Returns `true` if state
    /// changed.
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        self.enter_history_walk();
        let idx = self.history_idx.unwrap_or(self.history.len());
        if idx == 0 {
            return false;
        }
        let new_idx = idx - 1;
        self.load_history(new_idx);
        true
    }

    /// Walk history one step forward (newer). Restores the draft
    /// when stepping past the most-recent entry. Returns `true` if
    /// state changed.
    pub fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        let next = idx + 1;
        if next >= self.history.len() {
            // Past the newest entry: restore live draft.
            self.buffer = std::mem::take(&mut self.draft);
            self.cursor = self.buffer.len();
            self.mode = self.draft_mode;
            self.history_idx = None;
        } else {
            self.load_history(next);
        }
        true
    }

    fn load_history(&mut self, idx: usize) {
        let raw = &self.history[idx];
        let (mode, body) = strip_history_prefix(raw);
        self.mode = mode;
        self.buffer = body.to_string();
        self.cursor = self.buffer.len();
        self.history_idx = Some(idx);
    }

    /// Push the just-submitted entry onto the history ring (with
    /// mode prefix) and reset transient state.
    pub fn record_submission(&mut self, prefixed: String) {
        if !prefixed.is_empty() {
            self.history.push(prefixed);
            if self.history.len() > HISTORY_CAPACITY {
                let overflow = self.history.len() - HISTORY_CAPACITY;
                self.history.drain(0..overflow);
            }
        }
        self.buffer.clear();
        self.cursor = 0;
        self.mode = InputMode::Prompt;
        self.history_idx = None;
        self.draft.clear();
        self.draft_mode = InputMode::Prompt;
    }
}

fn strip_history_prefix(raw: &str) -> (InputMode, &str) {
    if let Some(rest) = raw.strip_prefix('!') {
        (InputMode::Bash, rest)
    } else if let Some(rest) = raw.strip_prefix('#') {
        (InputMode::Note, rest)
    } else if let Some(rest) = raw.strip_prefix('/') {
        (InputMode::Command, rest)
    } else {
        (InputMode::Prompt, raw)
    }
}

/// Static fallback list of tools shown by `/tools` when the TUI is
/// running in offline mode (no runtime to query). Mirrors the set
/// `backend::build_default_tools` registers.
pub fn default_offline_tool_catalog() -> Vec<(String, String)> {
    vec![
        ("read_file".into(), "Read a file from the workspace".into()),
        ("write_file".into(), "Write a file to the workspace".into()),
        ("apply_patch".into(), "Apply a V4A patch to a file".into()),
        (
            "list_dir".into(),
            "List a directory under the workspace".into(),
        ),
        (
            "run_shell".into(),
            "Run a shell command in the workspace".into(),
        ),
        (
            "search_files".into(),
            "Search files for a regex pattern".into(),
        ),
    ]
}

// ──────────────────────────────────────────────────────────────────────
// Top-level App
// ──────────────────────────────────────────────────────────────────────

pub struct App {
    /// Multi-mode input state (Goal 145).
    pub prompt: PromptInputState,
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
    /// Goal-146: stack of overlay modals. The topmost (last) modal
    /// receives keys; an empty stack means chat keys are active.
    pub modals: Vec<crate::tui::ui::modal::Modal>,
    /// Goal-146: registry of `/`-prefixed slash commands. Lazily
    /// initialised in [`App::new`] with [`CommandRegistry::default_set`].
    pub commands: crate::tui::commands::CommandRegistry,
    /// Goal-146: list of tools the runtime has registered. Populated
    /// by `main.rs` from `Backend::tool_specs()` after the worker
    /// boots, and read by the `/tools` command. Defaults to a static
    /// list when running offline.
    pub tool_catalog: Vec<(String, String)>,
    /// Goal-146: cursor / selected index into the command-menu
    /// completion popup. `None` means the user hasn't navigated
    /// (Enter executes the literal buffer).
    pub command_menu_selected: Option<usize>,
    /// Goal-146: planning-mode flag mirrored on the UI side. Reflects
    /// the latest `/plan on|off` invocation. Used to render an
    /// indicator and to seed `/status`.
    pub planning_mode_on: bool,
    /// Set when the agent has proposed a plan via `exit_plan_mode` and we are
    /// waiting for the user to approve or reject it. Cleared by
    /// `PlanConfirmed` / `PlanRejected` events. Used to show a status-bar
    /// indicator so the user knows input is expected.
    pub plan_awaiting_approval: bool,
    /// Goal-147: tracks the most recent Esc / Ctrl+C presses so the
    /// second press within [`double_press_window`] can promote a soft
    /// action (interrupt / clear) into a real exit. See
    /// [`App::handle_key`].
    pub double_press: DoublePressTracker,
    // ── Goal-158: @file autocomplete ─────────────────────────────────
    /// The text the user has typed after `@` while in AtFile mode.
    pub atfile_query: String,
    /// Candidate file paths matching [`atfile_query`]. Refreshed on
    /// every keystroke in AtFile mode. Contains at most
    /// [`MAX_ATFILE_SUGGESTIONS`] entries.
    pub atfile_suggestions: Vec<String>,
    /// Currently highlighted row in the AtFile popup. `None` means
    /// nothing is highlighted yet (typing narrows the list).
    pub atfile_selected: Option<usize>,
    // ── Goal-160: Ctrl+R history search ──────────────────────────────
    /// Current search query in HistorySearch mode.
    pub hsearch_query: String,
    /// Indices into `prompt.history` that match [`hsearch_query`],
    /// in priority order (prefix matches first). Capped at
    /// [`MAX_HSEARCH_RESULTS`].
    pub hsearch_matches: Vec<usize>,
    /// Currently highlighted row in the history-search popup.
    pub hsearch_selected: usize,
    // ── Goal-161: Permission Request Modal ───────────────────────────
    /// A pending tool-permission request delivered from the backend
    /// worker via the side-channel. `None` means no permission dialog
    /// is open. When `Some`, the modal is rendered and all keys are
    /// routed to `handle_permission_key`.
    pub pending_permission: Option<PendingPermission>,
    /// Set of tool names the user has chosen to "Allow All" for the
    /// current session. Requests for these tools skip the modal.
    pub auto_allowed_tools: HashSet<String>,
    /// Whether the runtime permission hook is currently active.
    /// Toggled by `/permissions on|off`. Shared with the backend worker.
    pub permission_hook_enabled: Arc<AtomicBool>,
    /// Goal-167: current task list maintained by `todo_write` calls.
    /// Empty when no task list has been set this session.
    pub current_todos: Vec<crate::tools::todo::TodoItem>,
    /// Goal-168: mirrored goal state, updated by `UiEvent::Goal*` events.
    pub active_goal: Option<GoalState>,
    /// Goal-171: workspace root path, used by /resume to list sessions.
    pub workspace_path: std::path::PathBuf,
    /// Goal-174: active colour palette. Defaults to [`DARK`]; switchable
    /// via `/theme <name>` without restart.
    pub theme: &'static crate::tui::ui::theme::Theme,
}

// ── Goal-161: PendingPermission ──────────────────────────────────────────────

/// Holds the state for the permission-request modal while it is open.
/// The `reply` sender is consumed exactly once when the user presses Y or N.
pub struct PendingPermission {
    pub tool_name: String,
    pub args_preview: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Maximum candidates shown in the @file popup.
pub const MAX_ATFILE_SUGGESTIONS: usize = 12;

/// Maximum results shown in the Ctrl+R history-search popup.
pub const MAX_HSEARCH_RESULTS: usize = 12;

/// Fuzzy-search `history` for `query` (case-insensitive substring match).
///
/// Returns indices into `history` ordered by relevance: prefix matches come
/// before substring matches. When `query` is empty, returns all indices in
/// reverse insertion order (most-recent first). Results are capped at
/// [`MAX_HSEARCH_RESULTS`].
pub fn search_history(history: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        // All entries, most recent first.
        let mut all: Vec<usize> = (0..history.len()).rev().collect();
        all.truncate(MAX_HSEARCH_RESULTS);
        return all;
    }
    let mut prefix: Vec<usize> = Vec::new();
    let mut substr: Vec<usize> = Vec::new();
    for (i, entry) in history.iter().enumerate() {
        let lower = entry.to_lowercase();
        if lower.starts_with(&q) {
            prefix.push(i);
        } else if lower.contains(&q) {
            substr.push(i);
        }
    }
    // Most recent prefix matches first, then most recent substr matches.
    prefix.reverse();
    substr.reverse();
    let mut out = prefix;
    out.extend(substr);
    out.truncate(MAX_HSEARCH_RESULTS);
    out
}

/// Enumerate workspace files matching `query` (case-insensitive prefix /
/// substring match). Returns relative paths, newest-first within each
/// depth tier, capped at [`MAX_ATFILE_SUGGESTIONS`].
///
/// Excludes: `target/`, `.git/`, `node_modules/`. Walks at most 3
/// directory levels deep so the function stays fast even in large trees.
pub fn glob_workspace_files(query: &str) -> Vec<String> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let q = query.to_lowercase();
    let mut results: Vec<String> = Vec::new();
    collect_files(&cwd, &cwd, 0, &q, &mut results);
    results.sort();
    results.dedup();
    // Prefer entries whose filename starts with the query.
    results.sort_by_key(|p| {
        let name = std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        if name.starts_with(&q) {
            0u8
        } else {
            1u8
        }
    });
    results.truncate(MAX_ATFILE_SUGGESTIONS);
    results
}

fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    query: &str,
    out: &mut Vec<String>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs and common large dirs.
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, depth + 1, query, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| name_str.to_string());
            if (query.is_empty() || rel.to_lowercase().contains(query))
                && out.len() < MAX_ATFILE_SUGGESTIONS * 4
            {
                out.push(rel);
            }
        }
    }
}

impl App {
    pub fn new() -> Self {
        // Goal-169: load workspace skill commands at startup.
        let workspace = crate::config::Config::from_env()
            .map(|c| c.workspace)
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let skills = crate::tui::skill_commands::SkillCommandLoader::load(&workspace);
        let commands =
            crate::tui::commands::CommandRegistry::default_set().with_skill_commands(skills);
        Self {
            prompt: PromptInputState::new(),
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
            modals: Vec::new(),
            commands,
            tool_catalog: default_offline_tool_catalog(),
            command_menu_selected: None,
            planning_mode_on: false,
            plan_awaiting_approval: false,
            double_press: DoublePressTracker::default(),
            atfile_query: String::new(),
            atfile_suggestions: Vec::new(),
            atfile_selected: None,
            hsearch_query: String::new(),
            hsearch_matches: Vec::new(),
            hsearch_selected: 0,
            pending_permission: None,
            auto_allowed_tools: HashSet::new(),
            permission_hook_enabled: Arc::new(AtomicBool::new(false)),
            current_todos: Vec::new(),
            active_goal: None,
            workspace_path: workspace,
            theme: &crate::tui::ui::theme::DARK,
        }
    }

    /// Backwards-compat shim for legacy code paths that still expect
    /// a single `input` string. Reads the prompt buffer.
    pub fn input(&self) -> &str {
        &self.prompt.buffer
    }

    /// Replace the prompt buffer (used by PlanReview's `e`-edit path
    /// and a handful of unit tests). Resets cursor to end and mode to
    /// Prompt.
    pub fn set_input<S: Into<String>>(&mut self, value: S) {
        self.prompt.buffer = value.into();
        self.prompt.cursor = self.prompt.buffer.len();
        self.prompt.mode = InputMode::Prompt;
        self.prompt.history_idx = None;
    }

    /// Process one key event. Returns an optional [`UserAction`] that
    /// the caller must forward to the backend worker.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        // ── Ctrl+C: highest priority, double-press promotes to exit
        // (Goal 147 §5). Modals + buffer + turn state all decide what
        // the *first* press does; the second press inside the window
        // always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.handle_ctrl_c();
        }

        // ── Goal-161: permission modal ───────────────────────────────
        // When a tool-permission request is pending, all keys go to the
        // permission dialog. Y/Enter allow, N/Esc deny.
        if self.pending_permission.is_some() {
            return self.handle_permission_key(key);
        }

        // ── Modal stack ──────────────────────────────────────────────
        // Goal-146: when any modal is on the stack, it owns the key
        // events. Modals may produce UserActions (Goal-147 added the
        // PlanReview y/n/Esc paths that send ConfirmPlan / RejectPlan
        // to the backend).
        if !self.modals.is_empty() {
            return self.handle_modal_key_action(key);
        }

        // ── Ctrl+E: contextual ───────────────────────────────────────
        // When the input buffer is non-empty, Ctrl+E behaves as
        // "move to end-of-line" inside the input. When the buffer
        // is empty, Ctrl+E falls back to Goal-144's "expand the
        // most recent ToolResult" behaviour. This is the conflict
        // resolution the goal calls for in §10.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            if self.prompt.buffer.is_empty() {
                self.toggle_last_expandable();
            } else {
                self.prompt.move_end();
            }
            return None;
        }

        // ── Ctrl+A: line-start in the input box ──────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.prompt.move_home();
            return None;
        }

        // ── Ctrl+B / Ctrl+F: transcript paged scroll ─────────────────
        // Terminal-independent fallbacks for PgUp/PgDn. Some macOS
        // terminals don't deliver a true PageUp keycode (they map
        // fn+↑ to ESC sequences crossterm doesn't surface as PageUp)
        // and don't preserve the SHIFT modifier on Shift+↑. Plain
        // Ctrl+letter codes are the most portable scroll keys we
        // can offer.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.scroll_offset = self.scroll_offset.saturating_add(10);
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.scroll_offset = self.scroll_offset.saturating_sub(10);
            return None;
        }

        // ── Ctrl+R: history search (Goal 160) ────────────────────────
        // In Prompt mode, Ctrl+R enters HistorySearch. In
        // HistorySearch mode, a second Ctrl+R moves down one match
        // (bash-compatible). In other modes, it is a no-op.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            match self.prompt.mode {
                InputMode::Prompt => {
                    self.enter_history_search_mode();
                    return None;
                }
                InputMode::HistorySearch => {
                    // Cycle to next match.
                    if !self.hsearch_matches.is_empty() {
                        self.hsearch_selected =
                            (self.hsearch_selected + 1) % self.hsearch_matches.len();
                    }
                    return None;
                }
                _ => return None,
            }
        }

        // ── Shift+Tab: cycle modes ───────────────────────────────────
        if key.code == KeyCode::BackTab {
            self.prompt.mode = self.prompt.mode.cycle_next();
            return None;
        }

        // ── History-search navigation (Goal 160) ─────────────────────
        if self.prompt.mode == InputMode::HistorySearch {
            return self.handle_history_search_key(key);
        }

        // ── @file autocomplete navigation (Goal 158) ─────────────────
        // When in AtFile mode, route navigation keys to the file
        // completion popup before anything else.
        if self.prompt.mode == InputMode::AtFile {
            return self.handle_atfile_key(key);
        }

        // ── Command-menu navigation (Goal 146) ───────────────────────
        // Intercept Up/Down/Tab/Enter when the user is composing a
        // slash command so the popup behaves like an autocomplete
        // menu rather than scrolling the transcript / submitting.
        if self.prompt.mode == InputMode::Command {
            if let Some(action) = self.handle_command_menu_key(key) {
                return action;
            }
        }

        // ── Chat screen ──────────────────────────────────────────────
        match key.code {
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.prompt.insert_char('\n');
                None
            }
            KeyCode::Enter => self.submit_prompt(),
            // Transcript scrolling — checked **before** history walk
            // because Shift+↑/↓ should win even when the buffer is
            // empty and history exists (otherwise the
            // `should_walk_history_*` guard would silently consume
            // the keypress for history navigation). Goal 150 follow-
            // up: user reported scroll keys still drove the input
            // box, root cause was this ordering.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                None
            }
            KeyCode::Up if self.should_walk_history_up() => {
                self.prompt.history_prev();
                None
            }
            KeyCode::Down if self.should_walk_history_down() => {
                self.prompt.history_next();
                None
            }
            KeyCode::Char('q') if self.prompt.buffer.is_empty() => {
                self.should_quit = true;
                None
            }
            KeyCode::Char(c) => {
                self.handle_char_input(c);
                None
            }
            KeyCode::Backspace => {
                if self.prompt.buffer.is_empty() && self.prompt.mode != InputMode::Prompt {
                    // Empty buffer in a non-Prompt mode: drop back to
                    // Prompt rather than no-op. This is how the user
                    // exits a mode they entered by accident.
                    self.prompt.mode = InputMode::Prompt;
                } else {
                    self.prompt.backspace();
                }
                None
            }
            KeyCode::Delete => {
                self.prompt.delete_forward();
                None
            }
            KeyCode::Left => {
                self.prompt.move_left();
                None
            }
            KeyCode::Right => {
                self.prompt.move_right();
                None
            }
            KeyCode::Home => {
                self.prompt.move_home();
                None
            }
            KeyCode::End => {
                self.prompt.move_end();
                None
            }
            KeyCode::Esc => self.handle_esc(),
            _ => None,
        }
    }

    /// Goal-147: dispatch the Esc key when no modal is active.
    ///
    /// Order of resolution:
    ///   1. Buffer non-empty → clear it and reset to Prompt mode.
    ///   2. A turn is running → emit `UserAction::Interrupt`, push a
    ///      System block, and start the double-press window.
    ///   3. Otherwise → no-op. **Esc never quits** from the chat
    ///      screen (Goal 147). Quitting is owned by `Ctrl+C×2`,
    ///      `Ctrl+D`, `/exit`, or `q` inside a modal.
    ///
    /// The double-press window is tracked but unused for Esc — Esc
    /// has no escalation path; we update the timestamp anyway so
    /// future enhancements can read it without re-plumbing.
    fn handle_esc(&mut self) -> Option<UserAction> {
        let now = Instant::now();
        let _within_window = self
            .double_press
            .last_esc_at
            .map(|t| now.duration_since(t) <= double_press_window())
            .unwrap_or(false);
        self.double_press.last_esc_at = Some(now);

        // Step 1: non-empty buffer or non-Prompt mode → clear.
        if !self.prompt.buffer.is_empty() || self.prompt.mode != InputMode::Prompt {
            self.prompt.buffer.clear();
            self.prompt.cursor = 0;
            self.prompt.mode = InputMode::Prompt;
            self.prompt.history_idx = None;
            return None;
        }

        // Step 2: in-flight turn → interrupt.
        if self.turn.running {
            self.push_system("Interrupting… (press Ctrl+C again to exit)");
            return Some(UserAction::Interrupt);
        }

        // Step 3: idle and empty — explicitly no-op (do **not** quit).
        None
    }

    /// Goal-147: dispatch Ctrl+C with double-press semantics.
    ///
    /// Order of resolution:
    ///   1. Two presses inside [`double_press_window`] → real exit.
    ///   2. Modal active → pop the topmost modal (single-press path).
    ///   3. Buffer non-empty → clear it.
    ///   4. Turn running → `UserAction::Interrupt` + System block.
    ///   5. Idle and empty → arm the "press again to exit" hint.
    fn handle_ctrl_c(&mut self) -> Option<UserAction> {
        let now = Instant::now();
        let within_window = self
            .double_press
            .last_ctrl_c_at
            .map(|t| now.duration_since(t) <= double_press_window())
            .unwrap_or(false);

        if within_window {
            // Second press inside the window → exit.
            self.should_quit = true;
            self.double_press.last_ctrl_c_at = None;
            return None;
        }

        self.double_press.last_ctrl_c_at = Some(now);

        // Step 2: pop a modal.
        if !self.modals.is_empty() {
            self.modals.pop();
            return None;
        }

        // Step 3: clear buffer.
        if !self.prompt.buffer.is_empty() || self.prompt.mode != InputMode::Prompt {
            self.prompt.buffer.clear();
            self.prompt.cursor = 0;
            self.prompt.mode = InputMode::Prompt;
            self.prompt.history_idx = None;
            return None;
        }

        // Step 4: interrupt the running turn.
        if self.turn.running {
            self.push_system("Interrupting… (press Ctrl+C again to exit)");
            return Some(UserAction::Interrupt);
        }

        // Step 5: idle, empty → arm the second press.
        self.push_system("Press Ctrl+C again to exit");
        None
    }

    /// History walk on Up should fire when (a) we are already
    /// walking (history_idx is Some) — so consecutive ↑ keep
    /// stepping back — or (b) the buffer is empty (entry point per
    /// goal §5).
    fn should_walk_history_up(&self) -> bool {
        if self.prompt.history.is_empty() {
            return false;
        }
        self.prompt.history_idx.is_some() || self.prompt.buffer.is_empty()
    }

    fn should_walk_history_down(&self) -> bool {
        self.prompt.history_idx.is_some()
    }

    fn handle_char_input(&mut self, c: char) {
        // Auto-detect mode from the first character when the buffer
        // is empty. The prefix character itself is consumed (used as
        // the mode marker, not stored).
        if self.prompt.buffer.is_empty() && self.prompt.mode == InputMode::Prompt {
            match c {
                '!' => {
                    self.prompt.mode = InputMode::Bash;
                    return;
                }
                '#' => {
                    self.prompt.mode = InputMode::Note;
                    return;
                }
                '/' => {
                    self.prompt.mode = InputMode::Command;
                    return;
                }
                _ => {}
            }
        }
        // Goal-158: `@` anywhere in Prompt mode triggers AtFile
        // completion. The `@` itself IS inserted into the buffer so
        // the user can see their typing; the query starts empty.
        if c == '@' && self.prompt.mode == InputMode::Prompt {
            self.prompt.insert_char('@');
            self.enter_atfile_mode();
            return;
        }
        self.prompt.insert_char(c);
    }

    /// Dispatch the current buffer based on the active mode. Returns
    /// the [`UserAction`] (if any) the caller must forward to the
    /// backend worker. Always resets the prompt to a clean state.
    fn submit_prompt(&mut self) -> Option<UserAction> {
        if self.prompt.buffer.is_empty() {
            // Don't submit empty prompts. Stay where we are — but if
            // the user is in a non-Prompt mode with nothing typed, do
            // nothing rather than spamming a no-op System block.
            return None;
        }
        let mode = self.prompt.mode;
        let body = self.prompt.buffer.clone();
        let prefixed = format!("{}{}", mode.history_prefix(), body);

        let action = match mode {
            InputMode::Prompt => {
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
            InputMode::Bash => {
                self.blocks.push(TranscriptBlock::User {
                    text: format!("!{body}"),
                });
                self.scroll_to_bottom();
                Some(UserAction::RunShell(body))
            }
            InputMode::Note => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("# {body}"),
                });
                self.scroll_to_bottom();
                None
            }
            InputMode::Command => self.dispatch_slash_command(&body),
            // AtFile mode is handled before submit_prompt is reached
            // (handle_atfile_key intercepts Enter). Treat as Prompt if
            // somehow reached here.
            InputMode::AtFile => {
                self.exit_atfile_mode();
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
            // HistorySearch intercepts Enter before submit_prompt.
            // Treat defensively as Prompt.
            InputMode::HistorySearch => {
                self.exit_history_search_mode();
                self.blocks
                    .push(TranscriptBlock::User { text: body.clone() });
                self.scroll_to_bottom();
                self.start_turn();
                Some(UserAction::SendMessage(body))
            }
        };

        self.prompt.record_submission(prefixed);
        self.command_menu_selected = None;
        action
    }

    /// Parse `body` (without the leading `/`) as `name + args`, look
    /// it up in [`App::commands`], and run the handler. Returns an
    /// optional [`UserAction`] for the dispatcher.
    fn dispatch_slash_command(&mut self, body: &str) -> Option<UserAction> {
        use crate::tui::commands::{CommandHandler, CommandOutcome};

        let mut parts = body.split_whitespace();
        let name = parts.next().unwrap_or("");
        let args: Vec<String> = parts.map(String::from).collect();

        // Clone the registry to avoid borrowing self while invoking
        // the handler (which takes &mut self).
        let registry = self.commands.clone();

        // Goal-169: check built-in commands first, then skill commands.
        if let Some(spec) = registry.lookup(name) {
            return match &spec.handler {
                CommandHandler::Sync(f) => {
                    match f(self, &args) {
                        CommandOutcome::Done => {}
                        CommandOutcome::Error(msg) => self.push_error(msg),
                        CommandOutcome::OpenModal(modal) => self.modals.push(modal),
                    }
                    None
                }
                CommandHandler::Async(f) => {
                    let actions = f(self, &args);
                    // The dispatcher only carries one UserAction back to
                    // the caller; queue the rest into App for later. In
                    // practice every async command returns 0 or 1 actions
                    // today.
                    actions.into_iter().next()
                }
            };
        }

        // Goal-169: skill command fallback.
        if let Some(skill) = registry.lookup_skill(name) {
            let args_str = args.join(" ");
            let prompt = skill.expand(&args_str);
            self.push_system(format!(
                "Running skill /{}: {}",
                skill.name, skill.description
            ));
            self.blocks.push(crate::tui::app::TranscriptBlock::User {
                text: prompt.clone(),
            });
            self.scroll_to_bottom();
            self.start_turn();
            return Some(UserAction::RunSkillPrompt { prompt });
        }

        self.push_error(format!("Unknown command: /{name}. Try /help."));
        None
    }

    /// Apply an event coming from the backend worker.
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantPartial { text } => {
                self.append_streaming_assistant(&text);
            }
            UiEvent::AssistantMessage { content } => {
                // Goal-147: the legacy `"plan:"` / `"## plan"` text
                // sniff is gone — plan-mode now arrives through the
                // structured `UiEvent::PlanProposed` channel. Any
                // assistant text that looks like a plan prefix is now
                // just displayed as-is.
                self.finalise_streaming_assistant(content);
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
            UiEvent::PlanProposed {
                plan_text,
                tool_calls,
            } => {
                // Fix-E: show the plan inline in the transcript rather
                // than as a floating modal.  The dedicated
                // `TranscriptBlock::PlanProposal` variant is rendered
                // as a bordered box inside the message stream, and the
                // status bar already shows "plan: y/n" to signal that
                // the input layer is awaiting a decision.
                self.blocks.push(TranscriptBlock::PlanProposal {
                    plan_text,
                    tool_calls,
                });
                self.plan_awaiting_approval = true;
            }
            UiEvent::PlanConfirmed => {
                self.close_plan_review_modal();
                self.blocks.push(TranscriptBlock::System {
                    text: "Plan approved".into(),
                });
                self.plan_awaiting_approval = false;
            }
            UiEvent::PlanRejected { reason } => {
                self.close_plan_review_modal();
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Plan rejected: {reason}"),
                });
                self.plan_awaiting_approval = false;
            }
            // Goal-167: replace the task list whenever the agent calls todo_write.
            UiEvent::TodoUpdated { todos } => {
                self.current_todos = todos;
            }

            // ── Goal-168: goal-loop events ──────────────────────────────────
            UiEvent::GoalContinuing { reason, turns } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Goal continuing (turn {turns}): {reason}"),
                });
                // Update mirrored state.
                if let Some(ref mut gs) = self.active_goal {
                    gs.turns = turns;
                    gs.last_reason = Some(reason);
                }
            }
            UiEvent::GoalAchieved { condition, turns } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Goal achieved after {turns} turns: \"{condition}\""),
                });
                self.active_goal = None;
            }
            UiEvent::GoalCleared => {
                self.blocks.push(TranscriptBlock::System {
                    text: "Goal cleared.".into(),
                });
                self.active_goal = None;
            }
            // ── Goal-170: turn abort ──────────────────────────────────────────
            UiEvent::Interrupted => {
                self.blocks.push(TranscriptBlock::System {
                    text: "[Interrupted]".into(),
                });
                self.turn.finish();
                self.pending_latency_ms = None;
                self.plan_awaiting_approval = false;
            }
            UiEvent::McpServersLoaded { entries } => {
                self.modals.push(crate::tui::ui::modal::Modal::McpServers {
                    entries,
                    selected: 0,
                });
            }
            UiEvent::SessionResumed {
                session_id,
                turn_count,
            } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("▶ Resumed session {session_id} ({turn_count} messages)"),
                });
                self.turn.finish();
                self.scroll_to_bottom();
            }
        }
        // Sticky-scroll: when the user is already at the bottom
        // (scroll_offset == 0), keep them pinned as new content
        // arrives. If they've explicitly scrolled up (Shift+↑ /
        // PgUp set scroll_offset > 0), preserve their position so
        // streaming tokens don't yank them back down mid-read.
        if self.scroll_offset == 0 {
            self.scroll_to_bottom();
        }
    }

    /// If the topmost modal is a `PlanReview`, pop it. No-op
    /// otherwise — the runtime may emit `PlanConfirmed` after the
    /// user already dismissed the modal manually, in which case we
    /// only want to push the System block.
    fn close_plan_review_modal(&mut self) {
        if matches!(
            self.modals.last(),
            Some(crate::tui::ui::modal::Modal::PlanReview { .. })
        ) {
            self.modals.pop();
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Push a System block onto the transcript and scroll to bottom.
    /// Public so [`crate::commands`] handlers can use it directly.
    pub fn push_system(&mut self, text: impl Into<String>) {
        self.blocks
            .push(TranscriptBlock::System { text: text.into() });
        self.scroll_to_bottom();
    }

    /// Push an Error block onto the transcript and scroll to bottom.
    pub fn push_error(&mut self, text: impl Into<String>) {
        self.blocks
            .push(TranscriptBlock::Error { text: text.into() });
        self.scroll_to_bottom();
    }

    /// Reset the transcript to a single fresh welcome block and zero
    /// out per-session usage. Called by `/clear`.
    pub fn reset_transcript(&mut self) {
        self.blocks.clear();
        self.blocks.push(TranscriptBlock::System {
            text: "Conversation cleared.".into(),
        });
        self.usage = UsageStats::default();
        self.turn_count = 0;
        self.pending_latency_ms = None;
        self.scroll_to_bottom();
    }

    /// Handle a key in command-completion-menu context. Returns
    /// `Some(action)` (with `action` itself optional) if the key was
    /// consumed; the outer `None` means "fall through to the regular
    /// chat key path".
    pub fn handle_command_menu_key(&mut self, key: KeyEvent) -> Option<Option<UserAction>> {
        use crate::tui::ui::command_menu;
        let matches_count = self.commands.search(&self.prompt.buffer).len();

        match key.code {
            KeyCode::Up => {
                match self.command_menu_selected {
                    None => return None,
                    Some(0) => self.command_menu_selected = None,
                    Some(n) => self.command_menu_selected = Some(n - 1),
                }
                Some(None)
            }
            KeyCode::Down => {
                if matches_count == 0 {
                    return None;
                }
                let next = match self.command_menu_selected {
                    None => 0,
                    Some(n) if n + 1 < matches_count.min(command_menu::MAX_VISIBLE) => n + 1,
                    Some(n) => n,
                };
                self.command_menu_selected = Some(next);
                Some(None)
            }
            KeyCode::Tab => {
                let registry = self.commands.clone();
                let matches = registry.search(&self.prompt.buffer);
                if let Some(target) =
                    command_menu::tab_completion_target(&self.prompt.buffer, &matches)
                {
                    self.prompt.buffer = target;
                    self.prompt.cursor = self.prompt.buffer.len();
                    self.command_menu_selected = None;
                }
                Some(None)
            }
            KeyCode::Enter => {
                // If a menu item is selected, execute it; otherwise
                // fall through to the regular submit path so the
                // user's literal buffer is dispatched.
                if let Some(idx) = self.command_menu_selected {
                    let registry = self.commands.clone();
                    let matches = registry.search(&self.prompt.buffer);
                    if let Some(spec) = matches.get(idx) {
                        let chosen = spec.name.to_string();
                        self.prompt.buffer = chosen;
                        self.prompt.cursor = self.prompt.buffer.len();
                    }
                    self.command_menu_selected = None;
                }
                None
            }
            _ => None,
        }
    }

    // ── Goal-158: @file completion helpers ───────────────────────────

    /// Switch to AtFile mode and populate the initial suggestion list.
    fn enter_atfile_mode(&mut self) {
        self.prompt.mode = InputMode::AtFile;
        self.atfile_query.clear();
        self.atfile_selected = None;
        self.atfile_suggestions = glob_workspace_files("");
    }

    /// Recompute [`App::atfile_suggestions`] from [`App::atfile_query`].
    fn refresh_atfile_suggestions(&mut self) {
        self.atfile_suggestions = glob_workspace_files(&self.atfile_query);
        // Clamp selection so it doesn't point past the new list.
        if let Some(sel) = self.atfile_selected {
            if sel >= self.atfile_suggestions.len() {
                self.atfile_selected = if self.atfile_suggestions.is_empty() {
                    None
                } else {
                    Some(self.atfile_suggestions.len() - 1)
                };
            }
        }
    }

    /// Insert the selected (or first) suggestion into the buffer,
    /// replacing the `@<query>` tail that was typed.
    fn commit_atfile_selection(&mut self) {
        let idx = self.atfile_selected.unwrap_or(0);
        let Some(chosen) = self.atfile_suggestions.get(idx).cloned() else {
            self.exit_atfile_mode();
            return;
        };
        // Replace the `@<query>` suffix in the buffer with `@<chosen>`.
        let at_pos = self
            .prompt
            .buffer
            .rfind('@')
            .unwrap_or(self.prompt.buffer.len());
        self.prompt.buffer.truncate(at_pos);
        self.prompt.buffer.push('@');
        self.prompt.buffer.push_str(&chosen);
        self.prompt.cursor = self.prompt.buffer.len();
        self.exit_atfile_mode();
    }

    /// Return to Prompt mode and clear completion state.
    fn exit_atfile_mode(&mut self) {
        self.prompt.mode = InputMode::Prompt;
        self.atfile_query.clear();
        self.atfile_suggestions.clear();
        self.atfile_selected = None;
    }

    /// Handle a key when [`InputMode::AtFile`] is active.
    pub fn handle_atfile_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Esc => {
                // Cancel: exit AtFile mode, keep `@<query>` in buffer.
                self.exit_atfile_mode();
                None
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.commit_atfile_selection();
                None
            }
            KeyCode::Up => {
                match self.atfile_selected {
                    None => {}
                    Some(0) => self.atfile_selected = None,
                    Some(n) => self.atfile_selected = Some(n - 1),
                }
                None
            }
            KeyCode::Down => {
                let count = self.atfile_suggestions.len();
                if count == 0 {
                    return None;
                }
                let next = match self.atfile_selected {
                    None => 0,
                    Some(n) if n + 1 < count => n + 1,
                    Some(n) => n,
                };
                self.atfile_selected = Some(next);
                None
            }
            KeyCode::Backspace => {
                if self.atfile_query.is_empty() {
                    // Delete the `@` from the buffer and exit AtFile mode.
                    self.exit_atfile_mode();
                    self.prompt.backspace(); // removes `@`
                } else {
                    // Delete last char from query and buffer.
                    let last_len = self
                        .atfile_query
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                    let new_len = self.atfile_query.len() - last_len;
                    self.atfile_query.truncate(new_len);
                    self.prompt.backspace();
                    self.refresh_atfile_suggestions();
                }
                None
            }
            KeyCode::Char(c) => {
                self.atfile_query.push(c);
                self.prompt.insert_char(c);
                self.refresh_atfile_suggestions();
                None
            }
            _ => None,
        }
    }

    // ── Goal-160: Ctrl+R history search ───────────────────────────────

    /// Enter HistorySearch mode, clearing the search query and
    /// pre-populating matches with all history entries (most recent first).
    fn enter_history_search_mode(&mut self) {
        self.prompt.mode = InputMode::HistorySearch;
        self.hsearch_query.clear();
        self.hsearch_selected = 0;
        self.hsearch_matches = search_history(&self.prompt.history, "");
    }

    /// Refresh [`App::hsearch_matches`] from [`App::hsearch_query`].
    fn refresh_hsearch_matches(&mut self) {
        self.hsearch_matches = search_history(&self.prompt.history, &self.hsearch_query);
        if self.hsearch_selected >= self.hsearch_matches.len().max(1) {
            self.hsearch_selected = 0;
        }
    }

    /// Fill the prompt buffer with the currently selected history entry
    /// and return to Prompt mode.
    fn commit_history_selection(&mut self) {
        if let Some(&hist_idx) = self.hsearch_matches.get(self.hsearch_selected) {
            if let Some(entry) = self.prompt.history.get(hist_idx) {
                self.prompt.buffer = entry.clone();
                self.prompt.cursor = self.prompt.buffer.len();
            }
        }
        self.exit_history_search_mode();
    }

    /// Return to Prompt mode and clear search state.
    fn exit_history_search_mode(&mut self) {
        self.prompt.mode = InputMode::Prompt;
        self.hsearch_query.clear();
        self.hsearch_matches.clear();
        self.hsearch_selected = 0;
    }

    /// Handle a key when [`InputMode::HistorySearch`] is active.
    pub fn handle_history_search_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            KeyCode::Esc => {
                self.exit_history_search_mode();
                None
            }
            KeyCode::Enter => {
                self.commit_history_selection();
                None
            }
            KeyCode::Up => {
                if !self.hsearch_matches.is_empty() && self.hsearch_selected > 0 {
                    self.hsearch_selected -= 1;
                }
                None
            }
            KeyCode::Down => {
                if !self.hsearch_matches.is_empty()
                    && self.hsearch_selected + 1 < self.hsearch_matches.len()
                {
                    self.hsearch_selected += 1;
                }
                None
            }
            KeyCode::Backspace => {
                if self.hsearch_query.is_empty() {
                    self.exit_history_search_mode();
                } else {
                    let last_len = self
                        .hsearch_query
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                    let new_len = self.hsearch_query.len() - last_len;
                    self.hsearch_query.truncate(new_len);
                    self.refresh_hsearch_matches();
                }
                None
            }
            KeyCode::Char(c) => {
                self.hsearch_query.push(c);
                self.refresh_hsearch_matches();
                None
            }
            _ => None,
        }
    }

    // ── Goal-161: permission modal ────────────────────────────────────

    /// Receive a pending permission request from the backend side-channel.
    /// Auto-allow if the tool is in the `auto_allowed_tools` set;
    /// otherwise store it so the UI can display the modal on the next render.
    pub fn set_pending_permission(&mut self, req: crate::tui::events::PermissionRequest) {
        if self.auto_allowed_tools.contains(&req.tool_name) {
            // Auto-allow: resolve immediately without showing the modal.
            let _ = req.reply.send(true);
            return;
        }
        // If a previous request is somehow still pending (shouldn't happen
        // in practice — the backend serialises tool calls), deny it so the
        // oneshot is consumed and the worker can unblock.
        if let Some(old) = self.pending_permission.take() {
            let _ = old.reply.send(false);
        }
        self.pending_permission = Some(PendingPermission {
            tool_name: req.tool_name,
            args_preview: req.args_preview,
            reply: req.reply,
        });
    }

    /// Handle a key while a permission modal is active.
    /// - `y` / `Y` / `Enter` → allow once
    /// - `n` / `N` / `Esc`   → deny
    /// - `a` / `A`           → allow + add tool to auto-allow list
    pub fn handle_permission_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        let (allow, auto_allow) = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => (true, false),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => (false, false),
            KeyCode::Char('a') | KeyCode::Char('A') => (true, true),
            _ => return None,
        };
        if let Some(p) = self.pending_permission.take() {
            if auto_allow {
                self.auto_allowed_tools.insert(p.tool_name.clone());
            }
            let _ = p.reply.send(allow);
        }
        None
    }

    /// Handle a key event when at least one modal is on the stack.
    /// Returns `Some(action)` if the modal layer wants to forward a
    /// [`UserAction`] to the backend (currently only the PlanReview
    /// modal does this). The outer key dispatcher should not also
    /// process this key against the chat layer.
    pub fn handle_modal_key_action(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::tui::ui::modal::Modal;

        // Goal-147: PlanReview modal owns y / n / e / Enter / Esc and
        // *bypasses* the generic confirm logic.
        if let Some(Modal::PlanReview { .. }) = self.modals.last() {
            return self.handle_plan_review_key(key);
        }

        // Goal-171: ResumePicker owns ↑/↓/Enter/Esc and may return a UserAction.
        if let Some(Modal::ResumePicker { .. }) = self.modals.last() {
            return self.handle_resume_picker_key(key);
        }

        // Goal-173: McpServers owns ↑/↓/Esc.
        if let Some(Modal::McpServers { .. }) = self.modals.last() {
            return self.handle_mcp_servers_key(key);
        }

        // Generic modal dispatch (Goal 146).
        self.handle_modal_key(key);
        None
    }

    /// Goal-147: dispatch a key against an active `Modal::PlanReview`.
    ///
    /// * `y` / `Enter` → emit `UserAction::ConfirmPlan`. The modal is
    ///   **not** popped here — we wait for the runtime's
    ///   `PlanConfirmed` event so the visible state matches the
    ///   server-side decision.
    /// * `n` / `Esc` → pop the modal immediately and emit
    ///   `UserAction::RejectPlan("user rejected")`. Goal §8 forbids
    ///   collecting a free-form reason here.
    /// * `e` → copy the plan text into the prompt buffer (Prompt
    ///   mode), close the modal, and let the user edit/resend
    ///   normally.
    /// * Any other key is consumed but ignored, keeping plan-mode
    ///   focus.
    fn handle_plan_review_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::tui::ui::modal::Modal;

        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                // Optimistic close: pop the modal immediately so the user
                // sees the dismissal without waiting for the PlanConfirmed
                // event to round-trip from the runtime.
                self.modals.pop();
                Some(UserAction::ConfirmPlan)
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.modals.pop();
                Some(UserAction::RejectPlan("user rejected".into()))
            }
            KeyCode::Char('e') => {
                if let Some(Modal::PlanReview { plan_text, .. }) = self.modals.last().cloned() {
                    self.set_input(plan_text);
                }
                self.modals.pop();
                None
            }
            _ => None,
        }
    }

    /// Goal-171: dispatch a key against an active `Modal::ResumePicker`.
    fn handle_resume_picker_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::tui::ui::modal::Modal;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modals.pop();
                None
            }
            KeyCode::Up => {
                if let Some(Modal::ResumePicker { selected, .. }) = self.modals.last_mut() {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                None
            }
            KeyCode::Down => {
                if let Some(Modal::ResumePicker { entries, selected }) = self.modals.last_mut() {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                }
                None
            }
            KeyCode::Enter => {
                if let Some(Modal::ResumePicker { entries, selected }) = self.modals.last() {
                    if let Some(entry) = entries.get(*selected) {
                        let session_dir = entry.session_dir.clone();
                        self.modals.pop();
                        return Some(UserAction::ResumeSession { session_dir });
                    }
                }
                self.modals.pop();
                None
            }
            _ => None,
        }
    }

    /// Goal-173: dispatch a key against an active `Modal::McpServers`.
    fn handle_mcp_servers_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        use crate::tui::ui::modal::Modal;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modals.pop();
                None
            }
            KeyCode::Up => {
                if let Some(Modal::McpServers { selected, .. }) = self.modals.last_mut() {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                None
            }
            KeyCode::Down => {
                if let Some(Modal::McpServers { entries, selected }) = self.modals.last_mut() {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Handle a key event when at least one modal is on the stack.
    /// Returns `true` if the key was consumed by the modal layer
    /// (so the caller should skip the chat key path).
    pub fn handle_modal_key(&mut self, key: KeyEvent) -> bool {
        use crate::tui::ui::modal::{ConfirmAction, Modal};
        let Some(top) = self.modals.last_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                self.modals.pop();
            }
            KeyCode::Char('q') => {
                self.modals.pop();
            }
            KeyCode::Char('y') => {
                if let Modal::Confirm { on_yes, .. } = top.clone() {
                    self.modals.pop();
                    match on_yes {
                        ConfirmAction::Exit => {
                            self.should_quit = true;
                        }
                        ConfirmAction::Clear => {
                            self.reset_transcript();
                        }
                    }
                }
            }
            KeyCode::Char('n') => {
                if matches!(top, Modal::Confirm { .. }) {
                    self.modals.pop();
                }
            }
            KeyCode::Enter => {
                if let Modal::Confirm { on_yes, .. } = top.clone() {
                    self.modals.pop();
                    match on_yes {
                        ConfirmAction::Exit => self.should_quit = true,
                        ConfirmAction::Clear => self.reset_transcript(),
                    }
                } else {
                    // Enter on non-confirm modals just dismisses.
                    self.modals.pop();
                }
            }
            KeyCode::Up => {
                if let Modal::Journal { selected, .. } = top {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if let Modal::Journal { entries, selected } = top {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                }
            }
            _ => {}
        }
        true
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

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    // ── construction ────────────────────────────────────────────────

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input().is_empty());
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

    /// Goal-150: status bar must read `~/.recursive/config.toml` when
    /// no env var is set, otherwise it lies about the model the
    /// runtime is actually using.
    ///
    /// Both checks share one test body so they share the `PinnedHome`
    /// lock (HOME mutation is process-global; cf. lesson 17).
    #[test]
    fn detect_model_name_falls_back_to_config_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let _pin = crate::test_util::PinnedHome::new(home.path());

        // Snapshot env so we can clear / restore.
        let prev_recursive_model = std::env::var("RECURSIVE_MODEL").ok();
        let prev_openai_model = std::env::var("OPENAI_MODEL").ok();
        std::env::remove_var("RECURSIVE_MODEL");
        std::env::remove_var("OPENAI_MODEL");

        // Part A: no env, no config.toml → hardcoded default
        assert_eq!(detect_model_name(), "gpt-4o-mini");

        // Part B: write a config.toml under HOME → that wins
        let cfg_dir = home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\nmodel = \"deepseek-v4-flash\"\n",
        )
        .expect("write");
        assert_eq!(detect_model_name(), "deepseek-v4-flash");

        // Part C: env var overrides config.toml
        std::env::set_var("RECURSIVE_MODEL", "from-env");
        assert_eq!(detect_model_name(), "from-env");

        // Restore env.
        std::env::remove_var("RECURSIVE_MODEL");
        if let Some(v) = prev_recursive_model {
            std::env::set_var("RECURSIVE_MODEL", v);
        }
        if let Some(v) = prev_openai_model {
            std::env::set_var("OPENAI_MODEL", v);
        }
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
        app.set_input("hello");
        let action = app.handle_key(key(KeyCode::Enter));
        assert!(app.input().is_empty());
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
        app.set_input("hi");
        let _ = app.handle_key(key(KeyCode::Enter));
        assert!(app.turn.running);
        assert_eq!(app.turn_count, 1);
    }

    #[test]
    fn turn_finished_stops_turn() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hi");
        let _ = app.handle_key(key(KeyCode::Enter));
        app.handle_ui_event(UiEvent::TurnFinished);
        assert!(!app.turn.running);
    }

    #[test]
    fn esc_clears_buffer_without_quitting() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("partial");
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.input().is_empty());
    }

    #[test]
    fn char_appends_to_input() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Char('h')));
        let _ = app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.input(), "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hello");
        let _ = app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input(), "hell");
    }

    /// Plain ↑ never scrolls — even with empty buffer it walks
    /// history once any has been recorded; with no history it's a
    /// no-op. Transcript scrolling is reserved for Shift+↑/↓ and
    /// PgUp/PgDn (Goal 150 fix: history was always shadowing
    /// scroll, leaving the transcript stuck at bottom).
    #[test]
    fn plain_up_does_not_scroll_transcript() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        for i in 0..30 {
            app.blocks.push(TranscriptBlock::System {
                text: format!("msg {i}"),
            });
        }
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.scroll_offset, 0);
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn shift_up_increases_scroll_offset() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        for i in 0..30 {
            app.blocks.push(TranscriptBlock::System {
                text: format!("msg {i}"),
            });
        }
        let _ = app.handle_key(shift(KeyCode::Up));
        assert_eq!(app.scroll_offset, 1);
        let _ = app.handle_key(shift(KeyCode::Up));
        assert_eq!(app.scroll_offset, 2);
    }

    #[test]
    fn shift_down_stops_at_zero() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 2;
        let _ = app.handle_key(shift(KeyCode::Down));
        let _ = app.handle_key(shift(KeyCode::Down));
        let _ = app.handle_key(shift(KeyCode::Down));
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

    /// PgUp/PgDn now work regardless of buffer state — they have no
    /// editing semantics, so claiming them for transcript scroll is
    /// safe. (Goal 150 relaxed the previous "buffer empty" guard.)
    #[test]
    fn page_up_scrolls_even_when_buffer_not_empty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("typing");
        let _ = app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 10);
    }

    /// Goal 150 follow-up: terminal-independent scroll fallbacks.
    /// macOS Terminal often strips SHIFT from arrow keys, so
    /// Shift+↑/↓ reaches us as plain Up/Down (which fires history
    /// walk). Ctrl+B / Ctrl+F bypass that — they're plain ASCII
    /// control codes every terminal forwards verbatim.
    #[test]
    fn ctrl_b_and_ctrl_f_scroll_transcript() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        // Buffer is empty → without our explicit handler, Ctrl+B
        // could be claimed by some other readline-style binding.
        let _ = app.handle_key(ctrl('b'));
        assert_eq!(app.scroll_offset, 10);
        let _ = app.handle_key(ctrl('b'));
        assert_eq!(app.scroll_offset, 20);
        let _ = app.handle_key(ctrl('f'));
        assert_eq!(app.scroll_offset, 10);
        let _ = app.handle_key(ctrl('f'));
        assert_eq!(app.scroll_offset, 0);
        // Stops at zero — no underflow.
        let _ = app.handle_key(ctrl('f'));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_b_scrolls_even_when_buffer_not_empty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("typing some text");
        let _ = app.handle_key(ctrl('b'));
        assert_eq!(app.scroll_offset, 10);
    }

    /// Sticky-scroll: when the user has explicitly scrolled up,
    /// new events should NOT yank them back to the bottom.
    /// (Without this guard, every streaming PartialToken or
    /// ToolCall would reset scroll_offset to 0 mid-read.)
    #[test]
    fn new_event_keeps_scroll_offset_when_user_scrolled_up() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 5; // user pressed Ctrl+B / PgUp etc.
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello".into(),
        });
        assert_eq!(app.scroll_offset, 5);
    }

    /// Sticky-scroll counterpart: when the user is at the bottom,
    /// new events DO scroll-to-bottom (i.e. confirm offset stays 0
    /// rather than getting bumped by content arriving above the
    /// visible window — chat.rs's effective_scroll handles this
    /// already, but we keep the explicit reset for clarity).
    #[test]
    fn new_event_at_bottom_keeps_user_at_bottom() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 0;
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

    // ── Plan Mode (Goal 147) ───────────────────────────────────────

    #[test]
    fn plan_proposed_event_opens_plan_review_modal() {
        // Fix-E: PlanProposed now renders inline as a TranscriptBlock
        // instead of opening a floating modal.
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "1. read_file\n2. apply_patch".into(),
            tool_calls: vec![serde_json::json!({
                "name": "read_file",
                "id": "1",
                "arguments": { "path": "src/foo.rs" }
            })],
        });
        // No modal should be opened — plan is inline in the transcript.
        assert!(app.modals.is_empty());
        // The plan proposal block should be in the transcript.
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::PlanProposal { plan_text, .. }
                if plan_text.contains("read_file"))));
        assert!(app.plan_awaiting_approval);
    }

    #[test]
    fn plan_confirmed_closes_modal_and_pushes_system_block() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        app.handle_ui_event(UiEvent::PlanConfirmed);
        assert!(app.modals.is_empty());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { text } if text == "Plan approved")));
    }

    #[test]
    fn plan_rejected_pushes_system_block_with_reason() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        app.handle_ui_event(UiEvent::PlanRejected {
            reason: "user rejected".into(),
        });
        assert!(app.modals.is_empty());
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text == "Plan rejected: user rejected")));
    }

    #[test]
    fn plan_review_y_dispatches_confirm_plan_action() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        // Fix-E: the modal is now optimistically closed on 'y' so the
        // user immediately sees the plan was accepted instead of waiting
        // for the PlanConfirmed round-trip event.
        assert!(app.modals.is_empty());
    }

    #[test]
    fn plan_review_n_dispatches_reject_plan_action() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('n')));
        match action {
            Some(UserAction::RejectPlan(reason)) => assert_eq!(reason, "user rejected"),
            other => panic!("expected RejectPlan, got {other:?}"),
        }
        assert!(app.modals.is_empty());
    }

    #[test]
    fn plan_review_e_copies_text_to_input_and_closes_modal() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "edit me please".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        let action = app.handle_key(key(KeyCode::Char('e')));
        assert!(action.is_none());
        assert_eq!(app.input(), "edit me please");
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(app.modals.is_empty());
    }

    /// Goal §5: Esc closes the topmost modal rather than quitting.
    #[test]
    fn esc_first_press_closes_modal_not_quits() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::Help);
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(app.modals.is_empty());
        assert!(!app.should_quit);
    }

    /// Goal §5: with no modal but a non-empty buffer, Esc clears the
    /// buffer and does not quit, even on a single press.
    #[test]
    fn esc_first_press_clears_input_when_modal_empty_and_buffer_set() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("partial");
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.input().is_empty());
    }

    /// Goal §5: Esc does **not** quit even on a second press inside
    /// the double-press window. (Quitting is owned exclusively by
    /// Ctrl+C×2 and the explicit `/exit` / `q-in-modal` paths.)
    #[test]
    fn esc_does_not_quit_after_double_press_when_idle() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(key(KeyCode::Esc));
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
    }

    /// Goal §5: Ctrl+C during a running turn dispatches an Interrupt
    /// action and writes a System block.
    #[test]
    fn ctrl_c_first_press_during_turn_dispatches_interrupt() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.turn.start();
        let action = app.handle_key(ctrl('c'));
        assert!(matches!(action, Some(UserAction::Interrupt)));
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Interrupting"))));
        assert!(!app.should_quit);
    }

    /// Goal §5: Ctrl+C while idle pushes a "press again to exit"
    /// hint, then a second press inside the window quits.
    #[test]
    fn ctrl_c_first_press_idle_pushes_warning_then_exits_on_second() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(ctrl('c'));
        assert!(!app.should_quit);
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Press Ctrl+C again"))));
        let _ = app.handle_key(ctrl('c'));
        assert!(app.should_quit);
    }

    /// Goal §5: Ctrl+C×2 inside the window quits regardless of the
    /// soft action the first press kicked off (interrupt / clear /
    /// modal-pop).
    #[test]
    fn ctrl_c_double_press_within_window_quits() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.turn.start();
        let _ = app.handle_key(ctrl('c'));
        // Second press almost-instantly: must quit.
        let _ = app.handle_key(ctrl('c'));
        assert!(app.should_quit);
    }

    /// Goal §5: a Ctrl+C press outside the double-press window
    /// resets the counter, so the *next* press starts a fresh round
    /// of soft actions instead of immediately quitting.
    #[test]
    fn ctrl_c_outside_window_resets_counter() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        // Backdate last_ctrl_c_at so the next press is "outside".
        app.double_press.last_ctrl_c_at = Some(Instant::now() - Duration::from_secs(60));
        let action = app.handle_key(ctrl('c'));
        // First press fresh round: idle + empty → arms the warning.
        assert!(action.is_none());
        assert!(!app.should_quit);
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text.contains("Press Ctrl+C again"))));
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

// ──────────────────────────────────────────────────────────────────────
// PromptInput tests (Goal 145)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod prompt_input_tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn fresh_app() -> App {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app
    }

    // ── prompt_input::shift_tab_cycles_modes ────────────────────────

    #[test]
    fn shift_tab_cycles_modes() {
        let mut app = fresh_app();
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Note);
        let _ = app.handle_key(k(KeyCode::BackTab));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::leading_<x>_enters_<mode>_when_buffer_empty ──

    #[test]
    fn leading_bang_enters_bash_mode_when_buffer_empty() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        // The `!` is consumed as the mode marker, not stored.
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_hash_enters_note_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('#')));
        assert_eq!(app.prompt.mode, InputMode::Note);
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_slash_enters_command_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('/')));
        assert_eq!(app.prompt.mode, InputMode::Command);
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn leading_bang_after_existing_text_is_just_a_char() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('h')));
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "h!");
    }

    // ── prompt_input::backspace_on_empty_exits_to_prompt_mode ───────

    #[test]
    fn backspace_on_empty_exits_to_prompt_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        let _ = app.handle_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::cursor_left_right_moves_within_buffer ─────────

    #[test]
    fn cursor_left_right_moves_within_buffer() {
        let mut app = fresh_app();
        for c in "abc".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        assert_eq!(app.prompt.cursor, 3);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 2);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 1);
        let _ = app.handle_key(k(KeyCode::Right));
        assert_eq!(app.prompt.cursor, 2);
    }

    #[test]
    fn cursor_handles_multibyte_chars() {
        let mut app = fresh_app();
        for c in "你好".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        // Each Chinese char is 3 bytes in UTF-8.
        assert_eq!(app.prompt.cursor, 6);
        let _ = app.handle_key(k(KeyCode::Left));
        assert_eq!(app.prompt.cursor, 3);
        let _ = app.handle_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.buffer, "好");
    }

    #[test]
    fn insert_at_cursor_not_just_end() {
        let mut app = fresh_app();
        for c in "ac".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let _ = app.handle_key(k(KeyCode::Left));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "abc");
    }

    // ── prompt_input::shift_enter_inserts_newline_at_cursor ─────────

    #[test]
    fn shift_enter_inserts_newline_at_cursor() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('a')));
        let _ = app.handle_key(shift(KeyCode::Enter));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "a\nb");
    }

    #[test]
    fn alt_enter_also_inserts_newline() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('a')));
        let _ = app.handle_key(alt(KeyCode::Enter));
        let _ = app.handle_key(k(KeyCode::Char('b')));
        assert_eq!(app.prompt.buffer, "a\nb");
    }

    // ── prompt_input::history_up_down_navigates_records ─────────────

    #[test]
    fn history_up_down_navigates_records() {
        let mut app = fresh_app();
        // Submit two messages.
        app.set_input("first");
        let _ = app.handle_key(k(KeyCode::Enter));
        app.set_input("second");
        let _ = app.handle_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.history.len(), 2);

        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "second");
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "first");
        let _ = app.handle_key(k(KeyCode::Down));
        assert_eq!(app.prompt.buffer, "second");
        let _ = app.handle_key(k(KeyCode::Down));
        // Past newest → restored draft (empty here).
        assert!(app.prompt.buffer.is_empty());
    }

    // ── prompt_input::history_up_saves_draft_and_restores_on_overflow ─

    #[test]
    fn history_up_saves_draft_and_restores_on_overflow() {
        let mut app = fresh_app();
        app.set_input("alpha");
        let _ = app.handle_key(k(KeyCode::Enter));
        // Type some draft, then walk history. Note: history walk
        // only triggers when buffer is empty (per goal §5).
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.buffer, "alpha");
        let _ = app.handle_key(k(KeyCode::Down));
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn history_preserves_mode_prefix() {
        let mut app = fresh_app();
        // Submit a bash command.
        let _ = app.handle_key(k(KeyCode::Char('!')));
        for c in "echo hi".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let _ = app.handle_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        // Walk back: should restore Bash mode.
        let _ = app.handle_key(k(KeyCode::Up));
        assert_eq!(app.prompt.mode, InputMode::Bash);
        assert_eq!(app.prompt.buffer, "echo hi");
    }

    #[test]
    fn history_capacity_truncates_oldest() {
        let mut app = fresh_app();
        for i in 0..(HISTORY_CAPACITY + 5) {
            app.set_input(format!("msg{i}"));
            let _ = app.handle_key(k(KeyCode::Enter));
        }
        assert_eq!(app.prompt.history.len(), HISTORY_CAPACITY);
        // The earliest entries should have been dropped.
        assert!(!app.prompt.history.iter().any(|h| h == "msg0"));
    }

    // ── prompt_input::submit_in_bash_mode_dispatches_run_shell ──────

    #[test]
    fn submit_in_bash_mode_dispatches_run_shell() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        for c in "ls".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        assert!(matches!(action, Some(UserAction::RunShell(s)) if s == "ls"));
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::submit_in_note_mode_appends_system_block ──────

    #[test]
    fn submit_in_note_mode_appends_system_block() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('#')));
        for c in "remember this".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        // No backend action: notes are local-only.
        assert!(action.is_none());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { text }
                if text.contains("remember this"))));
    }

    #[test]
    fn submit_in_command_mode_dispatches_to_registry() {
        // Goal-146 replaces the old placeholder System block with the
        // actual command dispatcher. /help opens the Help modal.
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('/')));
        for c in "help".chars() {
            let _ = app.handle_key(k(KeyCode::Char(c)));
        }
        let action = app.handle_key(k(KeyCode::Enter));
        assert!(action.is_none());
        // /help pushed a Help modal onto the stack.
        assert_eq!(app.modals.last(), Some(&crate::tui::ui::modal::Modal::Help));
        // Buffer was reset.
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }

    // ── prompt_input::submit_clears_buffer_and_resets_mode ──────────

    #[test]
    fn submit_clears_buffer_and_resets_mode() {
        let mut app = fresh_app();
        let _ = app.handle_key(k(KeyCode::Char('!')));
        let _ = app.handle_key(k(KeyCode::Char('x')));
        let _ = app.handle_key(k(KeyCode::Enter));
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.cursor, 0);
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(app.prompt.history_idx.is_none());
    }

    // ── home / end on multi-line ────────────────────────────────────

    #[test]
    fn home_end_target_current_line_only() {
        let mut app = fresh_app();
        app.set_input("ab\ncd");
        // cursor is at end (5).
        app.prompt.cursor = 4; // between c and d
        let _ = app.handle_key(k(KeyCode::Home));
        assert_eq!(app.prompt.cursor, 3); // start of "cd"
        let _ = app.handle_key(k(KeyCode::End));
        assert_eq!(app.prompt.cursor, 5); // end of buffer
    }

    // ── ctrl+e disambiguation (goal §10) ────────────────────────────

    #[test]
    fn ctrl_e_with_empty_buffer_toggles_tool_result() {
        let mut app = fresh_app();
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            output: "ok".into(),
            success: true,
        });
        let _ = app.handle_key(ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolResult { expanded, .. }) => assert!(*expanded),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_e_with_text_moves_to_end_of_line() {
        let mut app = fresh_app();
        app.set_input("hello");
        app.prompt.cursor = 1;
        let _ = app.handle_key(ctrl('e'));
        assert_eq!(app.prompt.cursor, 5);
    }

    #[test]
    fn ctrl_a_moves_to_line_start() {
        let mut app = fresh_app();
        app.set_input("hello");
        let _ = app.handle_key(ctrl('a'));
        assert_eq!(app.prompt.cursor, 0);
    }

    // ── exhaustively cover history's empty-on-down case ─────────────

    #[test]
    fn history_down_with_no_walk_in_progress_is_noop() {
        let mut app = fresh_app();
        // Down on empty, no history → falls through to scroll path.
        let _ = app.handle_key(k(KeyCode::Down));
        assert!(app.prompt.history_idx.is_none());
    }

    // ── strip_history_prefix utility ────────────────────────────────

    #[test]
    fn strip_history_prefix_recognises_all_modes() {
        assert_eq!(strip_history_prefix("!ls").0, InputMode::Bash);
        assert_eq!(strip_history_prefix("#note").0, InputMode::Note);
        assert_eq!(strip_history_prefix("/cmd").0, InputMode::Command);
        assert_eq!(strip_history_prefix("hello").0, InputMode::Prompt);
        assert_eq!(strip_history_prefix("!ls").1, "ls");
    }
}

// ── Goal-158: @file autocomplete tests ───────────────────────────────────────
#[cfg(test)]
mod atfile_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn atfile_mode_triggered_by_at_in_prompt_mode() {
        let mut app = App::new();
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);
        assert!(app.prompt.buffer.ends_with('@'));
    }

    #[test]
    fn atfile_mode_not_triggered_in_bash_mode() {
        let mut app = App::new();
        app.prompt.mode = InputMode::Bash;
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::Bash);
    }

    #[test]
    fn atfile_mode_not_triggered_in_command_mode() {
        let mut app = App::new();
        app.prompt.mode = InputMode::Command;
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::Command);
    }

    #[test]
    fn glob_workspace_files_filters_by_query_prefix() {
        // We can only test that the function returns a Vec and doesn't panic;
        // actual path results are environment-dependent.
        let results = glob_workspace_files("Cargo");
        // Should be ≤ MAX_ATFILE_SUGGESTIONS
        assert!(results.len() <= MAX_ATFILE_SUGGESTIONS);
        // All returned paths should contain "cargo" (case-insensitive)
        for r in &results {
            assert!(r.to_lowercase().contains("cargo"), "unexpected result: {r}");
        }
    }

    #[test]
    fn glob_workspace_files_returns_at_most_12() {
        let results = glob_workspace_files("");
        assert!(results.len() <= MAX_ATFILE_SUGGESTIONS);
    }

    #[test]
    fn atfile_backspace_on_empty_query_exits_mode_and_deletes_at() {
        let mut app = App::new();
        // Type some text, then '@'
        app.handle_char_input('h');
        app.handle_char_input('i');
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);
        assert_eq!(app.prompt.buffer, "hi@");

        // Backspace with empty query should exit mode and remove '@'
        app.handle_atfile_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "hi");
    }

    #[test]
    fn atfile_enter_inserts_selected_path_and_exits() {
        let mut app = App::new();
        app.handle_char_input('@');
        assert_eq!(app.prompt.mode, InputMode::AtFile);

        // Manually inject a suggestion so the test is deterministic.
        app.atfile_suggestions = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        app.atfile_selected = Some(0);

        // Press Enter to commit.
        app.handle_atfile_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert!(
            app.prompt.buffer.ends_with("@src/lib.rs"),
            "buffer was: {}",
            app.prompt.buffer
        );
    }

    #[test]
    fn atfile_esc_cancels_and_preserves_at_query() {
        let mut app = App::new();
        app.handle_char_input('t');
        app.handle_char_input('e');
        app.handle_char_input('s');
        app.handle_char_input('t');
        app.handle_char_input(' ');
        app.handle_char_input('@');
        // Type a query.
        app.handle_atfile_key(k(KeyCode::Char('s')));
        app.handle_atfile_key(k(KeyCode::Char('r')));
        app.handle_atfile_key(k(KeyCode::Char('c')));

        assert_eq!(app.prompt.mode, InputMode::AtFile);
        let buf_before = app.prompt.buffer.clone();

        // Press Esc — mode should exit but buffer kept.
        app.handle_atfile_key(k(KeyCode::Esc));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, buf_before);
        // Suggestion list is cleared.
        assert!(app.atfile_suggestions.is_empty());
    }

    #[test]
    fn atfile_up_down_navigation() {
        let mut app = App::new();
        app.handle_char_input('@');
        app.atfile_suggestions = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
        app.atfile_selected = None;

        // Down selects first item.
        app.handle_atfile_key(k(KeyCode::Down));
        assert_eq!(app.atfile_selected, Some(0));

        // Down again — second.
        app.handle_atfile_key(k(KeyCode::Down));
        assert_eq!(app.atfile_selected, Some(1));

        // Up — back to first.
        app.handle_atfile_key(k(KeyCode::Up));
        assert_eq!(app.atfile_selected, Some(0));

        // Up again — deselects (None).
        app.handle_atfile_key(k(KeyCode::Up));
        assert_eq!(app.atfile_selected, None);
    }
}

#[cfg(test)]
mod hsearch_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn history_app(entries: &[&str]) -> App {
        let mut app = App::new();
        for e in entries {
            app.prompt.history.push(e.to_string());
        }
        app
    }

    // ── search_history unit tests ──────────────────────────────────────

    #[test]
    fn history_search_empty_query_returns_all_reversed() {
        let h = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let r = search_history(&h, "");
        // Most recent first: indices 2,1,0.
        assert_eq!(r, vec![2, 1, 0]);
    }

    #[test]
    fn history_search_prefix_match_ranked_first() {
        let h = vec![
            "foo bar".to_string(),
            "zz foo".to_string(),
            "foobar".to_string(),
        ];
        let r = search_history(&h, "foo");
        // Entries 0 and 2 start with "foo"; entry 1 is a substring match.
        // Prefix matches come first; within prefix group, reversed = 2 then 0.
        assert!(r.iter().position(|&x| x == 2) < r.iter().position(|&x| x == 1));
        assert!(r.iter().position(|&x| x == 0) < r.iter().position(|&x| x == 1));
    }

    #[test]
    fn history_search_case_insensitive() {
        let h = vec!["Hello World".to_string(), "goodbye".to_string()];
        let r = search_history(&h, "hello");
        assert!(r.contains(&0));
        assert!(!r.contains(&1));
    }

    #[test]
    fn history_search_returns_at_most_12() {
        let h: Vec<String> = (0..20).map(|i| format!("entry {i}")).collect();
        let r = search_history(&h, "entry");
        assert!(r.len() <= MAX_HSEARCH_RESULTS);
    }

    // ── App integration tests ──────────────────────────────────────────

    #[test]
    fn ctrl_r_in_prompt_mode_enters_history_search() {
        let mut app = history_app(&["hello", "world"]);
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        // All entries pre-loaded.
        assert_eq!(app.hsearch_matches.len(), 2);
    }

    #[test]
    fn ctrl_r_in_bash_mode_no_op() {
        let mut app = history_app(&["hello"]);
        app.prompt.mode = InputMode::Bash;
        app.handle_key(ctrl(KeyCode::Char('r')));
        // Should stay in Bash mode, not HistorySearch.
        assert_eq!(app.prompt.mode, InputMode::Bash);
    }

    #[test]
    fn history_search_enter_fills_buffer() {
        let mut app = history_app(&["cargo build", "cargo test"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        // With empty query, most recent first: index 1 ("cargo test") selected.
        assert_eq!(app.hsearch_selected, 0);
        // Press Enter → fill buffer with the selected entry.
        app.handle_history_search_key(k(KeyCode::Enter));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        assert_eq!(app.prompt.buffer, "cargo test");
    }

    #[test]
    fn history_search_esc_cancels() {
        let mut app = history_app(&["hello"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        app.handle_history_search_key(k(KeyCode::Esc));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
        // Buffer should be unchanged.
        assert!(app.prompt.buffer.is_empty());
    }

    #[test]
    fn history_search_backspace_on_empty_exits_mode() {
        let mut app = history_app(&["hello"]);
        app.handle_key(ctrl(KeyCode::Char('r')));
        assert_eq!(app.prompt.mode, InputMode::HistorySearch);
        assert!(app.hsearch_query.is_empty());
        // Backspace on empty query exits HistorySearch.
        app.handle_history_search_key(k(KeyCode::Backspace));
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }
}

// ── Goal-161: Permission Modal tests ─────────────────────────────────────────

#[cfg(test)]
mod perm_tests {
    use super::*;

    fn make_perm(tool: &str, args: &str) -> (App, tokio::sync::oneshot::Receiver<bool>) {
        let mut app = App::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let req = crate::tui::events::PermissionRequest {
            tool_name: tool.to_string(),
            args_preview: args.to_string(),
            reply: tx,
        };
        app.set_pending_permission(req);
        (app, rx)
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn pending_permission_set_and_stored() {
        let (app, _rx) = make_perm("run_shell", "ls -la");
        assert!(app.pending_permission.is_some());
        let p = app.pending_permission.as_ref().unwrap();
        assert_eq!(p.tool_name, "run_shell");
        assert_eq!(p.args_preview, "ls -la");
    }

    #[tokio::test]
    async fn y_key_sends_true_and_clears_modal() {
        let (mut app, rx) = make_perm("run_shell", "ls");
        app.handle_permission_key(k(KeyCode::Char('y')));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn n_key_sends_false_and_clears_modal() {
        let (mut app, rx) = make_perm("run_shell", "rm -rf /");
        app.handle_permission_key(k(KeyCode::Char('n')));
        assert!(app.pending_permission.is_none());
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn esc_key_sends_false() {
        let (mut app, rx) = make_perm("write_file", "path=foo.txt");
        app.handle_permission_key(k(KeyCode::Esc));
        assert!(app.pending_permission.is_none());
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn enter_key_sends_true() {
        let (mut app, rx) = make_perm("read_file", "path=foo.txt");
        app.handle_permission_key(k(KeyCode::Enter));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn a_key_sends_true_and_adds_to_auto_allowed() {
        let (mut app, rx) = make_perm("run_shell", "cargo test");
        app.handle_permission_key(k(KeyCode::Char('a')));
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
        assert!(app.auto_allowed_tools.contains("run_shell"));
    }

    #[tokio::test]
    async fn auto_allowed_tool_skips_modal() {
        let mut app = App::new();
        app.auto_allowed_tools.insert("run_shell".to_string());
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let req = crate::tui::events::PermissionRequest {
            tool_name: "run_shell".to_string(),
            args_preview: "cargo build".to_string(),
            reply: tx,
        };
        // Should auto-allow without storing to pending_permission.
        app.set_pending_permission(req);
        assert!(app.pending_permission.is_none());
        assert!(rx.await.unwrap());
    }

    #[test]
    fn handle_key_routes_to_permission_when_pending() {
        // When pending_permission is set, handle_key routes to permission handler.
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        let mut app = App::new();
        let req = crate::tui::events::PermissionRequest {
            tool_name: "write_file".to_string(),
            args_preview: "path=foo.rs".to_string(),
            reply: tx,
        };
        app.pending_permission = Some(PendingPermission {
            tool_name: req.tool_name,
            args_preview: req.args_preview,
            reply: req.reply,
        });
        assert!(app.pending_permission.is_some());
        // N key via handle_key should route to permission handler.
        app.handle_key(k(KeyCode::Char('n')));
        assert!(app.pending_permission.is_none());
    }
}

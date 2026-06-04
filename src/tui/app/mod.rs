//! Application state for the Recursive TUI.
//!
//! [`App`] owns everything visible to the user: the transcript blocks,
//! the input buffer, the current screen, scroll position, and bookkeeping
//! for streaming, usage, and per-turn timing.
//!
//! This module contains only the struct definition and re-exports.
//! Implementation is split across:
//! - [`state`]     — constructors, basic accessors, transcript mutation helpers
//! - [`event_loop`] — `handle_ui_event` + streaming helpers
//! - [`commands`]  — keyboard dispatch, modals, atfile, history search, permissions
//! - [`render`]    — standalone helper functions (preview_args, verb_for_tool, parse_*)

use std::collections::HashSet;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

use crate::runtime::GoalState;

pub mod commands;
pub mod event_loop;
pub mod render;
pub mod state;

// Re-export from sub-modules so the rest of the codebase can still do
// `use crate::tui::app::Foo` without any changes.
pub use crate::tui::completion::{
    collect_files, default_offline_tool_catalog, glob_workspace_files, search_history,
    MAX_ATFILE_SUGGESTIONS, MAX_HSEARCH_RESULTS,
};
pub use crate::tui::cost::{detect_model_name, estimate_cost, TurnState, UsageStats};
pub use crate::tui::input_state::{
    double_press_window, strip_history_prefix, DoublePressTracker, InputMode, PromptInputState,
    DOUBLE_PRESS_WINDOW, HISTORY_CAPACITY,
};
pub use crate::tui::model::{
    AppScreen, DiffHunk, DiffLine, DiffLineKind, ToolResultData, TranscriptBlock,
};
pub use render::{parse_v4a_patch, preview_args, verb_for_tool};

// ──────────────────────────────────────────────────────────────────────
// Top-level App struct
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
    /// Tracks when the TUI session started. Used by `/status` to report uptime.
    pub start_time: Instant,
    pub usage: UsageStats,
    pub turn: TurnState,
    pub turn_count: u64,
    pub pending_latency_ms: Option<u64>,
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
    /// Goal-202: set when the agent has called `request_plan_mode` and we are
    /// waiting for the user to allow or skip planning. Cleared by
    /// `PlanModeApproved` / `PlanModeRejected` events.
    pub plan_mode_request_pending: bool,
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

    // ── Progressive output ───────────────────────────────────────────────
    /// Blocks from `self.blocks[0..last_printed_idx]` have already been
    /// flushed to the terminal's scrollback buffer via
    /// `terminal.insert_before()`. The inline viewport only renders
    /// blocks at index `>= last_printed_idx` (in-flight content).
    pub last_printed_idx: usize,
    /// Queue of rendered lines waiting to be pushed to the scrollback
    /// buffer in the next event-loop iteration. Drained by the main
    /// loop using `terminal.insert_before()`.
    pub print_queue: Vec<Vec<ratatui::text::Line<'static>>>,

    // ── Modal scroll ─────────────────────────────────────────────────────
    /// Vertical scroll offset (in lines) for the currently-active modal.
    /// Reset to 0 whenever a new modal is pushed. For list-based modals
    /// (ResumePicker, McpServers, Journal) the key handler auto-updates
    /// this to keep the selection visible.
    pub modal_scroll: u16,
}

// ── Goal-161: PendingPermission ──────────────────────────────────────────────

/// Holds the state for the permission-request modal while it is open.
/// The `reply` sender is consumed exactly once when the user presses Y or N.
pub struct PendingPermission {
    pub tool_name: String,
    pub args_preview: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

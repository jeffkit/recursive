//! Input mode and prompt-input state for the Recursive TUI.
//!
//! Contains the multi-mode input machinery: [`InputMode`], the mutable
//! [`PromptInputState`] buffer, double-press tracking for Esc/Ctrl+C, and
//! history-ring management.

use std::time::Instant;

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
// InputMode (Goal 145)
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

pub fn strip_history_prefix(raw: &str) -> (InputMode, &str) {
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

//! In-process test harness for the TUI — the AI's "eyes".
//!
//! Drives an [`App`] with key events and [`UiEvent`]s, then renders it
//! onto an offscreen `ratatui::Buffer` via [`ratatui::backend::TestBackend`].
//! No real terminal, no async backend, no wall-clock drift. This lets
//! tests (and the AI authoring them) observe exactly what the user would
//! see and assert on it — closing the observation loop that makes TUI
//! testing tractable.
//!
//! # The two loops this enables
//!
//! - **Observation:** `harness.render().text()` / `numbered()` gives a
//!   deterministic string snapshot of the screen the AI can read and
//!   assert against. Cell-level style access (`bg`, `style`) lets
//!   tests verify *visual* properties (e.g. the highlight bar and the
//!   `▶` marker share a row) rather than just internal state.
//! - **Effectiveness:** because these tests are fast and deterministic,
//!   they are the substrate `cargo-mutants` mutates against — surviving
//!   mutants in a touched file point directly at weak/missing assertions.
//!
//! # What this is NOT
//!
//! The harness exercises the logic + rendering layers in-process. It does
//! not run the real binary, real keystroke raw-mode handling, or ANSI
//! round-trip. That integration layer is covered by the separate
//! `tui-pty` harness (stage 4).
//!
//! # Effectiveness loop — "改某文件 → 杀该文件变异点"
//!
//! A harness test that passes can still be useless (a tautology, or it
//! asserts on the wrong thing). The effectiveness check is mutation
//! testing via `cargo-mutants`: mutate the source the AI just touched and
//! re-run the tests. A mutant that *survives* (tests still green) marks a
//! real coverage gap — strengthen or add the test, then re-run until the
//! touched file's mutants are all killed.
//!
//! Keep runs fast by scoping to the touched files only:
//!
//! ```text
//! .dev/scripts/tui-mutants.sh                       # auto-detect changed files vs main
//! .dev/scripts/tui-mutants.sh crates/recursive-tui/src/app/render.rs
//! .dev/scripts/tui-mutants.sh --dir crates/recursive-tui/src/app
//! ```
//!
//! Exit code is non-zero if any mutant survives, so the script can gate a
//! commit. The full SOP (in-process harness → mutation gate → PTY tour →
//! quality gates) is codified in `.dev/skills/tui-acceptance.md`, and the
//! goal template at `.dev/goals/_TEMPLATE-tui-acceptance.md` embeds it
//! into every TUI goal's Acceptance section so the self-improve loop
//! follows it automatically.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use ratatui::Terminal;

use crate::app::App;
use crate::events::{UiEvent, UserAction};
use crate::keymap;
use crate::ui;

/// Default virtual terminal size used by [`Harness::new`].
pub const DEFAULT_WIDTH: u16 = 80;
pub const DEFAULT_HEIGHT: u16 = 24;

/// An owned snapshot of the rendered screen.
///
/// Cheap to clone; produced by [`Harness::render`]. All style/style
/// queries index by `(x, y)` with origin at the top-left cell.
#[derive(Clone)]
pub struct Screen {
    width: u16,
    height: u16,
    buf: Buffer,
}

impl Screen {
    /// Terminal width in cells.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Terminal height in rows.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// The symbol drawn at `(x, y)`. Wide / combining glyphs occupy a
    /// single cell index and are returned as their full string form.
    pub fn cell(&self, x: u16, y: u16) -> &str {
        self.buf[(x, y)].symbol()
    }

    /// The resolved style at `(x, y)`.
    pub fn style(&self, x: u16, y: u16) -> Style {
        self.buf[(x, y)].style()
    }

    /// The background colour at `(x, y)`, or `None` if unset / `Reset`.
    /// Use this when you need to tell a highlight bar apart from a panel
    /// block's uniform base fill.
    pub fn bg(&self, x: u16, y: u16) -> Option<Color> {
        match self.buf[(x, y)].style().bg {
            Some(Color::Reset) | None => None,
            Some(c) => Some(c),
        }
    }

    /// `true` if any cell on row `y` carries the specific background `color`.
    pub fn row_has_bg_color(&self, y: u16, color: Color) -> bool {
        (0..self.width).any(|x| self.bg(x, y) == Some(color))
    }

    /// `true` if any cell on row `y` has a background fill other than `base`.
    /// Pass a panel's base colour to filter out its uniform block background.
    pub fn row_has_bg_other_than(&self, y: u16, base: Color) -> bool {
        (0..self.width).any(|x| matches!(self.bg(x, y), Some(c) if c != base))
    }

    /// The full text of row `y`, with trailing spaces trimmed.
    pub fn line(&self, y: u16) -> String {
        let mut s = String::new();
        for x in 0..self.width {
            s.push_str(self.buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    /// Every row as trimmed text.
    pub fn lines(&self) -> Vec<String> {
        (0..self.height).map(|y| self.line(y)).collect()
    }

    /// The whole screen as one string, rows joined by `\n`, trailing
    /// blank rows dropped. Stable across renders as long as `App` state
    /// and the frozen-spinner flag are unchanged.
    pub fn text(&self) -> String {
        let mut rows: Vec<String> = self.lines();
        while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
            rows.pop();
        }
        rows.join("\n")
    }

    /// The screen with `NN|` row-number prefixes — the form the AI reads
    /// when reasoning about layout. Useful for assertion-failure messages
    /// and for snapshots where line numbers aid review.
    pub fn numbered(&self) -> String {
        let rows = self.lines();
        let width = self.height.to_string().len();
        rows.iter()
            .enumerate()
            .map(|(i, r)| format!("{:>width$}| {r}", i, width = width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// First row index whose text contains `needle`, or `None`.
    pub fn find_row(&self, needle: &str) -> Option<u16> {
        (0..self.height).find(|&y| self.line(y).contains(needle))
    }
}

/// In-process TUI driver.
///
/// Owns an [`App`] and a queue of [`UserAction`]s emitted by the keymap
/// (which, in production, would be forwarded to the backend worker). The
/// harness deliberately does **not** spin up a backend — tests that need
/// a model round-trip pump [`UiEvent`]s in by hand via [`Harness::pump`].
pub struct Harness {
    app: App,
    width: u16,
    height: u16,
    actions: VecDeque<UserAction>,
}

impl Harness {
    /// New harness with an [`App::new`] and the default terminal size.
    pub fn new() -> Self {
        Self::with_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// New harness with a custom virtual terminal size.
    pub fn with_size(width: u16, height: u16) -> Self {
        // `App::new` initialises `spinner_frame` to 0 and the harness never
        // increments it (the real loop does that out-of-band in `lib::run`),
        // so renders are deterministic by construction — no freeze flag needed.
        Self {
            app: App::new(),
            width,
            height,
            actions: VecDeque::new(),
        }
    }

    /// Borrow the underlying app state.
    pub fn app(&self) -> &App {
        &self.app
    }

    /// Mutably borrow the underlying app state — for setting up fixture
    /// state that has no keyboard shortcut (e.g. seeding `blocks`).
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Actions emitted by the keymap since the last call to
    /// [`Harness::drain_actions`], in emission order. These are what the
    /// UI would have asked the backend worker to do.
    pub fn actions(&self) -> &VecDeque<UserAction> {
        &self.actions
    }

    /// Remove and return all queued actions.
    pub fn drain_actions(&mut self) -> Vec<UserAction> {
        self.actions.drain(..).collect()
    }

    // ── input ───────────────────────────────────────────────────────

    /// Dispatch a single key event through the keymap. Any [`UserAction`]
    /// returned is queued (see [`Harness::actions`]).
    pub fn type_key(&mut self, key: KeyEvent) {
        if let Some(action) = keymap::dispatch(&mut self.app, key) {
            self.actions.push_back(action);
        }
    }

    /// Type a single character (no modifiers).
    pub fn type_char(&mut self, c: char) {
        self.type_key(plain(c));
    }

    /// Type a string of characters, no Enter. Useful for filling the
    /// input box without submitting.
    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.type_char(c);
        }
    }

    /// Press Enter.
    pub fn enter(&mut self) {
        self.type_key(plain_enter());
    }

    /// Press Ctrl+`c`.
    pub fn ctrl(&mut self, c: char) {
        self.type_key(ctrl(c));
    }

    /// Convenience: type the string then press Enter, returning any
    /// actions produced (typically one `SendMessage`).
    pub fn submit(&mut self, s: &str) -> Vec<UserAction> {
        self.type_str(s);
        self.enter();
        self.drain_actions()
    }

    // ── events from the (simulated) backend ──────────────────────────

    /// Apply a [`UiEvent`] to the app — the in-process equivalent of the
    /// event loop's `backend.event_rx.recv()` arm.
    pub fn pump(&mut self, event: UiEvent) {
        self.app.handle_ui_event(event);
    }

    /// Apply a sequence of events in order.
    pub fn pump_many(&mut self, events: impl IntoIterator<Item = UiEvent>) {
        for ev in events {
            self.pump(ev);
        }
    }

    // ── observation ──────────────────────────────────────────────────

    /// Render the current app state to an owned [`Screen`] snapshot.
    pub fn render(&self) -> Screen {
        let backend = TestBackend::new(self.width, self.height);
        // TestBackend::new is infallible by construction; the only error
        // path is an io failure on draw, which TestBackend cannot raise.
        let mut terminal = terminal(backend);
        terminal
            .draw(|f| ui::render(f, &self.app))
            .expect("test backend draw is infallible");
        let buf = terminal.backend().buffer().clone();
        Screen {
            width: self.width,
            height: self.height,
            buf,
        }
    }

    /// Render and return [`Screen::text`].
    pub fn screen_text(&self) -> String {
        self.render().text()
    }

    /// Render and return [`Screen::numbered`].
    pub fn screen_numbered(&self) -> String {
        self.render().numbered()
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

// `Terminal::new` returns `io::Result`; TestBackend construction never
// fails, but the type still requires us to go through `?`/expect. This
// helper keeps call sites clean. Allowed: harness is test-only.
fn terminal(backend: TestBackend) -> Terminal<TestBackend> {
    Terminal::new(backend).expect("TestBackend::new is infallible")
}

// ── key constructors ───────────────────────────────────────────────────

fn plain(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn plain_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::UiEvent;
    use crate::model::TranscriptBlock;

    #[test]
    fn harness_renders_empty_app_without_panic() {
        let h = Harness::new();
        let screen = h.render();
        // An empty app renders the splash; the screen is non-empty text.
        assert!(!screen.text().is_empty());
    }

    #[test]
    fn type_str_then_enter_emits_send_message() {
        let mut h = Harness::new();
        let actions = h.submit("hello");
        assert_eq!(actions, vec![UserAction::SendMessage("hello".into())]);
    }

    #[test]
    fn pump_assistant_message_appears_on_screen() {
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "Hi from the model".into(),
        });
        let text = h.screen_text();
        assert!(
            text.contains("Hi from the model"),
            "assistant text should be visible on screen:\n{}",
            h.screen_numbered()
        );
    }

    #[test]
    fn screen_text_is_stable_across_renders() {
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "stable".into(),
        });
        let a = h.screen_text();
        let b = h.screen_text();
        assert_eq!(a, b, "renders must be deterministic");
    }

    #[test]
    fn find_row_locates_highlighted_block() {
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "needle-text".into(),
        });
        let screen = h.render();
        assert!(screen.find_row("needle-text").is_some());
    }

    #[test]
    fn numbered_includes_row_prefixes() {
        let h = Harness::new();
        let numbered = h.screen_numbered();
        assert!(
            numbered.contains("0|"),
            "row 0 should be prefixed: {numbered}"
        );
    }

    #[test]
    fn blocks_fixture_renders_user_message() {
        let mut h = Harness::new();
        h.app_mut().blocks.push(TranscriptBlock::User {
            text: "fixture line".into(),
        });
        assert!(h.screen_text().contains("fixture line"));
    }

    #[test]
    fn drain_actions_clears_queue() {
        let mut h = Harness::new();
        h.type_str("one");
        h.enter();
        assert!(
            !h.actions().is_empty(),
            "typing + enter should queue an action"
        );
        let drained = h.drain_actions();
        assert_eq!(drained.len(), 1);
        assert!(h.actions().is_empty(), "drain_actions must clear the queue");
    }

    // ── Stage 2: real visual acceptance tests ───────────────────────────
    //
    // These exercise the two `/resume`-area bugs fixed in b202dc8 at the
    // *rendered* layer, not just the structural layer. The structural test
    // `theme_panel_list_offset_aligns_highlight_with_marker` (in commands.rs)
    // compares `lines` indices; the tests below compare what actually lands
    // on screen — catching the visual misalignment the user originally saw.

    use crate::app::InputMode;
    use crate::commands::{CommandHandler, CommandOutcome};
    use ratatui::style::Color;

    /// The interact-panel highlight colour (must match
    /// `render_command_interact_panel`'s `selected_style`).
    const HIGHLIGHT: Color = Color::Rgb(205, 100, 50);

    /// Invoke a sync slash-command that opens a panel and return the panel.
    fn open_panel(app: &mut App, name: &str, args: &[String]) -> crate::app::CommandPanelState {
        let registry = app.commands.clone();
        let spec = registry
            .lookup(name)
            .unwrap_or_else(|| panic!("/{name} registered"));
        match &spec.handler {
            CommandHandler::Sync(f) => match f(app, args) {
                CommandOutcome::OpenPanel(panel) => panel,
                other => panic!("/{name} expected OpenPanel, got {other:?}"),
            },
            _ => panic!("/{name} is not a sync command"),
        }
    }

    #[test]
    fn theme_panel_marker_row_carries_highlight_bg() {
        // Regression: the highlight bar must land on the same screen row as
        // the `▶` marker. Before the list_offset fix the bar sat on the
        // header row and the `▶` row had only the panel's base (Black) fill.
        let mut h = Harness::new();
        let panel = open_panel(h.app_mut(), "theme", &[]);
        h.app_mut().active_command_panel = Some(panel);
        h.app_mut().prompt.mode = InputMode::CommandInteract;

        let screen = h.render();
        let marker_row = screen
            .find_row("▶")
            .expect("a ▶ marker should be rendered on screen");
        assert!(
            screen.row_has_bg_color(marker_row, HIGHLIGHT),
            "the ▶ marker row must carry the highlight bar (visual alignment)\n{}",
            screen.numbered()
        );
    }

    #[test]
    fn theme_panel_header_row_is_not_highlighted() {
        // Companion guard: the header row ("Choose theme …") must NOT carry
        // the highlight colour. This is the row the buggy config
        // (list_offset = 0) would have highlighted instead of the item.
        let mut h = Harness::new();
        let panel = open_panel(h.app_mut(), "theme", &[]);
        h.app_mut().active_command_panel = Some(panel);
        h.app_mut().prompt.mode = InputMode::CommandInteract;

        let screen = h.render();
        let header_row = screen
            .find_row("Choose theme")
            .expect("the panel header should be rendered");
        assert!(
            !screen.row_has_bg_color(header_row, HIGHLIGHT),
            "the header row must not carry the highlight — the bar belongs on the item row\n{}",
            screen.numbered()
        );
    }

    #[test]
    fn model_panel_marker_row_carries_highlight_bg() {
        // Regression: the /model picker must render the orange highlight bar on
        // the ▶ marker row, like /theme. Set a key so the picker is non-empty,
        // then render and inspect the actual buffer (not just panel state).
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-dummy");

        let mut h = Harness::new();
        let panel = open_panel(h.app_mut(), "model", &[]);
        h.app_mut().active_command_panel = Some(panel);
        h.app_mut().prompt.mode = InputMode::CommandInteract;

        let screen = h.render();
        let marker_row = screen
            .find_row("▶")
            .expect("a ▶ marker should render on screen");
        assert!(
            screen.row_has_bg_color(marker_row, HIGHLIGHT),
            "the ▶ marker row must carry the highlight bar\n{}",
            screen.numbered()
        );

        match prev {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    #[test]
    fn model_panel_shows_current_custom_model_as_synthetic_entry() {
        // When the active model isn't offered by any preset (a custom provider
        // configured by raw api_base + model), the picker must still show it:
        // the header names the real running model and a synthetic
        // "Current (custom provider)" row is prepended with a ✓.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic-dummy");

        let mut h = Harness::new();
        // Force a custom active model that no preset lists.
        h.app_mut().active_preset = None;
        h.app_mut().model_name = "z-ai/glm-5.2".to_string();
        let panel = open_panel(h.app_mut(), "model", &[]);
        h.app_mut().active_command_panel = Some(panel);
        h.app_mut().prompt.mode = InputMode::CommandInteract;

        let screen = h.render();
        let text = screen.text();
        assert!(
            text.contains("current: z-ai/glm-5.2"),
            "header must name the real running model, got:\n{text}"
        );
        assert!(
            text.contains("Current (custom provider)"),
            "synthetic current row must appear, got:\n{text}"
        );
        assert!(
            text.contains('✓'),
            "the synthetic current row must carry a checkmark, got:\n{text}"
        );

        match prev {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    #[test]
    fn model_panel_viewport_follows_cursor_for_long_list() {
        // Regression: when the picker lists more models than fit on screen
        // (MAX_VISIBLE = 8 content rows), the viewport must scroll to keep
        // the cursor visible. Before the fix the renderer always showed the
        // first `content_rows` lines, so a cursor near the bottom was
        // off-screen and the user had to press ↑ many times before the
        // highlight appeared. Build a 30-row panel with the cursor at row 25
        // and assert the ▶ row is on screen and carries the highlight bar.
        use crate::app::CommandPanelState;
        use crate::commands::{build_model_picker_lines, ModelPickerEntry};

        let entries: Vec<ModelPickerEntry> = (0..30)
            .map(|i| ModelPickerEntry {
                preset_id: "p".into(),
                preset_name: "P".into(),
                model: format!("m-{i}"),
                context_window: 0,
                pricing: None,
            })
            .collect();
        // active = 25 (✓ on row 25), selected = 25 (▶ on row 25).
        let lines = build_model_picker_lines(&entries, "m-25", 25, 25);
        // list_offset = 2 (header + blank), so the cursor sits at line 27.
        let panel = CommandPanelState::new("model", lines)
            .with_selection(25)
            .with_item_count(30)
            .with_list_offset(2)
            .with_hint("↑↓ / Ctrl+P Ctrl+N select  ·  enter switch  ·  esc cancel");

        let mut h = Harness::new();
        h.app_mut().active_command_panel = Some(panel);
        h.app_mut().prompt.mode = InputMode::CommandInteract;

        let screen = h.render();
        let marker_row = screen
            .find_row("▶")
            .expect("the ▶ marker must be scrolled into view\n{}");
        assert!(
            screen.row_has_bg_color(marker_row, HIGHLIGHT),
            "the ▶ marker row must carry the highlight bar\n{}",
            screen.numbered()
        );
        // The active model's name must be visible (the viewport followed the
        // cursor to row 25, not stayed at the top showing m-0..m-7).
        assert!(
            screen.text().contains("m-25"),
            "the cursor's model must be visible after scroll\n{}",
            screen.numbered()
        );
    }

    #[test]
    fn session_resumed_replaces_visible_transcript() {
        // Regression: `/resume` must REPLACE the visible conversation, not
        // append to it. Pump some old content, then a SessionResumed event
        // carrying a fresh transcript; the old content must vanish from the
        // screen and the resumed dialogue must appear.
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "OLD-TURN-MUST-VANISH".into(),
        });

        h.pump(UiEvent::SessionResumed {
            session_id: "s1".into(),
            turn_count: 3,
            blocks: vec![
                TranscriptBlock::User {
                    text: "resumed question".into(),
                },
                TranscriptBlock::Assistant {
                    text: "resumed answer".into(),
                    streaming: false,
                    latency_ms: None,
                },
            ],
        });

        let text = h.screen_text();
        assert!(
            text.contains("resumed question") && text.contains("resumed answer"),
            "resumed transcript should be visible:\n{}",
            h.screen_numbered()
        );
        assert!(
            !text.contains("OLD-TURN-MUST-VANISH"),
            "old transcript must be replaced, not appended:\n{}",
            h.screen_numbered()
        );
    }

    #[test]
    fn session_resumed_appends_resume_note() {
        let mut h = Harness::new();
        h.pump(UiEvent::SessionResumed {
            session_id: "abc123".into(),
            turn_count: 9,
            blocks: vec![TranscriptBlock::User { text: "q".into() }],
        });
        let text = h.screen_text();
        assert!(
            text.contains("Resumed session abc123") && text.contains("9 messages"),
            "a resume note should be appended after the resumed transcript:\n{}",
            h.screen_numbered()
        );
    }
}

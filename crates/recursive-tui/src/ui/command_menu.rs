//! Command-completion popup (Goal 146).
//!
//! When the prompt input is in [`crate::app::InputMode::Command`],
//! the chat renderer overlays a small popup just above the input box
//! that lists the candidate commands matching the current buffer.
//!
//! The popup is purely visual — keyboard navigation lives in
//! [`crate::app::App::handle_command_menu_key`], which is called
//! from [`crate::app::App::handle_key`] before falling through to
//! the generic chat key path.

use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::commands::CommandSpec;

/// Maximum number of candidate rows to display in the popup.
pub const MAX_VISIBLE: usize = 8;

/// Compute the longest common prefix of `s1` and `s2`. Pure, used by
/// [`tab_completion_target`] and exposed for tests.
pub fn longest_common_prefix<'a>(s1: &'a str, s2: &'a str) -> &'a str {
    let limit = s1.len().min(s2.len());
    let mut idx = 0;
    while idx < limit && s1.as_bytes()[idx] == s2.as_bytes()[idx] {
        idx += 1;
    }
    &s1[..idx]
}

/// Return the canonical name to autocomplete to, given the current
/// buffer and the set of matches.
///
/// Algorithm: zero matches yields `None`; exactly one match yields
/// `Some(canonical_name)`; multiple matches yield the longest common
/// prefix of all canonical names, but only if it strictly extends
/// the buffer (otherwise `None`, leaving the buffer alone).
pub fn tab_completion_target(buffer: &str, matches: &[&CommandSpec]) -> Option<String> {
    let buf = buffer.trim_start_matches('/');
    match matches.len() {
        0 => None,
        1 => Some(matches[0].name.to_string()),
        _ => {
            let mut common: &str = matches[0].name;
            for m in &matches[1..] {
                common = longest_common_prefix(common, m.name);
                if common.is_empty() {
                    break;
                }
            }
            if common.len() > buf.len() {
                Some(common.to_string())
            } else {
                None
            }
        }
    }
}

/// Compute the rectangle to render the popup in, given the input
/// box's `area` and the number of candidate rows. Returns `None`
/// when the popup doesn't fit (terminal too short or candidates
/// empty).
pub fn popup_rect(input_area: Rect, candidate_count: usize, frame_area: Rect) -> Option<Rect> {
    if candidate_count == 0 {
        return None;
    }
    // 2 borders + N rows
    let popup_h = (candidate_count.min(MAX_VISIBLE) as u16) + 2;
    if input_area.y < popup_h || frame_area.y > input_area.y - popup_h {
        return None;
    }
    let popup_w = input_area.width.clamp(20, 60);
    Some(Rect {
        x: input_area.x,
        y: input_area.y - popup_h,
        width: popup_w,
        height: popup_h,
    })
}

/// Render the popup. No-op when the input mode is not Command, the
/// buffer is empty, or no matches are available.
pub fn render(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.prompt.mode != crate::app::InputMode::Command {
        return;
    }
    let matches = app.commands.search(&app.prompt.buffer);
    if matches.is_empty() {
        return;
    }
    let visible = &matches[..matches.len().min(MAX_VISIBLE)];
    let Some(area) = popup_rect(input_area, visible.len(), frame.area()) else {
        return;
    };
    frame.render_widget(Clear, area);

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let summary_style = Style::default().fg(Color::DarkGray);

    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let style = if app.command_menu_selected == Some(i) {
                selected_style
            } else {
                normal_style
            };
            Line::from(vec![
                Span::styled(format!(" /{:<10} ", spec.name), style),
                Span::styled(spec.summary.to_string(), summary_style),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Commands ")
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppScreen, InputMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fresh_command_app() -> App {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = InputMode::Command;
        app
    }

    #[test]
    fn longest_common_prefix_works() {
        assert_eq!(longest_common_prefix("clear", "compact"), "c");
        assert_eq!(longest_common_prefix("help", "help"), "help");
        assert_eq!(longest_common_prefix("a", "b"), "");
    }

    #[test]
    fn tab_completes_unique_prefix() {
        let r = crate::commands::CommandRegistry::default_set();
        // "he" → only /help; should complete to "help".
        let matches = r.search("he");
        let target = tab_completion_target("he", &matches);
        assert_eq!(target.as_deref(), Some("help"));
    }

    #[test]
    fn tab_extends_to_common_prefix_with_multiple_matches() {
        let r = crate::commands::CommandRegistry::default_set();
        // "co" matches /compact and /cost → common prefix "co"; not a
        // strict extension of "co", so target is None.
        let matches = r.search("co");
        assert_eq!(tab_completion_target("co", &matches), None);
        // "c" matches /clear /compact /cost → common is "c"; no
        // extension.
        let matches = r.search("c");
        assert_eq!(tab_completion_target("c", &matches), None);
    }

    #[test]
    fn tab_with_no_matches_returns_none() {
        let r = crate::commands::CommandRegistry::default_set();
        let matches = r.search("zz");
        assert!(tab_completion_target("zz", &matches).is_none());
    }

    #[test]
    fn up_down_moves_selection() {
        let mut app = fresh_command_app();
        // "c" matches 3 entries: clear, compact, cost.
        app.prompt.buffer = "c".into();
        app.prompt.cursor = 1;

        // Down enters the menu and selects the first item.
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command_menu_selected, Some(0));
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command_menu_selected, Some(1));
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command_menu_selected, Some(0));
        // Up at the top wraps off (None).
        let _ = app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command_menu_selected, None);
    }

    #[test]
    fn enter_runs_selected_command() {
        let mut app = fresh_command_app();
        app.prompt.buffer = "c".into();
        app.prompt.cursor = 1;
        // Select index 0 → /clear.
        let _ = app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command_menu_selected, Some(0));
        let _ = app.handle_key(key(KeyCode::Enter));
        // /clear resets the transcript to a single "cleared" block.
        assert_eq!(app.blocks.len(), 1);
        match &app.blocks[0] {
            crate::app::TranscriptBlock::System { text } => {
                assert!(text.contains("cleared"), "got {text:?}")
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn tab_completes_buffer() {
        let mut app = fresh_command_app();
        app.prompt.buffer = "he".into();
        app.prompt.cursor = 2;
        let _ = app.handle_key(key(KeyCode::Tab));
        // Buffer extends to "help".
        assert_eq!(app.prompt.buffer, "help");
    }

    #[test]
    fn esc_clears_buffer_and_exits_command_mode() {
        let mut app = fresh_command_app();
        app.prompt.buffer = "co".into();
        app.prompt.cursor = 2;
        // App's existing Esc behaviour clears buffer + reverts to
        // Prompt; we just verify it's not blocked by the menu logic.
        let _ = app.handle_key(key(KeyCode::Esc));
        assert!(app.prompt.buffer.is_empty());
        assert_eq!(app.prompt.mode, InputMode::Prompt);
    }
}

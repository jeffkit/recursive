//! Command-completion popup (Goal 146).
//!
//! When the prompt input is in [`crate::tui::app::InputMode::Command`],
//! the chat renderer overlays a small popup just above the input box
//! that lists the candidate commands matching the current buffer.
//!
//! The popup is purely visual — keyboard navigation lives in
//! [`crate::tui::app::App::handle_command_menu_key`], which is called
//! from [`crate::tui::app::App::handle_key`] before falling through to
//! the generic chat key path.

use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::app::App;
use crate::tui::commands::CommandSpec;

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

/// A single entry in the combined command-menu popup — either a built-in
/// command or a Goal-169 skill-backed command.
enum MenuEntry<'a> {
    Builtin(&'a crate::tui::commands::CommandSpec),
    Skill(&'a crate::tui::skill_commands::SkillCommand),
}

impl<'a> MenuEntry<'a> {
    fn name(&self) -> &str {
        match self {
            MenuEntry::Builtin(s) => s.name,
            MenuEntry::Skill(s) => &s.name,
        }
    }
    fn summary(&self) -> &str {
        match self {
            MenuEntry::Builtin(s) => s.summary,
            MenuEntry::Skill(s) => &s.description,
        }
    }
    fn is_skill(&self) -> bool {
        matches!(self, MenuEntry::Skill(_))
    }
}

/// Render the popup. No-op when the input mode is not Command, the
/// buffer is empty, or no matches are available.
///
/// Goal-169: the popup now shows both built-in and skill-backed commands
/// (skills are suffixed with `[skill]` in a dim colour so the user can
/// distinguish them at a glance).
pub fn render(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.prompt.mode != crate::tui::app::InputMode::Command {
        return;
    }
    let builtin_matches = app.commands.search(&app.prompt.buffer);
    let skill_matches = app.commands.search_skills(&app.prompt.buffer);

    // Combine: built-ins first, then skills.
    let mut combined: Vec<MenuEntry<'_>> = builtin_matches
        .iter()
        .map(|s| MenuEntry::Builtin(s))
        .chain(skill_matches.iter().map(|s| MenuEntry::Skill(s)))
        .collect();
    combined.truncate(MAX_VISIBLE);

    if combined.is_empty() {
        return;
    }
    let Some(area) = popup_rect(input_area, combined.len(), frame.area()) else {
        return;
    };
    frame.render_widget(Clear, area);

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let summary_style = Style::default().fg(Color::DarkGray);
    let skill_badge_style = Style::default().fg(Color::Green);

    let lines: Vec<Line<'static>> = combined
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let style = if app.command_menu_selected == Some(i) {
                selected_style
            } else {
                normal_style
            };
            let mut spans = vec![
                Span::styled(format!(" /{:<10} ", entry.name().to_string()), style),
                Span::styled(entry.summary().to_string(), summary_style),
            ];
            if entry.is_skill() {
                spans.push(Span::styled(" [skill]".to_string(), skill_badge_style));
            }
            Line::from(spans)
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

/// Render the @file completion popup (Goal 158). No-op when not in
/// AtFile mode or when the suggestion list is empty.
pub fn render_atfile(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.prompt.mode != crate::tui::app::InputMode::AtFile {
        return;
    }
    let suggestions = &app.atfile_suggestions;
    if suggestions.is_empty() {
        return;
    }
    let visible = &suggestions[..suggestions.len().min(MAX_VISIBLE)];
    let Some(area) = popup_rect(input_area, visible.len(), frame.area()) else {
        return;
    };
    frame.render_widget(Clear, area);

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);

    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let style = if app.atfile_selected == Some(i) {
                selected_style
            } else {
                normal_style
            };
            Line::from(Span::styled(format!(" {} ", path), style))
        })
        .collect();

    let title = format!(" @files  query: {:?} ", app.atfile_query);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

/// Render the Ctrl+R history-search popup (Goal 160). No-op when not in
/// HistorySearch mode or when there are no matches to show.
pub fn render_history_search(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.prompt.mode != crate::tui::app::InputMode::HistorySearch {
        return;
    }
    // Always show the popup (even when matches is empty) so the user
    // can see the search box with the current query.
    let match_count = app.hsearch_matches.len();
    let visible_count = match_count.clamp(1, MAX_VISIBLE);
    let Some(area) = popup_rect(input_area, visible_count, frame.area()) else {
        return;
    };
    frame.render_widget(Clear, area);

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let empty_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    let lines: Vec<Line<'static>> = if app.hsearch_matches.is_empty() {
        vec![Line::from(Span::styled(" (no matches) ", empty_style))]
    } else {
        let history = &app.prompt.history;
        app.hsearch_matches
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(i, &hist_idx)| {
                let entry = history.get(hist_idx).map(String::as_str).unwrap_or("");
                // Truncate long entries to 60 chars.
                let display = if entry.len() > 60 {
                    format!(" {}… ", &entry[..57])
                } else {
                    format!(" {} ", entry)
                };
                let style = if i == app.hsearch_selected {
                    selected_style
                } else {
                    normal_style
                };
                Line::from(Span::styled(display, style))
            })
            .collect()
    };

    let title = format!(" 🔍 {} ", app.hsearch_query);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightGreen))
        .title(title)
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

// ── Goal-161: Permission Request Modal ───────────────────────────────────────

/// Render the permission-request modal when a tool is waiting for user
/// approval. Displayed as a centred overlay with the tool name, an
/// abbreviated argument preview, and `[Y]es / [N]o` instructions.
pub fn render_permission_modal(frame: &mut Frame, app: &App) {
    let Some(ref perm) = app.pending_permission else {
        return;
    };

    // Centre a fixed-size box on screen.
    let area = frame.area();
    let modal_w = area.width.saturating_sub(8).min(72);
    let modal_h = 8u16;
    let x = (area.width.saturating_sub(modal_w)) / 2;
    let y = (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal_area);

    let header_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(Color::White);
    let hint_style = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);
    let muted_style = Style::default().fg(Color::DarkGray);

    let tool_line = Line::from(vec![
        Span::styled(" Tool: ", header_style),
        Span::styled(perm.tool_name.clone(), body_style),
    ]);

    let args_preview = if perm.args_preview.is_empty() {
        "(no arguments)".to_string()
    } else {
        perm.args_preview.clone()
    };
    let args_short: String = args_preview.chars().take(60).collect();
    let args_truncated = if args_preview.chars().count() > 60 {
        format!("{args_short}…")
    } else {
        args_short
    };
    let args_line = Line::from(vec![
        Span::styled(" Args: ", muted_style),
        Span::styled(args_truncated, body_style),
    ]);

    let sep_line = Line::from(Span::styled("─".repeat(modal_w as usize - 2), muted_style));

    let hint_line = Line::from(vec![
        Span::styled("  [Y]", hint_style),
        Span::styled("/", muted_style),
        Span::styled("[Enter]", hint_style),
        Span::styled(" Allow  ", body_style),
        Span::styled(
            "[N]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("/", muted_style),
        Span::styled("[Esc]", muted_style),
        Span::styled(" Deny  ", body_style),
        Span::styled(
            "[A]",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Allow All", body_style),
    ]);

    let lines = vec![
        Line::from(""),
        tool_line,
        args_line,
        Line::from(""),
        sep_line,
        hint_line,
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " ⚠ Permission Request ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Black));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, modal_area);
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, AppScreen, InputMode};
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
        let r = crate::tui::commands::CommandRegistry::default_set();
        // "he" → only /help; should complete to "help".
        let matches = r.search("he");
        let target = tab_completion_target("he", &matches);
        assert_eq!(target.as_deref(), Some("help"));
    }

    #[test]
    fn tab_extends_to_common_prefix_with_multiple_matches() {
        let r = crate::tui::commands::CommandRegistry::default_set();
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
        let r = crate::tui::commands::CommandRegistry::default_set();
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
            crate::tui::app::TranscriptBlock::System { text } => {
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

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
use crate::commands::{CommandRegistry, CommandSpec};
use crate::skill_commands::SkillCommand;

/// Maximum number of candidate rows to display in the popup.
pub const MAX_VISIBLE: usize = 8;

/// A single entry in the combined command-menu popup — either a built-in
/// command or a Goal-169/322 skill-backed command.
///
/// Public (Goal-322) so both the renderer and
/// [`crate::app::App::handle_command_menu_key`] share the same
/// source-of-truth for ordering and length.
pub enum MenuEntry<'a> {
    Builtin(&'a CommandSpec),
    Skill(&'a SkillCommand),
}

impl<'a> MenuEntry<'a> {
    pub fn name(&self) -> &str {
        match self {
            MenuEntry::Builtin(s) => s.name,
            MenuEntry::Skill(s) => &s.name,
        }
    }
    pub fn summary(&self) -> &str {
        match self {
            MenuEntry::Builtin(s) => s.summary,
            MenuEntry::Skill(s) => &s.description,
        }
    }
    pub fn is_skill(&self) -> bool {
        matches!(self, MenuEntry::Skill(_))
    }
    /// The argument_hint (e.g. `<file>`) for skill entries; empty for
    /// built-ins and skills without a hint.
    pub fn argument_hint(&self) -> &str {
        match self {
            MenuEntry::Skill(s) => &s.argument_hint,
            MenuEntry::Builtin(_) => "",
        }
    }
}

/// Build the combined (built-ins first, then skills) command menu entry
/// list, truncated to [`MAX_VISIBLE`].  Returns entries whose name or
/// alias starts with `buffer` (with the leading `/` stripped).
///
/// This is the single source of truth used by both the renderer and the
/// keyboard handler.
pub fn command_menu_entries<'a>(registry: &'a CommandRegistry, buffer: &str) -> Vec<MenuEntry<'a>> {
    let builtin = registry.search(buffer);
    let skills = registry.search_skills(buffer);
    let mut combined: Vec<MenuEntry<'_>> = builtin
        .iter()
        .map(|s| MenuEntry::Builtin(s))
        .chain(skills.iter().map(|s| MenuEntry::Skill(s)))
        .collect();
    combined.truncate(MAX_VISIBLE);
    combined
}

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

/// Return the canonical name to autocomplete to from a list of name
/// strings (works across both built-in and skill entries).
///
/// Algorithm: zero matches yields `None`; exactly one match yields
/// `Some(name)`; multiple matches yield the longest common prefix of
/// all names, but only if it strictly extends the buffer (otherwise
/// `None`, leaving the buffer alone).
pub fn tab_complete_names(buffer: &str, names: &[&str]) -> Option<String> {
    let buf = buffer.trim_start_matches('/');
    match names.len() {
        0 => None,
        1 => Some(names[0].to_string()),
        _ => {
            let mut common: &str = names[0];
            for name in &names[1..] {
                common = longest_common_prefix(common, name);
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
///
/// Goal-169: the popup now shows both built-in and skill-backed commands
/// (skills are suffixed with `[skill]` in a dim colour so the user can
/// distinguish them at a glance).
/// Goal-322: skill rows with a non-empty `argument_hint` show the hint
/// in dim colour.
pub fn render(frame: &mut Frame, input_area: Rect, app: &App) {
    if app.prompt.mode != crate::app::InputMode::Command {
        return;
    }

    let combined = command_menu_entries(&app.commands, &app.prompt.buffer);

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
    let hint_style = Style::default().fg(Color::DarkGray);

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
            let hint = entry.argument_hint();
            if !hint.is_empty() {
                spans.push(Span::styled(format!(" {hint}"), hint_style));
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
    if app.prompt.mode != crate::app::InputMode::AtFile {
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
    if app.prompt.mode != crate::app::InputMode::HistorySearch {
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

// ── Bottom-panel API (replaces the overlay popups) ───────────────────────────

/// Compute the height that the bottom panel slot needs in the Layout.
///
/// The panel slot lives **below** the input box.  When a slash-command,
/// @file-completion, or history-search mode is active, this returns the
/// number of rows required; otherwise returns 0 so the slot collapses.
pub fn panel_height(app: &App) -> u16 {
    use crate::app::InputMode;
    match app.prompt.mode {
        InputMode::Command => {
            let n = app.commands.search(&app.prompt.buffer).len()
                + app.commands.search_skills(&app.prompt.buffer).len();
            let visible = n.min(MAX_VISIBLE);
            if visible == 0 {
                0
            } else {
                visible as u16 + 2
            } // 2 = borders
        }
        InputMode::AtFile => {
            let n = app.atfile_suggestions.len().min(MAX_VISIBLE);
            if n == 0 {
                0
            } else {
                n as u16 + 2
            }
        }
        InputMode::HistorySearch => {
            // Always show at least one row (the "no matches" placeholder).
            app.hsearch_matches.len().clamp(1, MAX_VISIBLE) as u16 + 2
        }
        InputMode::CommandInteract => {
            // Height is driven by the panel state; cap at MAX_VISIBLE + 2 borders.
            app.active_command_panel
                .as_ref()
                .map(|p| p.height.min(MAX_VISIBLE as u16 + 2))
                .unwrap_or(0)
        }
        _ => 0,
    }
}

/// Render the active interactive panel into the slot below the input box.
///
/// `area` is provided by the Layout (`Constraint::Length(panel_height(app))`).
/// When no panel is active the constraint collapses to 0 and this is a no-op.
pub fn render_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    use crate::app::InputMode;
    match app.prompt.mode {
        InputMode::Command => render_command_panel(frame, area, app),
        InputMode::AtFile => render_atfile_panel(frame, area, app),
        InputMode::HistorySearch => render_history_panel(frame, area, app),
        InputMode::CommandInteract => render_command_interact_panel(frame, area, app),
        _ => {}
    }
}

fn render_command_panel(frame: &mut Frame, area: Rect, app: &App) {
    let combined = command_menu_entries(&app.commands, &app.prompt.buffer);

    if combined.is_empty() {
        return;
    }

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let summary_style = Style::default().fg(Color::DarkGray);
    let skill_badge_style = Style::default().fg(Color::Green);
    let hint_style = Style::default().fg(Color::DarkGray);

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
            let hint = entry.argument_hint();
            if !hint.is_empty() {
                spans.push(Span::styled(format!(" {hint}"), hint_style));
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Commands ")
        .style(Style::default().bg(Color::Black));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_atfile_panel(frame: &mut Frame, area: Rect, app: &App) {
    let suggestions = &app.atfile_suggestions;
    if suggestions.is_empty() {
        return;
    }
    let visible = &suggestions[..suggestions.len().min(MAX_VISIBLE)];

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
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_history_panel(frame: &mut Frame, area: Rect, app: &App) {
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
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_command_interact_panel(frame: &mut Frame, area: Rect, app: &App) {
    let Some(panel) = &app.active_command_panel else {
        return;
    };

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Rgb(205, 100, 50))
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let hint_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    // Reserve the last row for the hint when present.
    let content_rows = if panel.hint.is_some() {
        area.height.saturating_sub(3) as usize // 2 borders + 1 hint
    } else {
        area.height.saturating_sub(2) as usize // 2 borders
    };

    // The highlight bar tracks the selected *item*, but `lines` may begin
    // with non-selectable rows (header + spacer). Map the item index to its
    // line index via `list_offset` so the bar lands on the same row as the
    // item's `▶` marker.
    let highlight_line = panel.selected.map(|sel| sel + panel.list_offset);
    let mut lines: Vec<Line<'static>> = panel
        .lines
        .iter()
        .take(content_rows)
        .enumerate()
        .map(|(i, text)| {
            let style = if highlight_line == Some(i) {
                selected_style
            } else {
                normal_style
            };
            Line::from(Span::styled(format!(" {text} "), style))
        })
        .collect();

    if let Some(hint) = &panel.hint {
        lines.push(Line::from(Span::styled(format!(" {hint} "), hint_style)));
    }

    let title = format!(" /{} ", panel.command_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(205, 100, 50)))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(205, 100, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Black));

    frame.render_widget(Paragraph::new(lines).block(block), area);
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

    // ── Goal-322: command_menu_entries integration ─────────────────────

    #[test]
    fn command_menu_entries_includes_skills() {
        let mut r = crate::commands::CommandRegistry::default_set();
        let skill = crate::skill_commands::SkillCommand {
            name: "refactor".to_string(),
            description: "Refactor code".to_string(),
            aliases: vec!["rf".to_string()],
            argument_hint: "<file>".to_string(),
            allowed_tools: None,
            prompt_template: "Refactor $ARGUMENTS".to_string(),
            source_path: std::path::PathBuf::from("/fake/refactor.md"),
        };
        r = r.with_skill_commands(vec![skill]);
        // Use prefix "re" so built-in matches are minimal and skill shows up.
        let entries = command_menu_entries(&r, "re");
        assert!(!entries.is_empty());
        let has_skill = entries.iter().any(|e| e.name() == "refactor");
        assert!(has_skill);
    }

    #[test]
    fn command_menu_entries_builtins_come_before_skills() {
        let mut r = crate::commands::CommandRegistry::default_set();
        let skill = crate::skill_commands::SkillCommand {
            name: "resume-train".to_string(),
            description: "A skill".to_string(),
            aliases: vec![],
            argument_hint: "".to_string(),
            allowed_tools: None,
            prompt_template: "".to_string(),
            source_path: std::path::PathBuf::from("/fake/resume-train.md"),
        };
        r = r.with_skill_commands(vec![skill]);
        // "res" matches built-in "resume" and skill "resume-train".
        let entries = command_menu_entries(&r, "res");
        // "resume" (built-in) should appear before "resume-train" (skill).
        let resume_pos = entries.iter().position(|e| e.name() == "resume");
        let skill_pos = entries.iter().position(|e| e.name() == "resume-train");
        assert!(resume_pos < skill_pos);
    }

    #[test]
    fn tab_complete_names_works_with_skills() {
        let names = vec!["refactor", "review"];
        // "re" has common prefix "re" which is the same as input, so None.
        assert_eq!(tab_complete_names("re", &names), None);
        // "ref" has one match.
        let names = vec!["refactor"];
        assert_eq!(
            tab_complete_names("ref", &names),
            Some("refactor".to_string())
        );
        // Empty returns None.
        assert_eq!(tab_complete_names("zzz", &[] as &[&str]), None);
    }

    #[test]
    fn skill_row_with_argument_hint_renders_hint() {
        let mut app = fresh_command_app();
        let mut r = crate::commands::CommandRegistry::default_set();
        let skill = crate::skill_commands::SkillCommand {
            name: "refactor".to_string(),
            description: "Refactor code".to_string(),
            aliases: vec![],
            argument_hint: "<file>".to_string(),
            allowed_tools: None,
            prompt_template: "".to_string(),
            source_path: std::path::PathBuf::from("/fake/refactor.md"),
        };
        r = r.with_skill_commands(vec![skill]);
        app.commands = r;
        app.prompt.buffer = "ref".into();
        let entries = command_menu_entries(&app.commands, &app.prompt.buffer);
        // The skill entry should have the argument_hint.
        let skill_entry = entries.iter().find(|e| e.name() == "refactor").unwrap();
        assert_eq!(skill_entry.argument_hint(), "<file>");
        assert!(skill_entry.is_skill());
    }

    #[test]
    fn menu_entry_builtin_has_empty_argument_hint() {
        let r = crate::commands::CommandRegistry::default_set();
        let entries = command_menu_entries(&r, "help");
        let help_entry = entries.iter().find(|e| e.name() == "help").unwrap();
        assert!(!help_entry.is_skill());
        assert!(help_entry.argument_hint().is_empty());
    }

    #[test]
    fn summary_returns_builtin_summary() {
        // kills summary -> "" / "xyzzy" (44) for the Builtin arm
        use crate::commands::{CommandHandler, CommandOutcome, CommandSpec};
        let spec = CommandSpec {
            name: "x",
            aliases: &[],
            summary: "sum-desc",
            usage: "",
            handler: CommandHandler::Sync(|_, _| CommandOutcome::Done),
        };
        let entry = MenuEntry::Builtin(&spec);
        assert_eq!(entry.summary(), "sum-desc");
    }

    #[test]
    fn summary_returns_skill_description() {
        // kills summary -> "" / "xyzzy" (44) for the Skill arm
        use crate::skill_commands::SkillCommand;
        let skill = SkillCommand {
            name: "x".into(),
            description: "skill-desc".into(),
            aliases: vec![],
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "".into(),
            source_path: std::path::PathBuf::from("/fake/x.md"),
        };
        let entry = MenuEntry::Skill(&skill);
        assert_eq!(entry.summary(), "skill-desc");
    }

    #[test]
    fn tab_completion_target_single_exact_match_returns_some() {
        // kills delete match arm 1 (102): with buf==name, the `1 =>` arm
        // returns Some(name) while the `_ =>` fallback returns None
        // (common.len() > buf.len() is false).
        use crate::commands::{CommandHandler, CommandOutcome, CommandSpec};
        let spec = CommandSpec {
            name: "help",
            aliases: &[],
            summary: "",
            usage: "",
            handler: CommandHandler::Sync(|_, _| CommandOutcome::Done),
        };
        let matches: Vec<&CommandSpec> = vec![&spec];
        assert_eq!(
            tab_completion_target("help", &matches),
            Some("help".to_string())
        );
    }

    #[test]
    fn tab_completion_target_multi_common_extends_buffer() {
        // kills `>`->`<` (111): common "hel" (3) > buf "he" (2) -> Some;
        // mutant `<`: 3 < 2 false -> None.
        use crate::commands::{CommandHandler, CommandOutcome, CommandSpec};
        let s1 = CommandSpec {
            name: "help",
            aliases: &[],
            summary: "",
            usage: "",
            handler: CommandHandler::Sync(|_, _| CommandOutcome::Done),
        };
        let s2 = CommandSpec {
            name: "hello",
            aliases: &[],
            summary: "",
            usage: "",
            handler: CommandHandler::Sync(|_, _| CommandOutcome::Done),
        };
        let matches: Vec<&CommandSpec> = vec![&s1, &s2];
        assert_eq!(
            tab_completion_target("he", &matches),
            Some("hel".to_string())
        );
    }

    #[test]
    fn tab_complete_names_single_exact_match_returns_some() {
        // kills delete match arm 1 (131): buf==name -> `1 =>` Some; `_ =>` None.
        let names = vec!["refactor"];
        assert_eq!(
            tab_complete_names("refactor", &names),
            Some("refactor".to_string())
        );
    }

    #[test]
    fn tab_complete_names_multi_common_extends_buffer() {
        // kills `>`->`<` (140): common "abc" (3) > buf "ab" (2) -> Some;
        // mutant `<`: 3 < 2 false -> None.
        let names = vec!["abcdef", "abcxyz"];
        assert_eq!(tab_complete_names("ab", &names), Some("abc".to_string()));
    }

    #[test]
    fn popup_rect_some_when_fits() {
        // kills -> None (154) and `-`->`+` in Rect y (165:25): orig y = 10-5 = 5.
        let input = Rect::new(0, 10, 40, 3);
        let frame = Rect::new(0, 0, 80, 24);
        let r = popup_rect(input, 3, frame).expect("should fit");
        assert_eq!(r.y, 5); // 10 - (3+2)
        assert_eq!(r.height, 5); // 3 rows + 2 borders
    }

    #[test]
    fn popup_rect_boundary_y_equals_popup_h_still_fits() {
        // kills `<`->`==`/`<=` (159:21): input_area.y == popup_h (5).
        // orig `<`: 5<5 false -> falls through -> Some; mutant `==`/`<=`: true -> None.
        let input = Rect::new(0, 5, 40, 3);
        let frame = Rect::new(0, 0, 80, 24);
        assert!(popup_rect(input, 3, frame).is_some());
    }

    #[test]
    fn popup_rect_none_when_frame_overlaps_popup() {
        // kills `-`->`+` in the if condition (159:62):
        // orig: frame.y(6) > input.y - popup_h(10-5=5) -> 6>5 true -> None.
        // mutant `+`: 6 > 10+5=15 false -> Some.
        let input = Rect::new(0, 10, 40, 3);
        let frame = Rect::new(0, 6, 80, 24);
        assert!(popup_rect(input, 3, frame).is_none());
    }

    #[test]
    fn popup_rect_none_when_no_candidates() {
        let input = Rect::new(0, 10, 40, 3);
        let frame = Rect::new(0, 0, 80, 24);
        assert!(popup_rect(input, 0, frame).is_none());
    }

    // ── panel_height (pure) ──────────────────────────────────────────────

    fn app_mode(mode: InputMode) -> App {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = mode;
        app
    }

    #[test]
    fn panel_height_command_mode_with_skill() {
        // kills delete Command arm (351), `+`->`-`/`*` in builtin+skills
        // sum (353), and `+`->`-` in visible+2 (358).
        // buffer "re" -> builtin /resume (1) + skill refactor (1) -> n=2
        // -> visible 2 -> height 4.
        use crate::skill_commands::SkillCommand;
        let skill = SkillCommand {
            name: "refactor".into(),
            description: "Refactor".into(),
            aliases: vec![],
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "".into(),
            source_path: std::path::PathBuf::from("/fake/refactor.md"),
        };
        let r = crate::commands::CommandRegistry::default_set().with_skill_commands(vec![skill]);
        let mut app = app_mode(InputMode::Command);
        app.commands = r;
        app.prompt.buffer = "re".into();
        // Sanity: ensure both contribute (n=2, not 1 or 0).
        assert_eq!(
            app.commands.search("re").len() + app.commands.search_skills("re").len(),
            2
        );
        assert_eq!(panel_height(&app), 4);
    }

    #[test]
    fn panel_height_atfile_mode() {
        // kills delete AtFile arm (361) and `+`->`-` in n+2 (366).
        let mut app = app_mode(InputMode::AtFile);
        app.atfile_suggestions = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(panel_height(&app), 5); // 3 + 2 borders
    }

    #[test]
    fn panel_height_atfile_mode_empty_is_zero() {
        let app = app_mode(InputMode::AtFile);
        assert_eq!(panel_height(&app), 0);
    }

    #[test]
    fn panel_height_history_search_mode() {
        // kills `+`->`-` in clamp(1,MAX)+2 (371).
        let mut app = app_mode(InputMode::HistorySearch);
        app.hsearch_matches = vec![0, 1];
        assert_eq!(panel_height(&app), 4); // 2 + 2 borders
    }

    #[test]
    fn panel_height_command_interact_capped() {
        // kills `+`->`-` in MAX_VISIBLE+2 cap (377): height 20 -> min(20,10)=10;
        // mutant `-`: min(20, 8-2=6) = 6.
        let mut app = app_mode(crate::app::InputMode::CommandInteract);
        let mut panel = crate::app::CommandPanelState::new("cmd", vec![]);
        panel.height = 20;
        app.active_command_panel = Some(panel);
        assert_eq!(panel_height(&app), 10);
    }

    #[test]
    fn panel_height_prompt_mode_zero() {
        let app = app_mode(InputMode::Prompt);
        assert_eq!(panel_height(&app), 0);
    }
}

#[cfg(test)]
mod render_debt_tests {
    use super::*;
    use crate::app::{App, AppScreen, InputMode, PendingPermission};
    use crate::skill_commands::SkillCommand;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn draw(width: u16, height: u16, f: impl FnOnce(&mut ratatui::Frame)) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("TestBackend infallible");
        term.draw(|fr| f(fr)).expect("draw infallible");
        term.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        let mut s = String::new();
        for x in 0..width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn any_row_contains(buf: &Buffer, needle: &str, width: u16, height: u16) -> bool {
        (0..height).any(|y| row_text(buf, y, width).contains(needle))
    }

    fn row_has_bg(buf: &Buffer, y: u16, width: u16, color: Color) -> bool {
        (0..width).any(|x| buf[(x, y)].style().bg == Some(color))
    }

    fn count_symbol(buf: &Buffer, y: u16, width: u16, sym: &str) -> usize {
        (0..width).filter(|&x| buf[(x, y)].symbol() == sym).count()
    }

    fn history_app(history: Vec<String>, matches: Vec<usize>, selected: usize) -> App {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = InputMode::HistorySearch;
        app.prompt.history = history;
        app.hsearch_matches = matches;
        app.hsearch_selected = selected;
        app.hsearch_query = "q".into();
        app
    }

    // ── render_history_search ────────────────────────────────────────────

    #[test]
    fn history_search_renders_in_mode() {
        // kills render_history_search -> () (284) and `!=`->`==` guard (284).
        let app = history_app(vec!["hello world".into()], vec![0], 0);
        let buf = draw(80, 24, |f| {
            render_history_search(f, Rect::new(0, 20, 80, 3), &app);
        });
        assert!(any_row_contains(&buf, "hello", 80, 24));
    }

    #[test]
    fn history_search_skips_when_not_in_mode() {
        // kills `!=`->`==` from the other side: not in mode -> no render.
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = InputMode::Prompt;
        app.prompt.history = vec!["hello".into()];
        app.hsearch_matches = vec![0];
        app.hsearch_query = "q".into();
        let buf = draw(80, 24, |f| {
            render_history_search(f, Rect::new(0, 20, 80, 3), &app);
        });
        assert!(!any_row_contains(&buf, "hello", 80, 24));
        assert!(!any_row_contains(&buf, "🔍", 80, 24));
    }

    #[test]
    fn history_search_highlights_selected_row() {
        // kills `==`->`!=` (321): selected=0 -> row 0 highlighted (LightGreen bg);
        // mutant highlights row 1 instead.
        let app = history_app(vec!["aaa".into(), "bbb".into()], vec![0, 1], 0);
        let buf = draw(80, 24, |f| {
            render_history_search(f, Rect::new(0, 20, 80, 3), &app);
        });
        // Popup at y=16..19; match 0 ("aaa") on row 17, match 1 ("bbb") on 18.
        assert!(row_has_bg(&buf, 17, 80, Color::LightGreen));
        assert!(!row_has_bg(&buf, 18, 80, Color::LightGreen));
    }

    // ── render_panel dispatch + private panels ───────────────────────────

    fn panel_area() -> Rect {
        Rect::new(0, 20, 80, 8)
    }

    #[test]
    fn render_panel_dispatches_command_mode() {
        // kills delete Command arm in render_panel (394).
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = InputMode::Command;
        app.prompt.buffer = "he".into();
        let buf = draw(80, 24, |f| render_panel(f, panel_area(), &app));
        assert!(any_row_contains(&buf, "help", 80, 24));
    }

    #[test]
    fn render_panel_dispatches_history_search_mode() {
        // kills delete HistorySearch arm (396) and render_history_panel -> () (486).
        let app = history_app(vec!["foo entry".into()], vec![0], 0);
        let buf = draw(80, 24, |f| render_panel(f, panel_area(), &app));
        assert!(any_row_contains(&buf, "foo entry", 80, 24));
    }

    #[test]
    fn render_command_panel_shows_skill_argument_hint() {
        // kills delete `!` in `if !hint.is_empty()` (435).
        let skill = SkillCommand {
            name: "refactor".into(),
            description: "Refactor".into(),
            aliases: vec![],
            argument_hint: "<file>".into(),
            allowed_tools: None,
            prompt_template: "".into(),
            source_path: std::path::PathBuf::from("/fake/refactor.md"),
        };
        let r = crate::commands::CommandRegistry::default_set().with_skill_commands(vec![skill]);
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.mode = InputMode::Command;
        app.commands = r;
        app.prompt.buffer = "ref".into();
        let buf = draw(80, 24, |f| render_panel(f, panel_area(), &app));
        assert!(any_row_contains(&buf, "<file>", 80, 24));
    }

    // ── render_permission_modal ──────────────────────────────────────────

    fn app_with_permission(tool: &str, args: &str) -> App {
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.pending_permission = Some(PendingPermission {
            tool_name: tool.into(),
            args_preview: args.into(),
            reply: tx,
        });
        app
    }

    #[test]
    fn permission_modal_renders_when_pending() {
        // kills render_permission_modal -> () (595).
        let app = app_with_permission("MyTool", "some args");
        let buf = draw(80, 24, |f| render_permission_modal(f, &app));
        assert!(any_row_contains(&buf, "MyTool", 80, 24));
        assert!(any_row_contains(&buf, "Permission", 80, 24));
    }

    #[test]
    fn permission_modal_centered_x_uses_division() {
        // kills `/`->`%` (603): width 80, modal_w 72 -> x = (80-72)/2 = 4.
        // orig corner "┌" at (4, 8); mutant `%`: x = 8 % 2 = 0 -> corner at (0, 8).
        let app = app_with_permission("T", "a");
        let buf = draw(80, 24, |f| render_permission_modal(f, &app));
        assert_eq!(buf[(4, 8)].symbol(), "┌");
    }

    #[test]
    fn permission_modal_separator_length_uses_subtraction() {
        // kills `-`->`/` (640): sep = "─".repeat(modal_w - 2) = 70;
        // mutant `/`: "─".repeat(modal_w / 2) = 36.
        let app = app_with_permission("T", "a");
        let buf = draw(80, 24, |f| render_permission_modal(f, &app));
        // Modal y=8; sep line is the 5th content line -> row 13.
        assert_eq!(count_symbol(&buf, 13, 80, "─"), 70);
    }
}

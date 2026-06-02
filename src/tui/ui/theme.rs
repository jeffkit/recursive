//! Named colour palettes for the TUI.
//!
//! All colour constants used across the TUI rendering modules come from a
//! [`Theme`] struct that lives in [`AppState`].  Switching themes at runtime
//! requires only updating `app.state.theme` — no re-compile needed.
//!
//! Built-in themes: [`DARK`] (default), [`LIGHT`], [`SOLARIZED`].

use ratatui::style::Color;

// ── Theme struct ──────────────────────────────────────────────────────────

/// A named colour palette for the TUI.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    // Status bar
    pub status_bg: Color,
    pub status_fg: Color,
    pub status_mode_fg: Color,
    pub status_cost_fg: Color,
    pub status_dim_fg: Color,
    // Input box
    pub input_border: Color,
    pub input_prompt_fg: Color,
    pub input_bash_fg: Color,
    pub input_note_fg: Color,
    pub input_command_fg: Color,
    pub input_atfile_fg: Color,
    // Transcript
    pub user_bar: Color,
    pub assistant_bar: Color,
    pub system_bar: Color,
    pub tool_call_icon: Color,
    pub tool_ok_fg: Color,
    pub tool_err_fg: Color,
    pub code_fg: Color,
    pub diff_add: Color,
    pub diff_del: Color,
}

// ── Built-in palettes ─────────────────────────────────────────────────────

pub const DARK: Theme = Theme {
    name: "dark",
    status_bg: Color::DarkGray,
    status_fg: Color::White,
    status_mode_fg: Color::Green,
    status_cost_fg: Color::Cyan,
    status_dim_fg: Color::DarkGray,
    input_border: Color::DarkGray,
    input_prompt_fg: Color::Cyan,
    input_bash_fg: Color::Yellow,
    input_note_fg: Color::Green,
    input_command_fg: Color::Magenta,
    input_atfile_fg: Color::Cyan,
    user_bar: Color::Blue,
    assistant_bar: Color::Green,
    system_bar: Color::DarkGray,
    tool_call_icon: Color::Yellow,
    tool_ok_fg: Color::Green,
    tool_err_fg: Color::Red,
    code_fg: Color::Cyan,
    diff_add: Color::Green,
    diff_del: Color::Red,
};

pub const LIGHT: Theme = Theme {
    name: "light",
    status_bg: Color::Gray,
    status_fg: Color::Black,
    status_mode_fg: Color::Blue,
    status_cost_fg: Color::DarkGray,
    status_dim_fg: Color::Gray,
    input_border: Color::Gray,
    input_prompt_fg: Color::Blue,
    input_bash_fg: Color::Rgb(180, 100, 0),
    input_note_fg: Color::Rgb(0, 120, 0),
    input_command_fg: Color::Rgb(100, 0, 150),
    input_atfile_fg: Color::Blue,
    user_bar: Color::Blue,
    assistant_bar: Color::Rgb(0, 130, 0),
    system_bar: Color::Gray,
    tool_call_icon: Color::Rgb(180, 100, 0),
    tool_ok_fg: Color::Rgb(0, 130, 0),
    tool_err_fg: Color::Red,
    code_fg: Color::Blue,
    diff_add: Color::Rgb(0, 130, 0),
    diff_del: Color::Red,
};

pub const SOLARIZED: Theme = Theme {
    name: "solarized",
    status_bg: Color::Rgb(7, 54, 66),
    status_fg: Color::Rgb(131, 148, 150),
    status_mode_fg: Color::Rgb(38, 139, 210),
    status_cost_fg: Color::Rgb(42, 161, 152),
    status_dim_fg: Color::Rgb(88, 110, 117),
    input_border: Color::Rgb(88, 110, 117),
    input_prompt_fg: Color::Rgb(42, 161, 152),
    input_bash_fg: Color::Rgb(181, 137, 0),
    input_note_fg: Color::Rgb(133, 153, 0),
    input_command_fg: Color::Rgb(108, 113, 196),
    input_atfile_fg: Color::Rgb(42, 161, 152),
    user_bar: Color::Rgb(38, 139, 210),
    assistant_bar: Color::Rgb(133, 153, 0),
    system_bar: Color::Rgb(88, 110, 117),
    tool_call_icon: Color::Rgb(181, 137, 0),
    tool_ok_fg: Color::Rgb(133, 153, 0),
    tool_err_fg: Color::Rgb(220, 50, 47),
    code_fg: Color::Rgb(42, 161, 152),
    diff_add: Color::Rgb(133, 153, 0),
    diff_del: Color::Rgb(220, 50, 47),
};

pub const ALL_THEMES: &[&Theme] = &[&DARK, &LIGHT, &SOLARIZED];

/// Look up a theme by name; falls back to [`DARK`] for unknown names.
pub fn find_theme(name: &str) -> &'static Theme {
    ALL_THEMES
        .iter()
        .copied()
        .find(|t| t.name == name)
        .unwrap_or(&DARK)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_has_expected_name() {
        assert_eq!(DARK.name, "dark");
    }

    #[test]
    fn find_theme_returns_dark_for_unknown() {
        assert_eq!(find_theme("nope").name, "dark");
        assert_eq!(find_theme("").name, "dark");
    }

    #[test]
    fn all_themes_have_unique_names() {
        let names: Vec<&str> = ALL_THEMES.iter().map(|t| t.name).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(names.len(), unique.len(), "duplicate theme names");
    }

    #[test]
    fn find_theme_returns_correct_theme() {
        assert_eq!(find_theme("light").name, "light");
        assert_eq!(find_theme("solarized").name, "solarized");
        assert_eq!(find_theme("dark").name, "dark");
    }
}

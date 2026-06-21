//! Bottom status-bar renderer.
//!
//! The status bar is the dense one-liner that summarises the agent's
//! current connection, model, token usage, cost, turn counter and
//! per-turn elapsed time. It is rendered below the transcript on the
//! chat screen.
//!
//! Format (segments separated by ` │ `):
//!
//! ```text
//!  local │ deepseek-chat │ ↑1.2k ↓342  $0.0024 │ turn 3 │ ⏱ 2.3s
//! ```
//!
//! Cost is omitted when the model has no pricing entry in `providers.toml`;
//! elapsed time is shown only while a turn is running.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::app::App;

/// Build the styled status-bar paragraph for the given [`App`].
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let line = build_line(app);
    let paragraph =
        Paragraph::new(line).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

/// Public for tests — produces the styled `Line` without rendering.
pub fn build_line(app: &App) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    // [connection]
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        "local".to_string(),
        Style::default()
            .fg(Color::Green)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ));

    // [model]
    spans.push(separator());
    spans.push(Span::styled(
        app.model_name.clone(),
        Style::default().fg(Color::Cyan).bg(Color::DarkGray),
    ));

    // [version + workspace] — the identity info that used to live in the
    // startup banner header now lives here, in the always-visible status bar.
    spans.push(separator());
    spans.push(Span::styled(
        format!("v{}", env!("CARGO_PKG_VERSION")),
        Style::default().fg(Color::Gray).bg(Color::DarkGray),
    ));
    spans.push(separator());
    spans.push(Span::styled(
        abbreviate_workspace(&app.workspace_path),
        Style::default().fg(Color::Gray).bg(Color::DarkGray),
    ));

    // [tokens + cost]
    spans.push(separator());
    spans.push(Span::styled(
        format!(
            "↑{} ↓{}",
            human_count(app.usage.total_input),
            human_count(app.usage.total_output),
        ),
        Style::default().fg(Color::White).bg(Color::DarkGray),
    ));
    if let Some(cost) = crate::tui::app::estimate_cost(
        &app.model_name,
        app.usage.total_input,
        app.usage.total_output,
        app.usage.total_cache_hit,
        app.usage.total_cache_miss,
    ) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("${cost:.4}"),
            Style::default().fg(Color::Yellow).bg(Color::DarkGray),
        ));
    }

    // [cache hit rate] — most recent turn only, shown when there's cache data.
    // Uses the per-turn counters (not the session totals) because the
    // cumulative figure trends to ~100% as the cached prompt prefix is re-read
    // on every step, hiding the cold-start misses. The invariant
    // `hit + miss == total input tokens` holds for every provider (see
    // `TokenUsage` docs), so this denominator is the real prompt size.
    let turn_cache = app.usage.turn_cache_hit + app.usage.turn_cache_miss;
    if turn_cache > 0 {
        let pct = (app.usage.turn_cache_hit as f64 / turn_cache as f64) * 100.0;
        spans.push(separator());
        spans.push(Span::styled(
            format!("📦{:.0}%", pct),
            Style::default().fg(Color::Green).bg(Color::DarkGray),
        ));
    }

    // [turn]
    spans.push(separator());
    spans.push(Span::styled(
        format!("turn {}", app.turn_count),
        Style::default().fg(Color::White).bg(Color::DarkGray),
    ));

    // [plan mode indicators]
    if app.plan_awaiting_approval {
        spans.push(separator());
        spans.push(Span::styled(
            "plan: y/n".to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // [elapsed] — only while turn is running
    if let Some(started) = app.turn.started_at {
        let elapsed = started.elapsed().as_secs_f64();
        spans.push(separator());
        spans.push(Span::styled(
            format!("⏱ {:.1}s", elapsed),
            Style::default().fg(Color::Magenta).bg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}

/// Abbreviate an absolute workspace path for the status bar, replacing the
/// home-directory prefix with `~`.
fn abbreviate_workspace(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if !h.is_empty() && s.starts_with(&h) {
            return format!("~{}", &s[h.len()..]);
        }
    }
    s
}

fn separator() -> Span<'static> {
    Span::styled(
        " │ ".to_string(),
        Style::default().fg(Color::DarkGray).bg(Color::DarkGray),
    )
}

/// Format an integer compactly: 1234 → "1.2k", 1_500_000 → "1.5M".
fn human_count(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn human_count_formats_thresholds() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1234), "1.2k");
        assert_eq!(human_count(1_500_000), "1.5M");
    }

    #[test]
    fn status_bar_includes_model_and_tokens() {
        let mut app = App::new();
        app.model_name = "deepseek-chat".to_string();
        app.usage.total_input = 1234;
        app.usage.total_output = 342;
        let line = build_line(&app);
        let text = line_text(&line);
        assert!(text.contains("local"));
        assert!(text.contains("deepseek-chat"));
        assert!(text.contains("↑1.2k"));
        assert!(text.contains("↓342"));
        assert!(text.contains("turn"));
    }

    #[test]
    fn status_bar_includes_cost_for_known_model() {
        let mut app = App::new();
        app.model_name = "gpt-4o-mini".to_string();
        app.usage.total_input = 1000;
        app.usage.total_output = 1000;
        let text = line_text(&build_line(&app));
        assert!(text.contains("$"));
    }

    #[test]
    fn status_bar_omits_cost_for_unknown_model() {
        let mut app = App::new();
        app.model_name = "totally-bogus-model".to_string();
        app.usage.total_input = 1000;
        app.usage.total_output = 1000;
        let text = line_text(&build_line(&app));
        assert!(!text.contains("$"));
    }

    #[test]
    fn status_bar_shows_elapsed_only_when_turn_running() {
        let mut app = App::new();
        let no_turn = line_text(&build_line(&app));
        assert!(!no_turn.contains("⏱"));
        app.turn.start();
        let with_turn = line_text(&build_line(&app));
        assert!(with_turn.contains("⏱"));
    }

    #[test]
    fn status_bar_includes_version_and_workspace() {
        let mut app = App::new();
        app.workspace_path = std::path::PathBuf::from("/tmp/some-workspace");
        let text = line_text(&build_line(&app));
        // Version is the crate version, prefixed with `v`.
        assert!(
            text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "status bar should show version: {text:?}"
        );
        // Workspace tail should appear (path not under $HOME → shown verbatim).
        assert!(
            text.contains("some-workspace"),
            "status bar should show workspace: {text:?}"
        );
    }

    #[test]
    fn abbreviate_workspace_replaces_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            let p = home.join("projects/Recursive");
            let abbreviated = abbreviate_workspace(&p);
            assert!(
                abbreviated.starts_with("~/"),
                "expected ~-prefixed path, got {abbreviated:?}"
            );
            assert!(abbreviated.ends_with("projects/Recursive"));
        }
    }

    #[test]
    fn status_bar_shows_plan_awaiting_indicator() {
        let mut app = App::new();
        let no_plan = line_text(&build_line(&app));
        assert!(!no_plan.contains("plan:"));
        app.plan_awaiting_approval = true;
        let with_plan = line_text(&build_line(&app));
        assert!(with_plan.contains("plan: y/n"));
    }

    #[test]
    fn cache_hit_rate_uses_turn_cache_not_total_input() {
        // The denominator is hit + miss (the real prompt size), never the
        // bare `total_input`, which for Anthropic excludes cached tokens.
        let mut app = App::new();
        app.usage.total_input = 150; // "new" prompt tokens
        app.usage.turn_cache_hit = 900; // cached prefix tokens
        app.usage.turn_cache_miss = 150;
        let text = line_text(&build_line(&app));
        // Hit rate = 900 / (900 + 150) = 85.7% → "86%"
        assert!(
            text.contains("86%"),
            "expected ~86% cache rate, got: {text:?}"
        );
        assert!(
            !text.contains("600%"),
            "should not use total_input as denominator"
        );
    }

    #[test]
    fn cache_hit_rate_uses_current_turn_not_session_totals() {
        // Session totals would read ~99% (a long warm session), but the
        // current turn was a cold cache miss → the bar must show the turn.
        let mut app = App::new();
        app.usage.total_cache_hit = 99_000;
        app.usage.total_cache_miss = 1_000;
        app.usage.turn_cache_hit = 0;
        app.usage.turn_cache_miss = 500;
        let text = line_text(&build_line(&app));
        assert!(
            text.contains("📦0%"),
            "expected current-turn 0%, got: {text:?}"
        );
        assert!(
            !text.contains("99%"),
            "must not use session totals: {text:?}"
        );
    }

    #[test]
    fn cache_hit_rate_zero_when_no_cache_data() {
        let app = App::new();
        let text = line_text(&build_line(&app));
        // No cache data → no 📦 segment at all
        assert!(
            !text.contains("📦"),
            "got cache segment with no data: {text:?}"
        );
    }

    #[test]
    fn cache_hit_rate_shows_zero_pct_when_all_miss() {
        let mut app = App::new();
        app.usage.turn_cache_hit = 0;
        app.usage.turn_cache_miss = 500;
        let text = line_text(&build_line(&app));
        // turn_cache = 500 > 0 → should show "0%"
        assert!(text.contains("0%"), "expected 0% cache rate, got: {text:?}");
    }
}

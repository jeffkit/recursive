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

use crate::app::App;

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

    // [connection] — "local" once the runtime is ready, "offline" (red) when
    // the backend could not build a runtime, "starting…" before either signal
    // arrives. `offline_reason` is set by `UiEvent::RuntimeOffline` at init,
    // so the user sees the real state immediately instead of a stuck spinner.
    spans.push(Span::raw(" "));
    let (conn_label, conn_color) = if app.connected {
        ("local", Color::Green)
    } else if app.offline_reason.is_some() {
        ("offline", Color::Red)
    } else {
        ("starting\u{2026}", Color::Yellow)
    };
    spans.push(Span::styled(
        conn_label.to_string(),
        Style::default()
            .fg(conn_color)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ));

    // [model] — when offline, show "no provider" instead of the hardcoded
    // `deepseek-v4-flash` fallback that `detect_model_name` produces when
    // nothing is configured. Otherwise the user would see a real-looking
    // model name while the agent can't actually run.
    spans.push(separator());
    let model_label: String = if app.offline_reason.is_some() && !app.connected {
        "no provider".to_string()
    } else {
        app.model_name.clone()
    };
    let model_color = if app.offline_reason.is_some() && !app.connected {
        Color::Red
    } else {
        Color::Cyan
    };
    spans.push(Span::styled(
        model_label,
        Style::default().fg(model_color).bg(Color::DarkGray),
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
    if let Some(cost) = crate::app::estimate_cost(
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

    // [loop] — Goal-323: event-driven loop state indicator
    if let Some(ls) = &app.loop_state {
        spans.push(separator());
        let label = if ls.turns_run == 0 && ls.max_turns == 0 {
            "loop: idle".to_string()
        } else if ls.max_turns > 0 {
            format!("loop: turn {}/{}", ls.turns_run, ls.max_turns)
        } else {
            format!("loop: turn {}", ls.turns_run)
        };
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(Color::LightGreen)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
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

    /// Content of the span at `idx`, panicking if it's out of range. Used to
    /// assert on a specific status-bar slot (connection label, model label)
    /// rather than the joined line, which can contain overlapping substrings
    /// from the workspace path.
    fn span_content(line: &Line, idx: usize) -> String {
        line.spans
            .get(idx)
            .map(|s| s.content.as_ref().to_string())
            .unwrap_or_else(|| panic!("span {idx} missing; line has {} spans", line.spans.len()))
    }

    /// Foreground color of the span at `idx`, or `None` if unset. Used to pin
    /// the offline/online model-slot color so a mutant that flips the color
    /// condition (e.g. `&&`→`||` on the color branch) is caught, not just the
    /// label text.
    fn span_fg(line: &Line, idx: usize) -> Option<Color> {
        line.spans.get(idx).and_then(|s| s.style.fg)
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

        // Before runtime is ready: shows "starting…".
        let text = line_text(&build_line(&app));
        assert!(
            text.contains("starting"),
            "expected 'starting' before RuntimeReady; got: {text}"
        );
        assert!(text.contains("deepseek-chat"));

        // After RuntimeReady: shows "local".
        app.connected = true;
        let line = build_line(&app);
        let text = line_text(&line);
        assert!(
            text.contains("local"),
            "expected 'local' after RuntimeReady; got: {text}"
        );
        assert!(text.contains("deepseek-chat"));
        assert!(text.contains("↑1.2k"));
        assert!(text.contains("↓342"));
        assert!(text.contains("turn"));
    }

    #[test]
    fn status_bar_shows_offline_when_offline_reason_set() {
        // Reproduces the bug report: with no provider configured, the model
        // falls back to the hardcoded "deepseek-v4-flash" and the status bar
        // used to stay stuck at "starting…". With offline_reason set, it must
        // show "offline" and "no provider" instead.
        //
        // We assert on the specific connection/model spans rather than the
        // joined line text, because the workspace path printed later in the
        // bar can itself contain the substring "offline" (e.g. a worktree
        // named `tui-offline-status`), which would make a naive
        // `text.contains("offline")` pass for the wrong reason.
        let mut app = App::new();
        app.model_name = "deepseek-v4-flash".to_string();
        app.offline_reason = Some("No LLM provider configured.".to_string());

        let line = build_line(&app);
        // span[0] = leading space, span[1] = connection label,
        // span[2] = separator, span[3] = model label.
        let conn = span_content(&line, 1);
        let model = span_content(&line, 3);
        assert_eq!(conn, "offline", "connection label should be 'offline'");
        assert_eq!(model, "no provider", "model label should be 'no provider'");
        // The model slot must be red when offline — pins the color branch so
        // a `&&`→`||` or `delete !` mutant on the color condition is caught,
        // not just the label.
        assert_eq!(
            span_fg(&line, 3),
            Some(Color::Red),
            "model label should be red when offline"
        );
    }

    #[test]
    fn status_bar_offline_resolves_to_local_after_runtime_ready() {
        // RuntimeReady clears offline_reason and flips to "local". Pins the
        // recovery path so a mutant that leaves offline_reason sticky is
        // killed.
        let mut app = App::new();
        app.model_name = "deepseek-chat".to_string();
        app.offline_reason = Some("offline".to_string());
        assert_eq!(span_content(&build_line(&app), 1), "offline");

        app.handle_ui_event(crate::events::UiEvent::RuntimeReady);
        let line = build_line(&app);
        assert_eq!(
            span_content(&line, 1),
            "local",
            "connection label should be 'local' after RuntimeReady"
        );
        assert_eq!(
            span_content(&line, 3),
            "deepseek-chat",
            "model label should reflect the configured model after RuntimeReady"
        );
        // Online → cyan model slot. Pins the color branch's else arm.
        assert_eq!(
            span_fg(&line, 3),
            Some(Color::Cyan),
            "model label should be cyan when online"
        );
    }

    #[test]
    fn status_bar_starting_state_shows_model_in_cyan() {
        // Before any runtime signal (offline_reason None, connected false):
        // "starting…" connection, model shown in cyan (not red). Pins the
        // color condition's `&&` so `&&`→`||` (which would turn the starting
        // model slot red) is caught.
        let mut app = App::new();
        app.model_name = "deepseek-chat".to_string();
        // offline_reason None, connected false → starting state.
        let line = build_line(&app);
        assert_eq!(span_content(&line, 1), "starting\u{2026}");
        assert_eq!(span_content(&line, 3), "deepseek-chat");
        assert_eq!(
            span_fg(&line, 3),
            Some(Color::Cyan),
            "model label should be cyan in the starting state, not red"
        );
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
            // The home prefix is replaced with `~`; the remaining path keeps
            // the platform separator (`/` on Unix, `\` on Windows), so assert
            // a `~` prefix followed by EITHER separator rather than hardcoding
            // `~/` (which fails on Windows where `dirs::home_dir()` returns a
            // `C:\Users\...` path and `Path::display()` uses backslashes).
            assert!(
                abbreviated.starts_with('~'),
                "expected ~-prefixed path, got {abbreviated:?}"
            );
            let after_tilde = &abbreviated[1..];
            assert!(
                after_tilde.starts_with('/') || after_tilde.starts_with('\\'),
                "expected a path separator after ~, got {abbreviated:?}"
            );
            assert!(
                abbreviated.ends_with("projects/Recursive"),
                "expected trailing projects/Recursive, got {abbreviated:?}"
            );
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

    // ── Goal-323: loop state indicator + pre-existing status.rs coverage ──

    #[test]
    fn status_bar_shows_no_loop_when_inactive() {
        let app = App::new();
        let text = line_text(&build_line(&app));
        assert!(
            !text.contains("loop:"),
            "no loop segment when inactive: {text:?}"
        );
    }

    #[test]
    fn status_bar_shows_loop_idle() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 0,
            max_turns: 0,
        });
        let text = line_text(&build_line(&app));
        assert!(text.contains("loop: idle"), "got {text:?}");
    }

    #[test]
    fn status_bar_shows_loop_turn_with_max() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 1,
            max_turns: 5,
        });
        let text = line_text(&build_line(&app));
        assert!(text.contains("loop: turn 1/5"), "got {text:?}");
    }

    #[test]
    fn status_bar_shows_loop_turn_unlimited() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 3,
            max_turns: 0,
        });
        let text = line_text(&build_line(&app));
        assert!(text.contains("loop: turn 3"), "got {text:?}");
        assert!(
            !text.contains("loop: turn 3/"),
            "unlimited must not show /max: {text:?}"
        );
    }

    #[test]
    fn status_bar_loop_segment_uses_separator() {
        // The loop segment is preceded by a separator ("│"); if separator()
        // is replaced with Default::default() this fails.
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 0,
            max_turns: 0,
        });
        let text = line_text(&build_line(&app));
        assert!(
            text.contains('│'),
            "expected separator before loop segment: {text:?}"
        );
    }

    #[test]
    fn separator_contains_pipe() {
        let s = separator();
        assert!(s.content.contains('│'), "got {:?}", s.content);
    }

    #[test]
    fn human_count_k_and_m_boundaries() {
        // 1000 must format as "1.0k" (kills `< 1000` → `<= 1000` mutant).
        assert_eq!(human_count(1000), "1.0k");
        // 1_000_000 must format as "1.0M" (kills `< 1_000_000` → `<= 1_000_000`).
        assert_eq!(human_count(1_000_000), "1.0M");
    }

    #[test]
    fn render_draws_status_line_into_buffer() {
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.model_name = "deepseek-chat".into();
        app.connected = true;
        app.workspace_path = std::path::PathBuf::from("/tmp/ws");
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 1,
            max_turns: 5,
        });
        let backend = TestBackend::new(400, 1);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            render(f, f.area(), &app);
        })
        .expect("draw");
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("deepseek-chat"),
            "render wrote model: {text:?}"
        );
        assert!(
            text.contains("loop: turn 1/5"),
            "render wrote loop: {text:?}"
        );
    }
}

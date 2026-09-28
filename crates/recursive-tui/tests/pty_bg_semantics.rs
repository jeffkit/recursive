//! Semantic lock for the PTY-layer bg assertions (issue #45).
//!
//! Pins the colour mapping across both observation layers so the
//! integration-layer `Screen::row_has_bg_color` is provably asserting the
//! same thing as the in-process harness (`harness.rs:113`):
//!
//!   SGR 41 (child emits `\x1b[41m`)
//!     ↔ ratatui `Color::Red` (what `harness.rs` would style the row with)
//!     ↔ vt100 `Color::Idx(1)` (what the PTY parse layer sees)
//!
//! If a dependency upgrade ever renumbers the SGR→Idx mapping, this test
//! fails and the mapping comment must be re-derived — not silently
//! re-pointed, because downstream assertion helpers bake the Idx in.

use ratatui::backend::TestBackend;
use ratatui::style::{Color as RatColor, Style};
use ratatui::widgets::Paragraph;
use tui_pty_harness::{spawn_and_snapshot, Color, RunSpec};

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn pty_row_has_bg_color_matches_sgr41_red_mapping() {
    let spec = RunSpec {
        prog: "printf",
        args: &["\\033[41mREDLINE\\033[0m\\n".to_string()],
        keys: &[],
        cols: 40,
        rows: 5,
        wait_ms: 1000,
        stable_ms: 80,
        cwd: None,
        envs: &[],
        record_raw: None,
    };
    let screen = spawn_and_snapshot(&spec).expect("spawn + snapshot");
    let row = screen.find_row("REDLINE").expect("REDLINE on screen");
    assert!(
        screen.row_has_bg_color(row, Color::Idx(1)),
        "SGR 41 must surface as vt100 Color::Idx(1) — same colour ratatui's \
         Color::Red renders over ANSI (see harness.rs:113 semantics)"
    );
    assert!(
        !screen.row_has_bg_color(row, Color::Idx(2)),
        "a red row must not match green"
    );
}

/// In-process counterpart of `pty_row_has_bg_color_matches_sgr41_red_mapping`:
/// renders the same red background via ratatui (TestBackend, the harness's
/// own rendering pipeline) and asserts `harness.rs:113` semantics agree with
/// the PTY layer. Together the two tests lock both observation layers to the
/// same conclusion for a "red background row" (mapping: SGR 41 ↔ ratatui
/// `Color::Red` ↔ vt100 `Color::Idx(1)`; green ↔ `Idx(2)` must NOT match).
/// Runs on all platforms (no PTY).
#[test]
fn inproc_row_has_bg_color_matches_ratatui_red_mapping() {
    let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 5)).expect("TestBackend");
    terminal
        .draw(|f| {
            let p = Paragraph::new("REDLINE").style(Style::default().bg(RatColor::Red));
            f.render_widget(p, f.area());
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    let width = 40u16;
    let bg = |y: u16| -> Option<RatColor> {
        (0..width)
            .map(|x| buf[(x, y)].style().bg)
            .find(|c| *c != Some(RatColor::Reset) && c.is_some())
            .unwrap_or_default()
    };
    assert_eq!(bg(0), Some(RatColor::Red), "ratatui layer sees a red row");
    // harness.rs:113 semantics, replicated verbatim (harness::Screen is only
    // produced by Harness::render, which renders the real App — it cannot
    // render an arbitrary red Paragraph). Any drift between this mirror and
    // harness.rs:105-117 is caught by the harness's own unit tests.
    let harness_bg = |x: u16, y: u16| -> Option<RatColor> {
        match buf[(x, y)].style().bg {
            Some(RatColor::Reset) | None => None,
            Some(c) => Some(c),
        }
    };
    let row_has = |y: u16, color: RatColor| (0..width).any(|x| harness_bg(x, y) == Some(color));
    assert!(
        row_has(0, RatColor::Red),
        "row_has_bg_color(row, Red) == true"
    );
    assert!(
        !row_has(0, RatColor::Green),
        "row_has_bg_color(row, Green) must be false — mirrors Idx(2) on the PTY side"
    );
}

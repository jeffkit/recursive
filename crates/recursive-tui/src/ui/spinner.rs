//! Braille spinner used while a turn is in flight.
//!
//! Goal-144 introduces a single-line spinner rendered transiently
//! at the bottom of the transcript while [`crate::app::TurnState::running`]
//! is true. The animation frame is driven by a counter in
//! [`crate::app::App::spinner_frame`] that the main loop ticks every
//! draw cycle.

/// 10-frame braille spinner sequence (≈100ms per frame at 50ms ticks).
pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Look up the spinner glyph for a given monotonically-increasing
/// frame counter.
pub fn frame_char(index: usize) -> &'static str {
    FRAMES[index % FRAMES.len()]
}

/// Format the spinner one-liner: `<spinner> <verb> <elapsed>s`.
pub fn format_line(index: usize, verb: &str, elapsed_secs: f64) -> String {
    format!("{} {} {:.1}s", frame_char(index), verb, elapsed_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frame_advances_with_index() {
        assert_eq!(frame_char(0), "⠋");
        assert_eq!(frame_char(1), "⠙");
        assert_eq!(frame_char(9), "⠏");
        // wraps
        assert_eq!(frame_char(10), "⠋");
        assert_eq!(frame_char(20), "⠋");
    }

    #[test]
    fn format_line_includes_verb_and_elapsed() {
        let line = format_line(0, "Thinking", 2.34);
        assert!(line.contains("Thinking"));
        assert!(line.contains("2.3s"));
        assert!(line.starts_with('⠋'));
    }
}

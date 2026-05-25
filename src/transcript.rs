//! Persistent on-disk format for transcripts.
//!
//! A `TranscriptFile` is everything you need to inspect or replay a
//! past run: the list of messages exchanged, plus a small `meta`
//! block describing the run.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::message::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMeta {
    /// ISO-8601 timestamp when the run was saved.
    pub saved_at: String,
    /// Number of steps the agent loop executed.
    pub steps: usize,
    /// Optional human label (often the model name).
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptFile {
    pub meta: TranscriptMeta,
    pub messages: Vec<Message>,
}

impl TranscriptFile {
    pub fn new(messages: Vec<Message>, steps: usize, model: Option<String>) -> Self {
        let saved_at = chrono_lite_now();
        Self {
            meta: TranscriptMeta {
                saved_at,
                steps,
                model,
            },
            messages,
        }
    }

    /// Pretty-printed JSON. Stable enough to be diffed across runs.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    pub fn read_from(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Render the transcript as a human-readable string suitable for
    /// piping to a pager.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "=== transcript ({} messages) ===\n",
            self.messages.len()
        ));
        out.push_str(&format!(
            "saved_at: {}\nsteps: {}\nmodel: {}\n\n",
            self.meta.saved_at,
            self.meta.steps,
            self.meta.model.as_deref().unwrap_or("(unknown)"),
        ));
        for (i, msg) in self.messages.iter().enumerate() {
            out.push_str(&format!("--- [{}] {:?} ---\n", i, msg.role));
            if !msg.content.is_empty() {
                out.push_str(&msg.content);
                if !msg.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        out
    }

    /// Render the last `n` messages as a human-readable string.
    /// If `n` is less than the total, prepends a "skipped" notice.
    pub fn pretty_tail(&self, n: usize) -> String {
        let total = self.messages.len();
        // If n >= total, behave like pretty()
        if n >= total {
            return self.pretty();
        }

        let skipped = total - n;
        let mut out = String::new();
        out.push_str(&format!("...skipped {} earlier messages\n\n", skipped));
        out.push_str(&format!(
            "=== transcript ({} messages) ===\n",
            self.messages.len()
        ));
        out.push_str(&format!(
            "saved_at: {}\nsteps: {}\nmodel: {}\n\n",
            self.meta.saved_at,
            self.meta.steps,
            self.meta.model.as_deref().unwrap_or("(unknown)"),
        ));
        // Only render the last n messages, but preserve their original indices
        let start_idx = total - n;
        for (i, msg) in self.messages.iter().enumerate().skip(start_idx) {
            out.push_str(&format!("--- [{}] {:?} ---\n", i, msg.role));
            if !msg.content.is_empty() {
                out.push_str(&msg.content);
                if !msg.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        out
    }

    /// Render the first `n` messages as a human-readable string.
    /// If `n` is less than the total, appends a "skipped" notice.
    pub fn pretty_head(&self, n: usize) -> String {
        let total = self.messages.len();
        // If n >= total, behave like pretty()
        if n >= total {
            return self.pretty();
        }

        let skipped = total - n;
        let mut out = String::new();
        // Render the first n messages
        out.push_str(&format!(
            "=== transcript ({} messages) ===\n",
            self.messages.len()
        ));
        out.push_str(&format!(
            "saved_at: {}\nsteps: {}\nmodel: {}\n\n",
            self.meta.saved_at,
            self.meta.steps,
            self.meta.model.as_deref().unwrap_or("(unknown)"),
        ));
        // Only render the first n messages (indices 0 to n-1)
        for (i, msg) in self.messages.iter().enumerate().take(n) {
            out.push_str(&format!("--- [{}] {:?} ---\n", i, msg.role));
            if !msg.content.is_empty() {
                out.push_str(&msg.content);
                if !msg.content.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        // Add skipped notice at the end
        out.push_str(&format!("\n... skipping {} later messages\n", skipped));
        out
    }

    /// Return the first `n` messages (`None` if `n` exceeds the count).
    /// `n == 0` returns an empty slice, useful for "start fresh but
    /// preserve nothing" callers.
    pub fn take_first_n(&self, n: usize) -> Option<&[Message]> {
        if n > self.messages.len() {
            None
        } else {
            Some(&self.messages[..n])
        }
    }

    /// Expose the messages slice for callers that need full access (e.g.
    /// `replay --resume-from N` printing context before continuing).
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

// Tiny RFC3339-ish timestamp without pulling in `chrono`. Format:
// "YYYY-MM-DDTHH:MM:SSZ" using UTC.
fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert epoch secs to UTC date+time without a calendar library.
    // Days since 1970-01-01:
    let day = secs / 86_400;
    let sec_of_day = secs % 86_400;
    let (h, m, s) = (sec_of_day / 3600, (sec_of_day / 60) % 60, sec_of_day % 60);
    let (y, mo, d) = epoch_day_to_ymd(day as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert "days since 1970-01-01" to (year, month, day) using the
/// civil-from-days algorithm by Howard Hinnant. Public-domain, exact
/// for any 64-bit day count.
fn epoch_day_to_ymd(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn roundtrip_preserves_messages_and_meta() {
        let messages = vec![
            Message::system("You are a helpful assistant.".to_string()),
            Message::user("Hello".to_string()),
            Message::assistant("Hi there!".to_string()),
        ];
        let file = TranscriptFile::new(messages.clone(), 3, Some("test-model".to_string()));

        let json = file.to_json().unwrap();
        let restored: TranscriptFile = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.messages.len(), messages.len());
        assert_eq!(restored.meta.steps, 3);
        assert_eq!(restored.meta.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn write_then_read_via_tempfile() {
        let messages = vec![Message::user("test".to_string())];
        let file = TranscriptFile::new(messages.clone(), 1, None);

        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_transcript_{}.json", std::process::id()));

        file.write_to(&path).unwrap();

        let restored = TranscriptFile::read_from(&path).unwrap();
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.meta.steps, 1);
        assert!(restored.meta.model.is_none());

        // Clean up
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn timestamp_format_is_iso_8601_basic() {
        let file = TranscriptFile::new(vec![], 0, None);
        let ts = &file.meta.saved_at;

        // Format: YYYY-MM-DDTHH:MM:SSZ
        assert!(
            ts.len() == 20,
            "timestamp length should be 20, got {}",
            ts.len()
        );
        assert!(
            ts.chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false),
            "should start with digit: {}",
            ts
        );
        assert!(ts.ends_with('Z'), "should end with Z: {}", ts);

        // Check structure: XXXX-XX-XXTXX:XX:XXZ
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn meta_model_is_optional() {
        let file_with_model = TranscriptFile::new(vec![], 1, Some("model".to_string()));
        let file_without_model = TranscriptFile::new(vec![], 1, None);

        assert!(file_with_model.meta.model.is_some());
        assert!(file_without_model.meta.model.is_none());

        // Roundtrip both
        let json1 = file_with_model.to_json().unwrap();
        let json2 = file_without_model.to_json().unwrap();

        let restored1: TranscriptFile = serde_json::from_str(&json1).unwrap();
        let restored2: TranscriptFile = serde_json::from_str(&json2).unwrap();

        assert!(restored1.meta.model.is_some());
        assert!(restored2.meta.model.is_none());
    }

    #[test]
    fn pretty_includes_header_and_meta() {
        let file = TranscriptFile::new(
            vec![Message::user("hello".to_string())],
            5,
            Some("gpt-4".to_string()),
        );
        let output = file.pretty();
        assert!(output.contains("=== transcript (1 messages) ==="));
        assert!(output.contains("saved_at:"));
        assert!(output.contains("steps: 5"));
        assert!(output.contains("model: gpt-4"));
    }

    #[test]
    fn pretty_renders_each_message_with_index_and_role() {
        let messages = vec![
            Message::system("sys".to_string()),
            Message::user("usr".to_string()),
            Message::assistant("asst".to_string()),
        ];
        let file = TranscriptFile::new(messages, 3, None);
        let output = file.pretty();
        assert!(output.contains("--- [0] System ---"));
        assert!(output.contains("--- [1] User ---"));
        assert!(output.contains("--- [2] Assistant ---"));
    }

    #[test]
    fn pretty_handles_empty_content_gracefully() {
        let file = TranscriptFile::new(vec![Message::user(String::new())], 1, None);
        let output = file.pretty();
        assert!(output.contains("--- [0] User ---"));
        // No crash; section header still present
    }

    #[test]
    fn take_first_n_returns_prefix() {
        let messages = vec![
            Message::system("sys".to_string()),
            Message::user("usr".to_string()),
            Message::assistant("asst".to_string()),
        ];
        let file = TranscriptFile::new(messages, 3, None);
        let prefix = file.take_first_n(2).expect("n=2 fits");
        assert_eq!(prefix.len(), 2);
        assert_eq!(prefix[0].content, "sys");
        assert_eq!(prefix[1].content, "usr");
    }

    #[test]
    fn take_first_n_zero_returns_empty_slice() {
        let file = TranscriptFile::new(vec![Message::user("x".to_string())], 1, None);
        let prefix = file.take_first_n(0).expect("n=0 always valid");
        assert!(prefix.is_empty());
    }

    #[test]
    fn take_first_n_too_large_returns_none() {
        let file = TranscriptFile::new(vec![Message::user("only".to_string())], 1, None);
        assert!(file.take_first_n(2).is_none());
    }

    #[test]
    fn pretty_tail_shows_skipped_notice_and_tail() {
        let messages = vec![
            Message::system("first".to_string()),
            Message::user("second".to_string()),
            Message::assistant("third".to_string()),
            Message::system("fourth".to_string()),
            Message::user("fifth".to_string()),
        ];
        let file = TranscriptFile::new(messages, 5, None);
        let output = file.pretty_tail(2);

        // Should contain skipped notice
        assert!(output.contains("...skipped 3 earlier messages"));
        // Should contain header
        assert!(output.contains("=== transcript (5 messages) ==="));
        // Should contain the last 2 messages (indices 3 and 4)
        assert!(output.contains("--- [3] System ---"));
        assert!(output.contains("fourth"));
        assert!(output.contains("--- [4] User ---"));
        assert!(output.contains("fifth"));
        // Should NOT contain the first 3 messages
        assert!(!output.contains("--- [0] System ---"));
        assert!(!output.contains("first"));
        assert!(!output.contains("--- [1] User ---"));
        assert!(!output.contains("second"));
        assert!(!output.contains("--- [2] Assistant ---"));
        assert!(!output.contains("third"));
    }

    #[test]
    fn pretty_tail_equals_pretty_when_n_large() {
        let messages = vec![
            Message::system("one".to_string()),
            Message::user("two".to_string()),
        ];
        let file = TranscriptFile::new(messages, 2, Some("model".to_string()));

        // n >= total should be equivalent to pretty()
        let tail_5 = file.pretty_tail(5);
        let full = file.pretty();
        assert_eq!(tail_5, full);

        // n == total should also be equivalent
        let tail_2 = file.pretty_tail(2);
        assert_eq!(tail_2, full);
    }

    #[test]
    fn pretty_head_shows_skipped_notice_and_head() {
        let messages = vec![
            Message::system("first".to_string()),
            Message::user("second".to_string()),
            Message::assistant("third".to_string()),
            Message::system("fourth".to_string()),
            Message::user("fifth".to_string()),
        ];
        let file = TranscriptFile::new(messages, 5, None);
        let output = file.pretty_head(2);

        // Should contain header
        assert!(output.contains("=== transcript (5 messages) ==="));
        // Should contain skipped notice at the end
        assert!(output.contains("... skipping 3 later messages"));
        // Should contain the first 2 messages (indices 0 and 1)
        assert!(output.contains("--- [0] System ---"));
        assert!(output.contains("first"));
        assert!(output.contains("--- [1] User ---"));
        assert!(output.contains("second"));
        // Should NOT contain the last 3 messages
        assert!(!output.contains("--- [2] Assistant ---"));
        assert!(!output.contains("third"));
        assert!(!output.contains("--- [3] System ---"));
        assert!(!output.contains("fourth"));
        assert!(!output.contains("--- [4] User ---"));
        assert!(!output.contains("fifth"));
    }

    #[test]
    fn pretty_head_equals_pretty_when_n_large() {
        let messages = vec![
            Message::system("one".to_string()),
            Message::user("two".to_string()),
        ];
        let file = TranscriptFile::new(messages, 2, Some("model".to_string()));

        // n >= total should be equivalent to pretty()
        let head_5 = file.pretty_head(5);
        let full = file.pretty();
        assert_eq!(head_5, full);

        // n == total should also be equivalent
        let head_2 = file.pretty_head(2);
        assert_eq!(head_2, full);
    }
}

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
        crate::atomic::atomic_write(path, json.as_bytes())
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
    fn take_first_n_exactly_len_returns_all() {
        // Kills: `replace > with >=` in take_first_n.
        // n == len must return Some (all messages), NOT None.
        // With `>= `, `n >= len` would be true and return None incorrectly.
        let msgs = vec![
            Message::user("a".to_string()),
            Message::user("b".to_string()),
        ];
        let file = TranscriptFile::new(msgs, 2, None);
        let result = file
            .take_first_n(2)
            .expect("n == len should return all messages");
        assert_eq!(result.len(), 2, "must return all 2 messages when n == len");
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

    // -----------------------------------------------------------------------
    // pretty(): empty-content and newline-handling edge cases
    // (kills delete-! mutants on is_empty() and ends_with('\n'))
    // -----------------------------------------------------------------------

    #[test]
    fn pretty_empty_content_produces_no_content_line() {
        // Message with empty content must not produce a content line
        let file = TranscriptFile::new(vec![Message::user(String::new())], 1, None);
        let output = file.pretty();
        // The header for the message is present
        assert!(output.contains("--- [0] User ---"));
        // But there must be no empty "content" output between the header and the next blank line
        // The output must NOT have two consecutive blank lines (which would happen if empty content
        // is printed as an empty string + newline).
        let lines: Vec<&str> = output.lines().collect();
        let msg_line = lines
            .iter()
            .position(|l| l.contains("--- [0] User ---"))
            .unwrap();
        // The line immediately after the header should be empty (the separator blank line)
        // i.e. no content was emitted between the header and the end of the block.
        let after_header = lines.get(msg_line + 1).copied().unwrap_or("");
        assert!(
            after_header.is_empty(),
            "empty content must not emit a content line; got: '{after_header}'"
        );
    }

    #[test]
    fn pretty_content_with_trailing_newline_not_doubled() {
        // Content already ending with '\n' must NOT get an extra '\n' before the block separator.
        // The block separator adds one '\n' always, so the sequence should be:
        //   "line\n" + block_sep "\n"  = "line\n\n"  (2 newlines, not 3)
        let file = TranscriptFile::new(vec![Message::user("line\n".to_string())], 1, None);
        let output = file.pretty();
        assert!(
            output.contains("line\n"),
            "original newline must be preserved"
        );
        assert!(
            !output.contains("line\n\n\n"),
            "triple newline must not appear: content already ends with newline, only block separator should follow"
        );
    }

    #[test]
    fn pretty_content_without_trailing_newline_gets_one() {
        // Content without trailing '\n': the function adds one, then the block separator adds another.
        // With mutant (delete !): no newline is added, so "no-newline" would be followed only by
        // the block separator → the message wouldn't end with "no-newline\n\n" but "no-newline\n".
        let file = TranscriptFile::new(vec![Message::user("no-newline".to_string())], 1, None);
        let output = file.pretty();
        // Both the added newline AND the block separator must follow the content
        assert!(
            output.contains("no-newline\n\n"),
            "content without trailing newline must have added newline + block separator"
        );
    }

    // Same edge cases for pretty_tail

    #[test]
    fn pretty_tail_content_with_trailing_newline_not_doubled() {
        let messages = vec![
            Message::user("old".to_string()),
            Message::user("line\n".to_string()),
        ];
        let file = TranscriptFile::new(messages, 2, None);
        let output = file.pretty_tail(1);
        assert!(
            !output.contains("line\n\n\n"),
            "pretty_tail must not triple-newline already-terminated content"
        );
    }

    #[test]
    fn pretty_tail_empty_content_produces_no_content_line() {
        let messages = vec![
            Message::user("old".to_string()),
            Message::user(String::new()),
        ];
        let file = TranscriptFile::new(messages, 2, None);
        let output = file.pretty_tail(1);
        let lines: Vec<&str> = output.lines().collect();
        let msg_line = lines
            .iter()
            .position(|l| l.contains("--- [1] User ---"))
            .unwrap();
        let after = lines.get(msg_line + 1).copied().unwrap_or("");
        assert!(
            after.is_empty(),
            "empty content in pretty_tail must not emit a content line; got: '{after}'"
        );
    }

    // Same edge cases for pretty_head

    #[test]
    fn pretty_head_content_with_trailing_newline_not_doubled() {
        let messages = vec![
            Message::user("line\n".to_string()),
            Message::user("new".to_string()),
        ];
        let file = TranscriptFile::new(messages, 2, None);
        let output = file.pretty_head(1);
        assert!(
            !output.contains("line\n\n\n"),
            "pretty_head must not triple-newline already-terminated content"
        );
    }

    #[test]
    fn pretty_head_empty_content_produces_no_content_line() {
        let messages = vec![
            Message::user(String::new()),
            Message::user("new".to_string()),
        ];
        let file = TranscriptFile::new(messages, 2, None);
        let output = file.pretty_head(1);
        let lines: Vec<&str> = output.lines().collect();
        let msg_line = lines
            .iter()
            .position(|l| l.contains("--- [0] User ---"))
            .unwrap();
        let after = lines.get(msg_line + 1).copied().unwrap_or("");
        assert!(
            after.is_empty(),
            "empty content in pretty_head must not emit a content line; got: '{after}'"
        );
    }

    // -----------------------------------------------------------------------
    // epoch_day_to_ymd: known-date tests (kills arithmetic mutants)
    // -----------------------------------------------------------------------

    #[test]
    fn epoch_day_to_ymd_unix_epoch() {
        // Day 0 = 1970-01-01
        assert_eq!(epoch_day_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn epoch_day_to_ymd_known_dates() {
        // 2024-01-01 = day 19723 from epoch (2024-1970)*365 + leap days
        assert_eq!(epoch_day_to_ymd(19723), (2024, 1, 1));
        // 2000-03-01 is a known leap-year boundary date
        // Days from 1970-01-01 to 2000-03-01 = 11017
        assert_eq!(epoch_day_to_ymd(11017), (2000, 3, 1));
        // 1970-12-31 = day 364
        assert_eq!(epoch_day_to_ymd(364), (1970, 12, 31));
        // 1971-01-01 = day 365
        assert_eq!(epoch_day_to_ymd(365), (1971, 1, 1));
    }

    /// Kills: `replace + with -` at line 202:33 AND `replace - with +` at line 202:47.
    ///
    /// Unix day 47541 = 2100-03-01; at this point `doe = 36524` exactly (first day
    /// where `doe / 36524 = 1`).  Both mutations flip the century-correction term
    /// in the `yoe` formula, shifting the result by 2 years in opposite directions:
    /// mutant 202:33 produces year 2099; mutant 202:47 produces year 2101.
    #[test]
    fn epoch_day_to_ymd_century_correction() {
        // 2100-03-01: first day after the "no leap year in 2100" boundary.
        assert_eq!(epoch_day_to_ymd(47541), (2100, 3, 1));
    }

    /// Kills: `replace - with +` at line 202:47 (second subtraction in yoe formula).
    ///
    /// `yoe = (doe - doe/1460 + doe/36524 - doe/146_096) / 365`
    /// The term `doe / 146_096` is 0 for most dates, so mutations there appear
    /// identical.  It equals 1 only when `doe = 146_096` — the very last day of a
    /// 400-year era.  Day 157_113 = 2400-02-29 has era=5, doe=146_096.
    ///
    /// Original: yoe = (146_096 − 100 + 3 − 1) / 365 = 399 → year 2399 → adj +1 → 2400.
    /// Mutant  : yoe = (146_096 − 100 + 3 + 1) / 365 = 400 → year 2400 → adj +1 → 2401.
    #[test]
    fn epoch_day_to_ymd_last_day_of_era() {
        // 2400-02-29: last day of the 400-year era ending in 2400.
        // 2400-01-01 = day 10957 + 146_097 = 157_054; +31 (Jan) +28 = 157_113.
        assert_eq!(epoch_day_to_ymd(157_113), (2400, 2, 29));
    }

    /// Kills: `replace - with +` and `replace - with /` at line 200:40.
    ///
    /// Unix day -719528 is proleptic Gregorian year 0, January 1.
    /// z = -60 < 0, so the `else { z - 146_096 }` branch runs for the era calculation.
    /// Both mutations replace `-` in `z - 146_096`:
    ///   `+ → +`: era becomes (−60 + 146096) / 146097 = 0 instead of -1 → huge doe overflow.
    ///   `÷ → /`: era becomes (−60 / 146096) = 0 instead of -1 → same overflow.
    #[test]
    fn epoch_day_to_ymd_negative_epoch_day_pre_ce() {
        // Year 0 Jan 1 = Unix day -719528 (proleptic Gregorian year 0 = 1 BC).
        assert_eq!(epoch_day_to_ymd(-719528), (0, 1, 1));
    }

    /// Kills: `replace - with /` at line 207:46.
    ///
    /// In the month mapping `if mp < 10 { mp + 3 } else { mp - 9 }`, the mutation
    /// changes `mp - 9` to `mp / 9`.  For mp=11 (February): `11/9 = 1` (January!)
    /// but `11 - 9 = 2` (February).  A February date catches the mutation.
    #[test]
    fn epoch_day_to_ymd_february() {
        // 2024-02-01 = 2024-01-01 + 31 = day 19723 + 31 = 19754
        assert_eq!(epoch_day_to_ymd(19754), (2024, 2, 1));
        // 2024-02-29 = leap day
        assert_eq!(epoch_day_to_ymd(19782), (2024, 2, 29));
    }

    /// Kills:
    ///   `replace % with / at 189:27` — sec_of_day becomes the day count (huge value)
    ///   `replace / with * at 190:53` — minutes formula becomes `(n*60)%60 = 0` always
    ///
    /// By comparing the time component of `chrono_lite_now()` against the system clock
    /// we verify the arithmetic produces the correct h/m/s, not an arbitrarily wrong one.
    #[test]
    fn chrono_lite_now_time_matches_system_clock() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let before_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let ts = chrono_lite_now();

        let after_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let h: u64 = ts[11..13].parse().expect("hours in timestamp");
        let m: u64 = ts[14..16].parse().expect("minutes in timestamp");
        let s: u64 = ts[17..19].parse().expect("seconds in timestamp");

        let sec_of_ts = h * 3600 + m * 60 + s;
        let sec_before = before_secs % 86_400;
        let sec_after = after_secs % 86_400;

        assert!(
            sec_of_ts >= sec_before && sec_of_ts <= sec_after + 1,
            "chrono_lite_now() time component {h:02}:{m:02}:{s:02} (sec={sec_of_ts}) must \
             fall within system-clock window [{sec_before},{sec_after}]; ts={ts}"
        );
    }

    /// Kills: `replace TranscriptFile::messages -> &[Message] with Vec::leak(Vec::new())`.
    ///
    /// The mutation replaces the body with an empty leaked vec, so the accessor
    /// always returns `&[]`.  Calling `.messages()` and checking the content
    /// pins the correct implementation.
    #[test]
    fn messages_accessor_returns_correct_slice() {
        let expected = vec![
            Message::user("hello".to_string()),
            Message::assistant("world".to_string()),
        ];
        let file = TranscriptFile::new(expected.clone(), 1, None);
        let slice = file.messages();
        assert_eq!(slice.len(), 2, "messages() must return all messages");
        assert_eq!(slice[0].content, "hello");
        assert_eq!(slice[1].content, "world");
    }

    #[test]
    fn chrono_lite_now_contains_correct_year_and_format() {
        let ts = chrono_lite_now();
        // Must be 20 chars: "YYYY-MM-DDTHH:MM:SSZ"
        assert_eq!(ts.len(), 20, "timestamp must be 20 chars: {ts}");
        // Year must be >= 2024 (this test won't be run before then)
        let year: u32 = ts[..4].parse().expect("first 4 chars must be year digits");
        assert!(year >= 2024, "year must be >= 2024, got {year}");
        assert!(ts.ends_with('Z'));
        // Structure delimiters
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }

    /// Kills: `replace / with %` at line 188:20 (`secs / 86_400`).
    ///
    /// With the mutant, `day = secs % 86_400` gives the seconds-within-day
    /// (0 to 86399) instead of the day-since-epoch (~20000+).  The resulting
    /// date string would be somewhere in 1970–2206 rather than the actual
    /// current date, so comparing the date portion against the system clock
    /// reliably catches the mutation.
    #[test]
    fn chrono_lite_now_date_matches_system_clock() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let ts = chrono_lite_now();

        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let date_ts = &ts[..10];

        let (yb, mob, db) = epoch_day_to_ymd((before / 86_400) as i64);
        let (ya, moa, da) = epoch_day_to_ymd((after / 86_400) as i64);
        let date_before = format!("{yb:04}-{mob:02}-{db:02}");
        let date_after = format!("{ya:04}-{moa:02}-{da:02}");

        assert!(
            date_ts == date_before || date_ts == date_after,
            "chrono_lite_now() date '{date_ts}' must match system date \
             '{date_before}' (or '{date_after}' at day boundary); ts={ts}"
        );
    }
}

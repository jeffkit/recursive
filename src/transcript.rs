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
}

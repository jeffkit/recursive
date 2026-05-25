# Goal 08 — Persistent transcripts

## What

Let the CLI write the complete transcript of a run to a JSON file.
A user passes `--transcript-out <path>`; on a successful `run` or
`repl` exit, the transcript is serialised as JSON and saved.

## Why

Right now, when a `recursive run` finishes, the conversation evaporates.
There's no way to:

- replay it on a fresh provider for comparison,
- diff two runs of the same goal under different models,
- audit what a long-running session actually did,
- post-process metrics beyond what our observation scripts grep.

Persisting transcripts is the foundation for all of that. `Message`
already derives `Serialize`/`Deserialize`, so the work is plumbing,
not data design.

## Scope (do exactly this, no more)

### 1. New module `src/transcript.rs`

A small, focused module that defines:

```rust
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
            meta: TranscriptMeta { saved_at, steps, model },
            messages,
        }
    }

    /// Pretty-printed JSON. Stable enough to be diffed across runs.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = self.to_json()
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
```

The `chrono_lite_now` + `epoch_day_to_ymd` helpers exist so we don't
add a new dependency for one timestamp string. Keep them private to
this module.

### 2. Wire into `src/lib.rs`

```rust
mod transcript;
pub use transcript::{TranscriptFile, TranscriptMeta};
```

### 3. CLI flag in `src/main.rs`

In the existing `Cli` struct (or wherever `run_once`'s args live), add:

```rust
/// Persist the full transcript to <path> as JSON when the run finishes.
#[arg(long, env = "RECURSIVE_TRANSCRIPT_OUT")]
transcript_out: Option<PathBuf>,
```

After `agent.run(...)` returns `Ok(outcome)` in the `run` command,
write the transcript if the flag was provided:

```rust
if let Some(p) = cli.transcript_out {
    let file = recursive::TranscriptFile::new(
        outcome.transcript.clone(),
        outcome.steps,
        Some(config.model.clone()),
    );
    file.write_to(&p)?;
    eprintln!("transcript: wrote {} messages to {}",
              outcome.transcript.len(), p.display());
}
```

(`outcome.transcript` already exists.) Mirror the same in `repl` on
clean exit (`Ctrl+D`) if straightforward; if not, scope this goal to
the `run` command only and note that `repl` is a follow-up.

### 4. Tests

Add to `src/transcript.rs`:

1. `roundtrip_preserves_messages_and_meta` — build a `TranscriptFile`
   with a system + user + assistant message, serialise to JSON,
   deserialise, check equality.
2. `write_then_read_via_tempfile` — write to a `tempfile::tempdir()`,
   read back, check equality.
3. `timestamp_format_is_iso_8601_basic` — assert the `saved_at`
   matches a regex like `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`.
   (Use a string check, not the `regex` crate.)
4. `meta_model_is_optional` — build with `model = None`, roundtrip.

Use `tempfile` if it's already in dev-deps; otherwise use
`std::env::temp_dir()` plus a random suffix and clean up. Don't add
new dependencies for this goal.

## Out of scope

- Replay mode (loading a transcript and continuing it). Just persistence.
- Streaming / incremental writes. One write at end is fine.
- Compressing or trimming the JSON.
- Persisting `total_usage` or `finish` reason. The conversation itself
  is what we want; metrics already live in journal/observations.
- Versioning the file format. v1 implicit.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- `recursive run --transcript-out /tmp/r.json "1+1?"` writes a
  well-formed JSON file you can `cat | jq .` on.
- 4 new tests pass.
- No new dependencies in `Cargo.toml`.

## Notes for the agent

- This is a **green-field module + CLI plumbing**. `write_file` is
  fine for creating `src/transcript.rs`; use `apply_patch` for the
  edits to `src/lib.rs` and `src/main.rs`.
- The civil-from-days algorithm above is dense but correct. Copy it
  verbatim if uncertain; the unit test for the timestamp format will
  catch any typo immediately.
- Don't try to make the timestamp time-zone aware. UTC + Z suffix is
  all we need.

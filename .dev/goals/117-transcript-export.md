# Goal 117 — Transcript export to portable JSON

**Roadmap**: Phase 14.3 — Transcript export/import (part 1/1)

**Design principle check**:
- Implemented as: `recursive export` CLI subcommand + `TranscriptExporter` in session.rs
- Reads existing session JSONL → outputs a single portable JSON file
- ❌ Does NOT modify agent.rs

## Why

Sessions are stored as append-only JSONL (one entry per line), which is
great for crash-safety and streaming writes. But for sharing, analysis,
and tooling integration (replay viewers, evaluation pipelines), a single
self-contained JSON file is needed. This adds an `export` subcommand
that converts a session into a portable transcript.

## Scope (do exactly this, no more)

### 1. Add export format struct in `src/session.rs`

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportedTranscript {
    pub version: u32,  // always 1
    pub session_id: String,
    pub model: String,
    pub goal: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub messages: Vec<TranscriptEntry>,
    pub message_count: u64,
}

impl ExportedTranscript {
    /// Build from a session directory (reads meta.json + transcript.jsonl).
    pub fn from_session_dir(session_dir: &Path) -> std::io::Result<Self> {
        let meta_path = session_dir.join("meta.json");
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(&meta_path)?
        ).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let entries = SessionReader::load_transcript(session_dir)?;

        Ok(Self {
            version: 1,
            session_id: meta.session_id,
            model: meta.model,
            goal: meta.goal,
            created_at: meta.created_at,
            finished_at: meta.finished_at,
            status: meta.status,
            messages: entries,
            message_count: entries.len() as u64,
        })
    }

    /// Write to a JSON file.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }
}
```

### 2. Add `export` subcommand in `src/main.rs`

Under the existing `Sessions` command group, add:

```rust
/// Export a session transcript to portable JSON
Export {
    /// Session ID or path to session directory
    session: String,
    /// Output file path (default: stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,
},
```

Implementation:
1. Resolve session path (by ID lookup or direct path)
2. Call `ExportedTranscript::from_session_dir()`
3. Write to output file or stdout

### 3. Tests

- **Test A**: `ExportedTranscript::from_session_dir` reads valid session
- **Test B**: Export produces valid JSON with all expected fields
- **Test C**: Export of empty session (meta only, no messages) works
- **Test D**: CLI `export` with --output writes to file

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` clean.
- `recursive sessions export <session-id>` outputs valid JSON.
- `recursive sessions export <session-id> -o out.json` writes file.
- Exported JSON contains all messages from the original JSONL.

## Notes for the agent

- `SessionMeta` and `SessionReader::load_transcript` already exist in
  `src/session.rs`. Reuse them.
- `TranscriptEntry` is already `Serialize` + `Deserialize`.
- The `sessions` CLI subcommand group already exists in main.rs —
  search for `Sessions` or `sessions` to find where to add `Export`.
- The session directory resolution: check if the input is a directory
  path first; if not, scan `.recursive/sessions/` for a matching session_id.
- Do NOT add new dependencies. `serde` and `serde_json` are already
  available.
- Files to modify: `src/session.rs` (~40 lines), `src/main.rs` (~30 lines)

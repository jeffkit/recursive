# Goal 119 — Add `sessions export` CLI subcommand

**Roadmap**: Phase 14.3 — Transcript export (completion)

**Design principle check**:
- Implemented as: new `Export` variant in `SessionCmd` enum
- Reads existing JSONL session → outputs single portable JSON
- ❌ Does NOT modify agent.rs

## Why

Goal 117 enhanced `sessions list` and `sessions show` but did not add
the `export` subcommand. This goal adds it, allowing users to export
a session as a single self-contained JSON file for sharing and analysis.

## Scope (do exactly this, no more)

### 1. Add ExportedTranscript struct to session.rs

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportedTranscript {
    pub version: u32,
    pub session_id: String,
    pub model: String,
    pub goal: String,
    pub created_at: String,
    pub status: String,
    pub messages: Vec<TranscriptEntry>,
    pub message_count: u64,
}

impl ExportedTranscript {
    pub fn from_session_dir(session_dir: &Path) -> std::io::Result<Self> {
        let meta = SessionReader::load_meta(session_dir)?;
        let entries = SessionReader::load_transcript(session_dir)?;
        Ok(Self {
            version: 1,
            session_id: meta.session_id,
            model: meta.model,
            goal: meta.goal,
            created_at: meta.created_at,
            status: meta.status,
            messages: entries.clone(),
            message_count: entries.len() as u64,
        })
    }
}
```

### 2. Add Export variant to SessionCmd in main.rs

```rust
/// Export a session as portable JSON.
Export {
    /// Session directory path or session ID.
    session: String,
    /// Output file (default: stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,
},
```

### 3. Handle the Export command

```rust
SessionCmd::Export { session, output } => {
    let path = resolve_session_path(&config.workspace, &session)?;
    let exported = recursive::session::ExportedTranscript::from_session_dir(&path)?;
    let json = serde_json::to_string_pretty(&exported)?;
    if let Some(out) = output {
        std::fs::write(&out, &json)?;
        println!("Exported to {}", out.display());
    } else {
        println!("{}", json);
    }
}
```

### 4. Tests

- **Test A**: ExportedTranscript::from_session_dir produces valid struct
- **Test B**: Exported JSON contains all expected fields
- **Test C**: Export of empty session works without panic

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `recursive sessions export <session-dir>` outputs JSON to stdout.
- `recursive sessions export <session-dir> -o out.json` writes file.
- Files modified: `src/session.rs` (~30 lines), `src/main.rs` (~20 lines)

## Notes for the agent

- `SessionReader::load_meta` and `SessionReader::load_transcript` exist.
- `TranscriptEntry` is already `Serialize + Deserialize`.
- The `SessionCmd` enum is in main.rs around line 208.
- Look at how `SessionCmd::Show` resolves paths — reuse the same pattern.
- Do NOT add new dependencies.

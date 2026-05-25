# Goal 45 — Session Management (Phase 4.2)

> **Roadmap**: feature 4.2, M size, Medium impact.
> **Design principle check**: orthogonal — reuses existing
> `TranscriptFile` from g08 as the persistence format; adds a thin
> CLI subcommand `recursive resume <session>`. Pluggable — `resume`
> is opt-in. Testable — round-trip a session file through save +
> load + run.

## What

The existing `--transcript-out` saves the full transcript at the end
of a run. The `replay` subcommand can pretty-print or step-replay.
This goal builds the **production pause/resume** flow:

1. **Session save**: when an agent run terminates with any
   non-success finish (BudgetExceeded, TranscriptLimit, Stuck), the
   binary writes a *session file* next to the transcript with the
   minimum state needed to resume cleanly. Contents:
   - The transcript (already in `TranscriptFile`).
   - Step count consumed.
   - Original goal text.
   - Model + provider used (so resume can use the same).
   - Tool registry version stamp (so resume detects mismatch).

2. **Session resume**: new subcommand `recursive resume <session>`
   that loads the file, reconstructs the agent state, and continues
   from where the previous run left off — same goal, same model,
   same transcript, fresh tool registry.

3. **List sessions**: `recursive sessions` lists known
   `<workspace>/.recursive/sessions/*.json` with a summary line each.

This is different from `replay` (which is for debugging — step
through history) because it's intended for actual continuation.

## Why

The current auto-resume in `self-improve.sh` works for one specific
flow (BudgetExceeded triggers a replay). But production users want:

- Manual checkpointing: "this is going to take a while, save state
  so I can resume after lunch."
- Cross-process continuation: an agent dies (OOM, kill, hang), a new
  process picks up from the saved checkpoint.

This is the foundation for those.

## API sketch

```rust
// src/session.rs (new file)
#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub schema_version: u32,    // start at 1
    pub goal: String,
    pub model: String,
    pub provider: String,
    pub tool_registry_hash: String,  // BLAKE3 of sorted tool names+specs
    pub steps_consumed: u32,
    pub transcript: TranscriptFile,
}

impl SessionFile {
    pub fn write(&self, path: &Path) -> Result<()>;
    pub fn read(path: &Path) -> Result<Self>;
}
```

## Tests

- `session_round_trip` — write a SessionFile, read it back, assert
  field equality.
- `resume_validates_tool_registry_hash` — write session with
  hash=X; load it with a registry whose hash=Y; expect error
  (or warning, depending on design).
- `session_list_finds_files_in_workspace` — write 3 session files,
  call list, assert all 3 returned with goal text.
- `resume_continues_from_seeded_transcript` — write a session
  showing 5 steps consumed; resume it with MockProvider that says
  "Done"; assert outcome `NoMoreToolCalls` and transcript has
  prior 5 + new ones.

## Wiring

- `src/session.rs` (new).
- `src/lib.rs`: export `SessionFile`.
- `src/main.rs`:
  - Auto-write a session file at run end when finish is non-success
    AND `--session-out` flag (or `RECURSIVE_SESSION_OUT` env) is set.
  - Add `Resume { session: PathBuf }` and `Sessions` subcommands.
- `src/agent.rs`: add a small `Agent::snapshot(&self) -> SessionFile`
  helper IF necessary. May be done entirely in main.rs.

## Acceptance

- `cargo build` green.
- `cargo test` green; +4 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `recursive sessions` runs in a fresh workspace without panicking
  (it should just print "no sessions").

## Out of scope (defer)

- Session encryption / signing.
- Session expiry / GC.
- Auto-save mid-run (currently only saves at terminal finish).

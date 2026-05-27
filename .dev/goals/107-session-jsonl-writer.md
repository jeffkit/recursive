# Goal 107 — Session Persistence: JSONL transcript writer

**Roadmap**: Phase 14.1 — Session Persistence (part 1/4)

**Design principle check**:
- Implemented as: refactored `src/session.rs` module + new `SessionWriter` struct
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Orthogonal: persistence is a side-effect layer; Agent core is unaware

## Why

Current session saving dumps the entire transcript as a single JSON blob
on abnormal exit only. This is fragile (crash loses everything), hard to
debug (one huge file), and doesn't support resume properly.

Claude Code's approach (append-only JSONL, one message per line) is
proven: crash-safe, streamable, grep-friendly. We adopt the same
pattern, scoped per-workspace.

## Reference: Claude Code JSONL format (simplified for Recursive)

Claude Code stores each message as a JSON line with:
- `uuid` — unique message ID
- `parentUuid` — links to previous message (enables branching)
- `type` — "user" | "assistant" | "tool_use" | "tool_result"
- `message` — the actual LLM message content
- `timestamp` — ISO 8601
- `sessionId` — session identifier
- Metadata: `cwd`, `version`, `gitBranch`

We simplify this for Recursive's needs.

## Scope (do exactly this, no more)

### 1. Session directory layout

```
~/.recursive/sessions/
  <workspace-slug>/                  # sanitised workspace path
    <session-id>.jsonl               # one file per session
    <session-id>.meta.json           # lightweight metadata
```

**Workspace slug**: The workspace absolute path with `/` replaced by `-`,
leading `-` stripped, truncated to 80 chars. Example:
`/Users/kongjie/projects/Recursive` → `Users-kongjie-projects-Recursive`.

This matches Claude Code's convention (e.g. `-Users-kongjie-projects-force-lab`).

### 2. JSONL message schema (one per line)

```rust
#[derive(Serialize, Deserialize)]
pub struct SessionEntry {
    /// Schema version for forward compatibility.
    pub v: u32,  // = 1
    /// Unique message identifier.
    pub id: String,
    /// Parent message ID (null for first message). Enables branching.
    pub parent_id: Option<String>,
    /// Message type: "system" | "user" | "assistant" | "tool_call" | "tool_result"
    pub entry_type: String,
    /// The message role (maps to LLM role).
    pub role: String,
    /// Text content of the message.
    pub content: String,
    /// Tool calls (for assistant messages), serialized.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<serde_json::Value>,
    /// Tool call ID this result responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// ISO 8601 timestamp.
    pub ts: String,
    /// Session identifier.
    pub session_id: String,
}
```

### 3. Session metadata file (`<session-id>.meta.json`)

```rust
#[derive(Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub goal: String,
    pub model: String,
    pub provider: String,
    pub workspace: String,
    pub tool_registry_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,  // "running" | "completed" | "failed" | "paused"
    pub steps: usize,
    pub message_count: usize,
}
```

### 4. `SessionWriter` implementation

```rust
pub struct SessionWriter {
    session_id: String,
    transcript_path: PathBuf,
    meta_path: PathBuf,
    file: BufWriter<File>,  // opened in append mode
    message_count: usize,
    last_id: Option<String>,
}

impl SessionWriter {
    /// Create a new session, writing initial meta file.
    pub fn create(workspace: &Path, goal: &str, model: &str, provider: &str, tool_specs: &[ToolSpec]) -> Result<Self>;

    /// Append a single message to the JSONL file.
    pub fn append(&mut self, msg: &Message) -> Result<()>;

    /// Finalize session (update meta status).
    pub fn finish(&mut self, status: &str, steps: usize) -> Result<()>;
}
```

### 5. `SessionReader` implementation

```rust
pub struct SessionReader;

impl SessionReader {
    /// Load full transcript from a session JSONL file.
    pub fn load_transcript(path: &Path) -> Result<Vec<Message>>;

    /// Load session metadata.
    pub fn load_meta(path: &Path) -> Result<SessionMeta>;

    /// List all sessions for a workspace.
    pub fn list_sessions(workspace: &Path) -> Result<Vec<SessionMeta>>;

    /// List all sessions across all workspaces.
    pub fn list_all_sessions() -> Result<Vec<(String, SessionMeta)>>;
}
```

### 6. ID generation

Use a simple counter-based ID within session: `"msg_001"`, `"msg_002"`, etc.
Parent linkage: each message's `parent_id` = previous message's `id`.
(This is simpler than UUID and sufficient; branching support comes later.)

### 7. Tests

- **Test A**: `SessionWriter::create` produces valid meta.json + empty .jsonl
- **Test B**: `SessionWriter::append` appends valid JSON lines, file grows
- **Test C**: `SessionReader::load_transcript` round-trips correctly
- **Test D**: `SessionWriter::finish` updates meta status
- **Test E**: `list_sessions` finds sessions for a given workspace
- **Test F**: Crash simulation — partial last line is skipped on read
- **Test G**: Workspace slug generation matches expected pattern

### 8. Backward compatibility

The old `SessionFile` struct (single JSON dump) remains for now — do NOT
delete it. New code writes JSONL; the resume command will prefer JSONL if
available, fall back to legacy JSON.

## Acceptance

- `cargo build` green.
- `cargo test` green (7+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- No new external dependencies (just serde_json + std::io, already present).
- A run without `--session-out` writes nothing (opt-in initially).

## Notes for the agent

- Keep the implementation in `src/session.rs`. Expand the existing file,
  don't create a new module.
- Use `chrono_lite_now()` already defined in session.rs for timestamps.
- Message ID is sequential within session — NOT UUID. Keep it simple.
- The `workspace_slug()` function should be a small utility at the top
  of the module.
- `BufWriter` with flush after each append. No fsync — we accept
  at-most-one-line loss on hard crash.
- Don't wire into `Agent::run` yet — that's Goal 108.

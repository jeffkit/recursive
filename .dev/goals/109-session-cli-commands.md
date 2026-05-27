# Goal 109 — Session CLI commands (list, show, resume, gc)

**Roadmap**: Phase 14.1 — Session Persistence (part 3/4)

**Design principle check**:
- Implemented as: new CLI subcommands under `recursive sessions`
- Does NOT touch agent core or tools

## Why

Users need to manage their sessions: see what ran, inspect transcripts,
resume interrupted runs, clean up old data.

## Scope (do exactly this, no more)

### 1. New subcommand group: `recursive sessions`

```rust
#[derive(Subcommand)]
enum SessionsCmd {
    /// List sessions for current workspace (or all with --all)
    List {
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "table")]
        format: String,  // "table" | "json"
    },
    /// Show session transcript (pretty-printed)
    Show {
        /// Session ID (prefix match OK)
        id: String,
        #[arg(long)]
        tail: Option<usize>,  // last N messages
    },
    /// Resume a session (restart agent with saved transcript)
    Resume {
        /// Session ID or "last" for most recent
        id: String,
        /// New goal for the resumed run
        goal: Option<String>,
    },
    /// Garbage collect old sessions
    Gc {
        /// Remove sessions older than N days
        #[arg(long, default_value = "30")]
        older_than: u32,
        /// Dry run (show what would be deleted)
        #[arg(long)]
        dry_run: bool,
    },
}
```

### 2. `sessions list` output

```
ID          Goal                        Model           Status     Age     Messages
a1b2c3d4    Implement HTTP server       claude-sonnet   completed  2d      156
e5f6g7h8    Fix compaction bug          deepseek-flash  failed     5h      42
i9j0k1l2    Add session persistence     gpt-4o-mini     running    1m      12
```

With `--all`: add a "Workspace" column.

### 3. `sessions show <id>`

Pretty-print the transcript with colors:
- System messages: dim
- User messages: bold cyan
- Assistant messages: normal
- Tool calls: yellow
- Tool results: green (truncated to 5 lines unless --full)

Support `--tail N` to show only last N messages.

### 4. `sessions resume <id|last>`

1. Load meta + transcript from JSONL
2. Validate tool registry hash (warn if changed, don't abort)
3. Build agent with transcript pre-loaded
4. Start new session (child of original) with optional new goal

### 5. `sessions gc`

- Walk `~/.recursive/sessions/*/`
- Check `updated_at` in meta.json
- Delete sessions older than threshold
- Report: "Removed N sessions (M MB freed)"

### 6. Tests

- **Test A**: `sessions list` with empty dir returns clean output
- **Test B**: `sessions list` finds sessions from multiple workspaces
- **Test C**: `sessions show` renders transcript correctly
- **Test D**: `sessions gc --dry-run` doesn't delete anything
- **Test E**: `sessions gc --older-than 0` removes all sessions
- **Test F**: Resume with "last" picks most recent session

## Acceptance

- `cargo build` green.
- `cargo test` green (6+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- `recursive sessions list` works with no sessions (empty table).
- `recursive sessions --help` shows all subcommands.

## Notes for the agent

- Session ID prefix matching: if user types `a1b2`, match any session
  starting with that prefix. Error if ambiguous (multiple matches).
- Use the existing `colored` patterns from main.rs for terminal output.
- The `list` command should sort by `updated_at` descending (newest first).
- Don't load full transcripts for `list` — only read meta.json files.

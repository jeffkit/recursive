# Goal 111 — Memory Layer 1: Working Memory (structured scratchpad)

**Roadmap**: Phase 14.5 — Memory System (part 2/4)

**Design principle check**:
- Implemented as: refactored `src/tools/memory.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Tools are registered in the tool registry, agent uses them via normal tool calls

## Why

Current `memory.rs` stores flat "notes" (id + tags + text). This is
too unstructured for working memory. The agent needs key-value pairs
that represent current state: what's the task, what's been decided,
what's blocked.

Working Memory is the agent's "whiteboard" — structured, small, always
available, persists across sessions in the same workspace.

## Scope (do exactly this, no more)

### 1. Replace flat notes with structured KV store

New on-disk format: `<workspace>/.recursive/memory/scratchpad.json`

```rust
#[derive(Serialize, Deserialize)]
pub struct Scratchpad {
    pub version: u32,  // = 1
    pub entries: BTreeMap<String, ScratchpadEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ScratchpadEntry {
    pub value: String,
    pub updated_at: String,
    pub source_session: Option<String>,  // which session wrote this
}
```

Size cap: 64KB total file size. On overflow, refuse writes with error.

### 2. New tools (replace old remember/recall/forget for workspace scope)

| Tool | Parameters | Description |
|------|-----------|-------------|
| `memory_get` | `key: String` | Get value for key. Returns null if absent. |
| `memory_set` | `key: String, value: String` | Upsert entry. |
| `memory_delete` | `key: String` | Remove entry. |
| `memory_list` | (none) | Return all key-value pairs with timestamps. |

### 3. Tool implementation

```rust
pub struct WorkingMemoryTool {
    workspace: PathBuf,
    store: Mutex<Scratchpad>,
}

impl WorkingMemoryTool {
    pub fn new(workspace: &Path) -> Self {
        let store = Self::load_or_default(workspace);
        Self { workspace: workspace.to_path_buf(), store: Mutex::new(store) }
    }

    fn save(&self, store: &Scratchpad) -> Result<()> {
        let path = self.workspace.join(".recursive/memory/scratchpad.json");
        let json = serde_json::to_string_pretty(store)?;
        if json.len() > 65536 {
            return Err(Error::Tool {
                name: "memory_set".into(),
                message: "scratchpad size limit (64KB) exceeded".into(),
            });
        }
        // atomic write: write to .tmp then rename
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}
```

### 4. Migration from old format

If `memory.json` (old format) exists but `scratchpad.json` doesn't:
- Convert each old note to a scratchpad entry: `key = note.id`, `value = note.text`
- Write as scratchpad.json
- Rename old file to `memory.json.bak`

### 5. Optional: inject top entries into system prompt

If `--session` is active and scratchpad is non-empty, inject a summary
of the 5 most recently updated entries as a section in the system prompt:

```
# Working Memory (recent state)
- current_task: "Implementing session persistence"
- architecture_decision: "JSONL over SQLite"
- blocked_on: (none)
```

This gives the agent immediate awareness of state without needing to
call `memory_list` first.

### 6. Tests

- **Test A**: `memory_set` + `memory_get` round-trip
- **Test B**: `memory_set` on existing key updates value and timestamp
- **Test C**: `memory_delete` removes key
- **Test D**: `memory_list` returns all entries sorted by key
- **Test E**: Size cap enforcement (write 70KB → error)
- **Test F**: Atomic write (simulate crash during save → old data intact)
- **Test G**: Migration from old memory.json format
- **Test H**: Top entries injection into system prompt

## Acceptance

- `cargo build` green.
- `cargo test` green (8+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- Old `remember`/`recall`/`forget` tools still work (for global scope).
- New `memory_get`/`set`/`delete`/`list` work for workspace scope.
- `scratchpad.json` is human-readable and editable.

## Notes for the agent

- Keep the OLD `remember`/`recall`/`forget` tools for global memory
  (`~/.recursive/memory.json` or `RECURSIVE_MEMORY_GLOBAL=1`). They
  become Layer 2 (semantic memory) tools.
- The NEW `memory_*` tools are workspace-scoped. They live alongside
  the old tools, not replacing them.
- Use `BTreeMap` for deterministic ordering (easier to diff/debug).
- Atomic write via tmp+rename prevents corruption on crash.
- The `source_session` field is optional — populate it if a SessionWriter
  is active, leave None otherwise.

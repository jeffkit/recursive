# Goal 261 — Partial-read guard for Edit tool (S-1)

**Roadmap**: Code quality — Edit tool precision follow-up

**Design principle check**:
- Implemented as: `Arc<Mutex<ReadFileState>>` shared between `ReadFile` and
  `StrReplaceTool`, injected via `ToolRegistry` (same pattern as `TouchedFiles`)
- ❌ Does NOT branch inside `agent.rs::Agent::run`

## Why

When an agent reads only a portion of a large file (using `start_line`/
`end_line` on the `Read` tool) and then attempts to `Edit` that file,
the edit is unsafe: the agent has an incomplete view of the file's
contents. It may:

- Choose an `old_string` that appears multiple times (but only saw one
  occurrence in its partial view)
- Miss context around the target region that makes the replacement wrong
- Fail to find the `old_string` at all (it was in the unread region)

`fake-cc`'s `FileEditTool` tracks `isPartialView` in `readFileState`
and rejects edits on partially-read files with a clear error message:
"File has not been fully read. Read the complete file before editing."

Recursive has had `start_line`/`end_line` support since goal 26 but no
such guard. This is the root-cause of "large-file Edit precision" failures
observed in self-improve runs.

## Architecture

Follow the **exact same pattern** as `TouchedFiles` (Goal 164):

1. A new shared state struct `ReadFileState` lives in `src/tools/fs.rs`
2. It is attached to `ToolRegistry` via `Arc<Mutex<ReadFileState>>`
   using a new `with_read_file_state(slot)` builder method
3. `ReadFile::execute` writes a record into the state after each
   successful read (path → `ReadRecord { is_partial: bool }`)
4. `StrReplaceTool::execute` reads the state and rejects edits on
   partially-read files before doing any work

The two tools share state only through the `Arc<Mutex<ReadFileState>>`
that the registry holds — they do not reference each other directly.

## Scope (do exactly this, no more)

### 1. `src/tools/fs.rs` — add `ReadFileState` and `ReadRecord`

```rust
/// Tracks which files have been read this session and whether the read
/// was partial (line-range) or complete.
#[derive(Debug, Default, Clone)]
pub struct ReadFileState {
    pub records: HashMap<PathBuf, ReadRecord>,
}

#[derive(Debug, Clone)]
pub struct ReadRecord {
    /// True when the read used start_line/end_line and did NOT cover
    /// the whole file.
    pub is_partial: bool,
}

impl ReadFileState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, path: PathBuf, is_partial: bool) {
        self.records.insert(path, ReadRecord { is_partial });
    }

    pub fn get(&self, path: &Path) -> Option<&ReadRecord> {
        self.records.get(path)
    }
}
```

### 2. `src/tools/fs.rs` — attach state slot to `ReadFile`

Add an optional `read_state` field to `ReadFile`:

```rust
#[derive(Debug, Clone)]
pub struct ReadFile {
    pub root: PathBuf,
    pub max_bytes: usize,
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}
```

In `ReadFile::execute`, after a successful read, record into the slot:

```rust
// Determine is_partial: true when a line range was requested AND
// the slice does not cover the whole file.
let is_partial = (start_line.is_some() || end_line.is_some())
    && !(start == 1 && end == total_lines);

if let Some(slot) = &self.read_state {
    if let Ok(mut state) = slot.lock() {
        state.record(abs.clone(), is_partial);
    }
}
```

Record both the full-read case (`is_partial: false`) and the partial
case (`is_partial: true`) so `StrReplaceTool` can distinguish "never
read" from "read partially" from "read fully".

### 3. `src/tools/str_replace.rs` — add guard

Add an optional `read_state` field to `StrReplaceTool`:

```rust
#[derive(Debug, Clone)]
pub struct StrReplaceTool {
    pub root: PathBuf,
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}
```

At the top of `execute`, after resolving `abs_path` and before reading
the file, check the state:

```rust
if let Some(slot) = &self.read_state {
    if let Ok(state) = slot.lock() {
        match state.get(&abs_path) {
            None => {
                return Err(Error::Tool {
                    name: "Edit".into(),
                    message: format!(
                        "File `{file_path}` has not been read yet. \
                         Read it first before editing."
                    ),
                });
            }
            Some(record) if record.is_partial => {
                return Err(Error::Tool {
                    name: "Edit".into(),
                    message: format!(
                        "File `{file_path}` was only partially read \
                         (line range). Read the complete file before editing."
                    ),
                });
            }
            Some(_) => {} // full read — proceed
        }
    }
}
```

**Important**: only enforce the guard when `read_state` is `Some`. When
`None` (the default), the tool behaves exactly as before (backward
compatible). The guard is opt-in via the registry builder.

### 4. `src/tools/mod.rs` — add `with_read_file_state` to `ToolRegistry`

Following the `with_touched_files` pattern exactly:

```rust
/// Shared read-file state, used by ReadFile + StrReplaceTool to enforce
/// the partial-read guard.
read_file_state: Option<Arc<Mutex<ReadFileState>>>,
```

Add to `ToolRegistry`:

```rust
pub fn with_read_file_state(mut self, slot: Arc<Mutex<ReadFileState>>) -> Self {
    self.read_file_state = Some(slot);
    self
}

pub fn read_file_state(&self) -> Option<Arc<Mutex<ReadFileState>>> {
    self.read_file_state.clone()
}
```

And propagate into `with_same_transport` and `fork`.

### 5. `src/run_core.rs` — wire it up

In the function that builds the `ToolRegistry` (search for
`.register(Arc::new(ReadFile::new(workspace)))` and
`.register(Arc::new(StrReplaceTool::new(workspace)))`):

```rust
let read_state = Arc::new(Mutex::new(ReadFileState::new()));
// ... existing registry build ...
let registry = registry
    .with_read_file_state(read_state.clone());

// Replace the plain ReadFile / StrReplaceTool registrations with
// state-aware versions:
registry.register(Arc::new(ReadFile {
    root: workspace.to_path_buf(),
    max_bytes: 256 * 1024,
    read_state: Some(read_state.clone()),
}));
registry.register(Arc::new(StrReplaceTool {
    root: workspace.to_path_buf(),
    read_state: Some(read_state.clone()),
}));
```

Check how `run_core.rs` currently constructs the registry for both
the main agent and sub-agents/workers — wire `read_state` in all
call sites that build a fresh registry for an independent agent
(sub-agents should get their own independent `ReadFileState` instance,
not the parent's, since they have their own Read/Edit context).

### 6. Tests

Add to `src/tools/fs.rs` tests:

- `read_state_records_full_read`: write a file, read it without line
  range, assert `ReadFileState.get(path).is_partial == false`.
- `read_state_records_partial_read`: write a 10-line file, read lines
  2-5, assert `is_partial == true`.
- `read_state_full_range_not_partial`: write a 5-line file, read with
  `start_line=1, end_line=5` (covers everything), assert `is_partial
  == false`.

Add to `src/tools/str_replace.rs` tests:

- `edit_rejected_when_file_never_read`: create a file, attempt edit
  without any prior Read, assert error contains "not been read".
- `edit_rejected_when_partial_read`: create a 20-line file, record a
  partial read in the state, attempt edit, assert error contains
  "partially read".
- `edit_allowed_after_full_read`: create a file, record a full read in
  the state, attempt edit, assert success.
- `edit_allowed_when_no_read_state`: use `StrReplaceTool::new()` (no
  `read_state`), assert edit works without guard (backward compat).

### 7. Verify

```bash
cargo test --lib tools::fs
cargo test --lib tools::str_replace
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean.

## Acceptance

- `ReadFile` records `is_partial` into `ReadFileState` after every
  successful read (both full and partial)
- `StrReplaceTool` rejects edits on files that were never read or were
  only partially read, when `read_state` is wired
- The guard is opt-in: `StrReplaceTool::new(root)` (no state) behaves
  exactly as before
- All new tests pass; no existing tests regressed
- All quality gates clean

## Notes for the agent

- Use `std::sync::Mutex` (not `tokio::sync::Mutex`) — `ReadFileState`
  is accessed synchronously from tool `execute` methods.
- `abs_path` in `StrReplaceTool::execute` is a `PathBuf` from
  `resolve_within`. Use this as the key, not the raw `file_path` string.
- `ReadFile::execute` uses `abs` (the resolved `PathBuf`) already.
  Record after the content is successfully decoded (after the
  `String::from_utf8` call), not before.
- The `is_partial` determination: a read is partial if AND ONLY IF
  `(start_line.is_some() || end_line.is_some()) && !(start==1 && end==total_lines)`.
  A read with `start_line=1, end_line=N` where N equals the total line
  count is a full read (not partial).
- Sub-agents (spawned via `SpawnWorkerTool`) should get their own fresh
  `ReadFileState` (empty) because they have an independent read history.
  Check `src/tools/spawn_worker.rs` to see where the registry is built
  for sub-agents and wire accordingly.
- Do NOT reset `ReadFileState` between turns. Once a file is fully read,
  it stays marked as fully read for the session. (A future goal can add
  invalidation on `Write`/`Edit`.)
- Do NOT add the guard to `WriteFile` — it's not needed there.

## Out of scope (DO NOT do these)

- Don't invalidate `ReadFileState` when a file is written (separate goal)
- Don't add a `read_state` field to `WriteFile`
- Don't track read timestamps or content hashes (that's fake-cc's
  modification-since-read check — a different, larger goal)
- Don't change the `Tool` trait signature
- Don't refactor how `ToolRegistry` stores tools

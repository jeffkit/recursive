# Goal 334 — Re-inject recently-read files after cross-turn compaction

**Roadmap**: Compaction upgrade (WS-3a — post-compact file restoration)

**Design principle check**:
- Implemented as: a `FileReinjector` in `src/compact/reinject.rs`, invoked
  from `src/runtime.rs::maybe_compact_cross_turn` after the summary is
  spliced in.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT emit `Role::Tool` messages — only `Role::System` attachments,
  so no orphan tool result can be created (invariant #8 safe).

## Why

After an LLM-summary compaction, the transcript is `[summary, ...recent N]`.
The summary mentions file paths but not contents, so on the next turn the
model frequently re-`Read`s files it already read — paying for the read
again and re-bloating the context. fake-cc re-injects the most-recently-read
files as attachments right after the summary (capped at 5 files, 50K tokens,
5K/file), deduped against the preserved tail, saving up to 25K tokens/compact
of redundant re-reads.

Recursive already maintains `ReadFileState` (`src/tools/fs.rs:46`) — an LRU
`HashMap<PathBuf, ReadRecord{content,timestamp}>` shared by Read/Edit/Write.
This goal exposes a read accessor and re-injects the recent files after the
cross-turn compaction summary.

## Scope (do exactly this, no more)

### 1. `src/tools/fs.rs` — public accessor

Add:
```rust
impl ReadFileState {
    /// Return up to `n` most-recently-recorded files, newest first, as
    /// (path, content) pairs. Locks the mutex externally; this method takes
    /// `&self` and reads `insertion_order`/`records` without mutating.
    pub fn recent_files(&self, n: usize) -> Vec<(PathBuf, String)> {
        self.insertion_order
            .iter()
            .rev()
            .take(n)
            .filter_map(|p| self.records.get(p).map(|r| (p.clone(), r.content.clone())))
            .collect()
    }
}
```
`recent_files` is `&self` (no mutation); callers hold the `Mutex` lock.

### 2. `src/compact/reinject.rs` — new module

```rust
//! Post-compaction re-injection of recently-read files as System attachments.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use crate::message::Message;
use crate::tools::fs::ReadFileState;

#[derive(Debug, Clone)]
pub struct FileReinjector {
    pub max_files: usize,
    pub token_budget: usize,      // default 50_000
    pub per_file_budget: usize,   // default 5_000
    pub read_state: Arc<Mutex<ReadFileState>>,
}

impl FileReinjector {
    /// Build attachment messages to insert after the compaction summary.
    /// `preserved` is the recent-N slice kept verbatim — files whose path
    /// already appears in a preserved `Role::Tool` message's content are
    /// skipped (heuristic dedup).
    pub fn reinject(&self, preserved: &[Message]) -> Vec<Message> { /* ... */ }
}
```

`reinject` logic:
1. Lock `read_state`, call `recent_files(self.max_files)`.
2. For each `(path, content)`: if `path` string is a substring of any
   preserved `Role::Tool` message's `content`, skip (already visible in the
   tail). (Heuristic; document in journal.)
3. Truncate content to `per_file_budget * 4` chars (≈ per_file_budget
   tokens), appending `\n[... truncated for compaction; use Read on the path for full text]`.
4. Accumulate until `token_budget` (chars/4) is exhausted; stop early.
5. Return one `Message::system(...)` per file, each formatted:
   ```
   [post-compact file restore: <path>]
   <content-or-truncation>
   ```
   If no files qualify, return an empty `Vec`.

### 3. `src/compact/mod.rs` — register

`pub mod reinject;` + `pub use reinject::FileReinjector;`.

### 4. `src/runtime.rs` — wire cross-turn

- Add field `file_reinjector: Option<FileReinjector>` to `AgentRuntime`,
  init `None`; builder setter `pub fn file_reinjector(mut self, r: FileReinjector) -> Self`.
- In `maybe_compact_cross_turn`, after `apply_to_transcript` returns
  `Some((removed, summary_chars))` and BEFORE the `PostCompact` hook
  dispatch (`runtime.rs:401`), insert the reinjected attachments into the
  transcript immediately after the summary (index 1..) and emit a
  `MessageAppended` event for each:
  ```rust
  if let Some(r) = &self.file_reinjector {
      let recent = self.transcript[1..].to_vec(); // preserved tail (post-drain)
      let atts = r.reinject(&recent);
      let insert_at = 1;
      for att in atts {
          Arc::make_mut(&mut self.transcript).insert(insert_at, att.clone());
          self.event_sink.emit(AgentEvent::MessageAppended {
              message: att, usage: None,
          }).await;
      }
  }
  ```
  (Adjust the preserved-slice extraction to the actual post-drain indices;
  the summary is at index 0, preserved tail follows. Take care to compute
  the slice before mutating.)
- Do NOT wire into `compact_on_overflow` or `compact_now` in this goal
  (follow-up); those paths can reinject later. Cross-turn is the main growth
  site.

### 5. Builder wiring (`crates/recursive-cli/src/cli/builder.rs`)

`build_runtime` already creates `let read_state = Arc::new(Mutex::new(ReadFileState::new()));`
(`builder.rs:34`) and passes clones to the tools. After constructing the
`Compactor`, construct a `FileReinjector` from env:
- `RECURSIVE_REINJECT_FILES` (`0`/`off`/`false` = disabled → `None`; unset =
  default 5; positive = explicit count).
- `RECURSIVE_REINJECT_FILE_BUDGET` (unset = 50_000).
- per_file_budget fixed 5_000 (not env-tunable in v1).
Use the SAME `read_state` Arc (clone it) so the reinjector sees what Read
just recorded. Put env-parse in `build_file_reinjector_from_env(...)` helper
for unit testing (no env races). Pass via `.file_reinjector(...)`.

Mirror in `crates/recursive-tui/src/runtime_builder.rs`.

### 6. Tests

`src/compact/reinject.rs`:
- `reinject_returns_recent_files_as_system_messages`
- `reinject_respects_max_files`
- `reinject_respects_token_budget` — two large files, budget fits one → only
  one returned.
- `reinject_truncates_oversized_file` — content > per_file_budget → truncated
  with marker.
- `reinject_dedups_against_preserved_tail` — a file whose path appears in a
  preserved `Role::Tool` content is skipped.
- `reinject_empty_when_no_files` — empty `ReadFileState` → empty Vec.
- `build_file_reinjector_from_env` — disabled/explicit/unset semantics
  (one sequential test).

`src/runtime.rs`:
- `cross_turn_compaction_reinjects_files_after_summary` — seed a transcript,
  record files in `ReadFileState`, trigger compaction, assert the transcript
  is `[summary, <file attachments>, ...recent]` and each attachment is
  `Role::System`.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- Reinject only emits `Role::System` messages (no `Role::Tool`) → no orphan
  possible; `tests/invariants/tool_call_pairing.rs` green.
- `RECURSIVE_REINJECT_FILES=0` → no reinjector, behavior identical to today.
- Reinjector uses the same `read_state` Arc as the Read tool (verified by
  the runtime test recording via the shared state).

## Notes for the agent

- The dedup is a **heuristic** (path substring in preserved Tool content).
  It may false-negative (path mentioned in non-Read output) but never
  false-positive-harmful (worst case: a file is re-injected that was already
  visible → minor redundancy, same as no dedup). Document this in the
  journal; a precise `tool_call_id → path` dedup is a follow-up.
- `recent_files` is `&self`; the caller holds the `Mutex` guard. Do NOT add
  an interior `Mutex` inside `ReadFileState` — it is already wrapped in
  `Arc<Mutex<>>` by the tools.
- The reinjected attachments are `Role::System`. Confirm the provider
  accepts a `System` message mid-transcript (after the summary `System`
  message). OpenAI/Anthropic accept multiple System messages; if a provider
  rejects it, fall back to `Role::User` with a `[system]` prefix — note the
  fallback in the journal and prefer `System` unless a test fails.
- **DO NOT modify** `src/run_core.rs` (intra-turn reinject is a follow-up),
  `src/llm/`, `src/kernel.rs`, `compact_on_overflow`, `compact_now`, or
  tool files beyond the `recent_files` accessor.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-reinject-files.md`.

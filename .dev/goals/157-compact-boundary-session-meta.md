# Goal 157 — Compact boundary markers + enriched session metadata

> **Roadmap**: Phase 18.6 — Transcript fidelity (part 3: resume efficiency)
> **Design principle check**:
> - **No new files**: all changes are within the existing JSONL and
>   `.meta.json` files.
> - **Backward-compatible**: `SessionReader` already skips
>   unrecognized `type` values; old files load unchanged.
> - **Faster resume**: `compact_boundary` lets the reader skip the
>   pre-compaction segment without reading it; `first_prompt` /
>   `last_prompt` in `.meta.json` enable session-picker display
>   without reading the JSONL at all.
> - **Depends on**: g152 (incremental writes); can be done in
>   parallel with g155 / g156.

## Why

### Problem 1 — Resume reads the entire JSONL even for compacted sessions

When `recursive resume <id>` opens a session, `SessionReader` reads
every line of `transcript.jsonl` and passes ALL messages to the
runtime. For long sessions that have been compacted, this means
re-reading and re-parsing the pre-compaction history—messages that
the LLM will never see (they were replaced by the compaction
summary).

Claude Code solves this with a `compact_boundary` system entry:

```json
{"type":"system","subtype":"compact_boundary","sessionId":"…"}
```

When the reader sees this marker, it **discards** everything before
it and starts fresh, keeping only the post-boundary segment. This
reduces peak memory and parse time proportionally to how many times
the session has been compacted.

Currently, Recursive writes the compaction summary message as a
regular `user`-role message with a `[compacted: N messages → M
chars]` prefix. There is no machine-readable boundary marker, so the
reader cannot efficiently skip the pre-compaction history.

### Problem 2 — Session picker requires reading the JSONL

`recursive sessions list` (and future TUI session picker) needs a
human-readable summary for each session. Today the only summary
available is from `.meta.json::goal`, which is a static string set
at session creation time—not what the user actually typed.

Claude Code keeps a `"lastPrompt"` field in a dedicated `last-prompt`
JSONL entry (appended after every user turn), and reads only the
file's head/tail (64KB each) to extract it without full parsing.

Our `.meta.json` has no `first_prompt` or `last_prompt` field at
all. The session picker would have to read the whole JSONL.

## What this goal does

### 1. Write a `compact_boundary` entry after compaction

In `AgentRuntime::run`, when compaction fires and a summary message
is inserted, immediately after the summary's `MessageAppended` emit,
write a system entry to mark the boundary:

```json
{
  "type": "system",
  "subtype": "compact_boundary",
  "turn": 7,
  "compacted_count": 42,
  "summary_uuid": "<uuid of the compaction summary message>"
}
```

This is written **directly** by the `SessionPersistenceSink` when it
receives a new event type `CompactionBoundary`:

```rust
// In AgentEvent:
CompactionBoundary {
    turn: u32,
    compacted_count: usize,
    summary_uuid: String,
},
```

`SessionPersistenceSink::emit` handles this event by writing the
system entry directly to the JSONL file (no `Message` involved—this
is a metadata record, not an LLM message).

### 2. Update `SessionReader` to use `compact_boundary`

`SessionReader::load_transcript` scans for the **last**
`compact_boundary` entry. If found:
- Discard all lines before (and including) the boundary entry.
- Start the returned `Vec<Message>` from the line immediately after.

This makes resume `O(post-compaction size)` instead of
`O(total file size)`.

Add a `CompactBoundaryOpts` enum to let callers override if needed:

```rust
pub enum CompactBoundaryBehavior {
    /// Default: skip pre-boundary messages (efficient resume).
    Skip,
    /// Load everything (e.g., for a "show full history" view).
    IncludeAll,
}
```

### 3. Track `first_prompt` and `last_prompt` in `.meta.json`

`SessionMeta` gains two new fields:

```rust
pub struct SessionMeta {
    // …existing fields…
    /// First meaningful user message, truncated to 200 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Most recent user message, truncated to 200 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
}
```

`SessionWriter::append` updates these fields whenever it appends a
`user`-role message:
- `first_prompt`: set only once (when `message_count == 1` and
  role is user).
- `last_prompt`: updated on every user message.

Both are written to `.meta.json` on every user message append (not
just on `finalize`). This ensures they survive a crash mid-session.

### 4. Update `sessions list` to use `.meta.json` summary

`recursive sessions list` currently shows the `goal` field.  
Update it to prefer `last_prompt` → `first_prompt` → `goal` as the
display summary (matching Claude Code's picker priority order).

### 5. `compact_boundary` in `SessionReader::scan_orphan_tool_calls`

`scan_orphan_tool_calls` (g153) should respect the compact boundary:
it only needs to scan the post-boundary segment to find orphan tool
calls (pre-boundary messages were already resolved).

## Scope

### In scope

- `AgentEvent::CompactionBoundary` event.
- System entry writing in `SessionPersistenceSink`.
- `SessionReader::load_transcript` boundary-aware loading.
- `first_prompt` / `last_prompt` fields in `SessionMeta`.
- Update `SessionWriter::append` to write `.meta.json` on user
  messages.
- Update `recursive sessions list` display logic.
- Unit tests for boundary skipping, `first_prompt` / `last_prompt`
  updates.
- Integration test: compact a session, resume it, verify only
  post-boundary messages are loaded.

### Out of scope

- A "show full history" command (can come later).
- Head/tail file reading optimization (our sessions are small enough
  that full-file reads are fine for now).
- Any TUI changes beyond what `sessions list` requires.

## Files to change

| File | Change |
|------|--------|
| `src/event.rs` | Add `CompactionBoundary` variant to `AgentEvent` |
| `src/session.rs` | Write system entry in `SessionPersistenceSink`; boundary-aware loading in `SessionReader`; `first_prompt`/`last_prompt` in `SessionMeta` |
| `src/runtime.rs` | Emit `CompactionBoundary` after compaction summary |
| `src/main.rs` | Update `sessions list` display to prefer `last_prompt` |
| `tests/compact_boundary.rs` | New integration tests |

## Acceptance criteria

- [ ] After compaction, `transcript.jsonl` contains a
      `{"type":"system","subtype":"compact_boundary"}` line.
- [ ] `SessionReader::load_transcript` with default behavior returns
      only post-boundary messages; the pre-boundary segment is not
      allocated.
- [ ] A session's `.meta.json` contains `first_prompt` and
      `last_prompt` after at least one user turn completes.
- [ ] `first_prompt` / `last_prompt` are present in `.meta.json`
      even if the session crashes before `finalize` is called.
- [ ] `recursive sessions list` displays `last_prompt` as the
      session summary where available.
- [ ] Old JSONL files without `compact_boundary` entries load
      correctly (no boundary found → load everything).
- [ ] `cargo test --all-targets` green; `cargo clippy -D warnings`
      green.

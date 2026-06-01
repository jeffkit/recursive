# Goal 155 — UUID-per-message chain + subagent message association

> **Roadmap**: Phase 18.6 — Transcript fidelity (part 1: message identity)
> **Design principle check**:
> - **Orthogonal**: `TranscriptEntry` gains a stable UUID field and a
>   `parent_uuid` pointer. No agent loop changes, no LLM-wire changes.
> - **Backward-compatible**: old JSONL files (sequential `msg_001`)
>   remain loadable; the reader falls back gracefully when `uuid` is
>   absent.
> - **Enables subagent message trees**: once every entry has a UUID,
>   a subagent can stamp its messages with the parent agent's last
>   message UUID as `parent_uuid`, creating a branching conversation
>   tree rather than a flat list.
> - **Depends on**: g152 (incremental transcript writes via
>   `SessionPersistenceSink`).

## Why

The current `TranscriptEntry` uses auto-incremented IDs (`msg_001`,
`msg_002`, …) with a sequential `parent_id` that always points to
the immediately preceding message. This design has two critical
limitations:

### 1. Cannot represent branching conversation trees

When a subagent runs inside a parent agent's turn, its messages
belong to the **same JSONL file** (one session = one file) but they
form a **branch** off the parent's message tree:

```
user (msg_001)
  └─ assistant: "let me delegate to subagent" (msg_002)
       ├─ subagent/user: "task from parent" (msg_003, branch A)
       │    └─ subagent/assistant: "result" (msg_004, branch A)
       └─ assistant: "subagent done, summarizing" (msg_005, main chain)
```

Sequential IDs cannot represent the fork: `msg_003`'s
`parent_id = "msg_002"` conflicts with `msg_005`'s same claim. One of
them gets the wrong parent.

### 2. Cannot correctly attribute tool results

Claude Code's JSONL uses `sourceToolAssistantUUID` on tool-result
(user) messages to point directly at the assistant message that
issued the tool call—not just the immediately preceding message. This
matters when multiple tool calls are in-flight concurrently or when
a tool result arrives out-of-order (e.g., from a background task).

Our current model always assigns `parent_id = msg_N-1`, which is
wrong whenever a tool result logically belongs to a non-adjacent
assistant message.

### 3. Cannot resume into the middle of a branching turn

`SessionReader::scan_orphan_tool_calls` (g153) needs to find "the
last assistant message that issued tool calls." In a branching tree,
"last" is ambiguous without a proper parent pointer—the reader must
walk the tree, not scan lines in order.

## What this goal does

### 1. Add `uuid` and `parent_uuid` to `TranscriptEntry`

```rust
pub struct TranscriptEntry {
    pub uuid: String,               // NEW: stable per-message UUID (uuid v4)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>, // NEW: UUID of parent message (null = root)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool_assistant_uuid: Option<String>, // NEW: for tool results

    // Keep existing fields for backward compat
    pub id: String,                 // kept: "msg_001" still written
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,  // kept: still written for old readers

    pub role: String,
    pub content: String,
    // …other fields unchanged…
}
```

### 2. Update `SessionWriter::append` to generate UUIDs

`append` generates a UUID v4 for each message and tracks
`last_uuid: Option<String>` to populate `parent_uuid` for the
next message in the main chain.

For subagent messages, `append` accepts an optional
`parent_uuid_override: Option<&str>` parameter so callers can
specify an explicit parent (e.g., the parent agent's last message
UUID).

```rust
pub fn append(
    &mut self,
    msg: &Message,
    parent_uuid_override: Option<&str>,
) -> std::io::Result<String> { … }
```

The returned `String` is the new message's UUID (changed from
returning `msg_id`).

### 3. Update `SessionPersistenceSink` and `AgentEvent`

`AgentEvent::MessageAppended` carries the UUID of the message's
parent (the previous message in the current chain). For subagent
events, the event carries the parent agent's last UUID.

```rust
MessageAppended {
    message: crate::message::Message,
    parent_uuid: Option<String>,  // NEW
},
```

### 4. Update `SessionReader::load_transcript`

The reader builds a `HashMap<uuid, TranscriptEntry>` (UUID-indexed)
in addition to the existing `Vec<Message>` output. The UUID index
is used by `scan_orphan_tool_calls` (g153) to correctly identify
the last assistant message in the main chain (not a subagent branch).

### 5. Sidechain (subagent) support in `AgentRuntime`

`AgentRuntimeBuilder` gains an optional `parent_agent_uuid` field.
When set, the runtime stamps all messages with
`parent_uuid = parent_agent_uuid`'s last message UUID, creating a
sidechain in the JSONL.

```rust
let sub_rt = AgentRuntime::builder()
    .llm(llm)
    .parent_session_writer(writer.clone())  // shares parent's JSONL
    .parent_agent_last_uuid("abc-123")      // branch root
    .build()?;
```

## Scope (do exactly this, no more)

### In scope

- Add `uuid`, `parent_uuid`, `source_tool_assistant_uuid` to
  `TranscriptEntry`.
- Keep `id` / `parent_id` as redundant output (written alongside
  UUID fields for backward compat, not used by new code).
- Update `SessionWriter::append` signature.
- Update `SessionPersistenceSink` to pass `parent_uuid` through.
- Update `SessionReader::load_transcript` to build UUID index.
- Add `AgentRuntimeBuilder::parent_agent_last_uuid` (stored, not yet
  wired to a real multi-agent orchestration path — that comes in a
  later goal).
- Unit tests for UUID chain correctness and sidechain branching.
- Integration test: two `AgentRuntime` instances sharing a
  `SessionWriter`, verify JSONL contains interleaved messages with
  correct `parent_uuid` pointers.

### Out of scope

- Full multi-agent orchestration (subagent spawning from tool calls).
- Content-replacement / compaction of sidechain messages.
- Any changes to the HTTP API or TUI.
- Removing `id` / `parent_id` from `TranscriptEntry` (keep for compat).

## Files to change

| File | Change |
|------|--------|
| `src/session.rs` | Add UUID fields to `TranscriptEntry`; update `SessionWriter::append`; update `SessionReader::load_transcript` |
| `src/event.rs` | Add `parent_uuid: Option<String>` to `MessageAppended` variant |
| `src/runtime.rs` | Pass `parent_uuid` when emitting `MessageAppended`; add `AgentRuntimeBuilder::parent_agent_last_uuid` |
| `tests/uuid_chain.rs` | New integration tests |

## Acceptance criteria

- [x] Every new `TranscriptEntry` in the JSONL has a non-empty `uuid`
      field (UUID v4 format).
- [x] `parent_uuid` of each entry (except root) equals the `uuid` of
      the preceding entry in the same chain.
- [x] A subagent sharing a `SessionWriter` produces entries whose
      `parent_uuid` correctly points to the parent agent's last
      entry UUID, not to each other.
- [x] `SessionReader::load_transcript_indexed` returns a `uuid_index`
      allowing O(1) lookup by UUID.
- [x] Old JSONL files without `uuid` fields load without error
      (missing UUID fields deserialized as empty string or None).
- [x] `cargo test --all-targets` green; `cargo clippy -D warnings`
      green.

## Status: COMPLETED (2026-06-01)

Implemented in commits `a875240` – `862f6cb`. All 6 acceptance criteria
verified by `tests/uuid_chain.rs` (6 tests, all green).

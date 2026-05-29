# Goal 152 — Incremental transcript writes during agent runs

> **Roadmap**: Phase 18.5 — Long-running goals (prerequisite for
> g153 orphan detection)
> **Design principle check**:
> - Single seam: route every "completed message" through one
>   `EventSink` event variant. No new persistence file, no schema
>   change to `transcript.jsonl`.
> - Reuses existing infrastructure: `AgentEvent` /
>   `EventSink` / `CompositeSink` already exist (g123); we add one
>   variant and one sink implementation.
> - No agent-loop semantic change. The runtime decides what counts
>   as a complete message exactly the way it does today; we just
>   tap the same point for a side-effecting write.
> - **Depends on**: nothing strictly — but it is a hard prerequisite
>   for g153's orphan detection to have any input data.

## Why

Today `transcript.jsonl` is written **once, after `runtime.run()`
returns**, by a batch loop in `main.rs`:

```rust
// src/main.rs:1648-1654 (Cmd::Resume) and 1937-1943 (Cmd::Run)
let outcome = runtime.run(goal.clone()).await?;
if let Some(ref sw) = session_writer {
    if let Ok(mut w) = sw.lock() {
        for msg in runtime.transcript().iter().skip(pre_transcript_len) {
            let _ = w.append(msg);
        }
    }
}
```

This is a "completed-or-bust" persistence model:

- One turn can run for tens of minutes and call hundreds of tools
  inside `RunCore::run_inner` (`src/agent.rs`).
- During that time, every `assistant` and `tool` message is pushed
  into an **in-memory** `Vec<Message>` — nothing reaches disk.
- If the process is killed (`SIGKILL`, OOM, `panic!`, power
  loss, user `Ctrl-C` not caught), `transcript.jsonl` keeps its
  state from the **previous** clean run. The current run's work
  vanishes.
- After the crash, `recursive sessions show <id>` and
  `recursive resume <id>` (g151) can only see the last clean
  state; for a 4-hour run that died at minute 230, that is useless.

There is a second, structural consequence: **g153's orphan
detection has no input** in the current architecture. Its design
rests on observing the shape "last assistant message has tool
calls, but the trailing tool result is missing" — but that shape
never lands on disk today, because the assistant message and its
tool results are always written together (or not at all).

Goal 141/142's per-turn workspace checkpoints are unrelated: they
snapshot **filesystem state** via a shadow git repo, not transcript
state. The two persistence layers live in parallel and neither
covers the other.

## Scope (do exactly this, no more)

### What this goal does

1. Add `AgentEvent::MessageAppended { message: Message }` — fired
   the instant `RunCore` (and `AgentRuntime` for the seed user
   message) commit a message to the transcript.
2. Add a new `EventSink` implementation `SessionPersistenceSink`
   that wraps `Arc<Mutex<SessionWriter>>` and calls
   `writer.append(&msg)` on every `MessageAppended` event.
3. Wire `SessionPersistenceSink` into the runtime via the existing
   `CompositeSink` mechanism (`src/event.rs`), so streaming /
   JSON-mode / TUI sinks coexist with persistence on the same
   event bus.
4. Delete the batch `for msg in ...append(msg)` loops in
   `src/main.rs` (two sites: `run_resumed` and `run_run`).
5. Adjust `finalize_session_writer` to do **only** the final
   `.meta.json` status update (`success` / `incomplete` /
   `interrupted`) — it no longer needs to flush messages, those
   are already on disk.

### What this goal does **not** do

- Does **not** persist event-level data (`PartialToken`, `Latency`,
  `Usage`, `Compacted`, `ToolCall`, `ToolResult`). Those are
  ephemeral UI/observability events; the canonical record is the
  `Message` itself.
- Does **not** write streaming chunks. Streaming `PartialToken`
  events fire many times per LLM call; only the final assembled
  assistant message (committed via `push_message`) lands in jsonl.
- Does **not** change the `TranscriptEntry` schema. A message that
  carries both `content` and `reasoning_content` continues to be
  one jsonl line with two fields; it is **not** split into two
  lines. Splitting reasoning into a separate row is a schema
  decision that would ripple into `load_messages`, exporters,
  audit, and replay — out of scope here.
- Does **not** add `fsync`. `SessionWriter::append` already calls
  `flush()` on the `BufWriter` (`src/session.rs:377`), which
  pushes the line into the kernel page cache. Surviving a power
  cut would need `fsync` on the file handle plus `fsync` on the
  parent directory; that is a separate goal with its own
  performance trade-offs.
- Does **not** revive `OnMessageFn` (`src/agent.rs:60`, deprecated
  in favour of `EventSink`). We extend the `EventSink` path
  instead.

## Architecture

### Where messages are committed today

Six call sites all funnel through one helper, `RunCore::push_message`
(`src/agent.rs:302`):

| line | site |
|------|------|
| `agent.rs:650` | `tool_result` after a successful tool call |
| `agent.rs:701` | `assistant` text-only message |
| `agent.rs:728` | `assistant` with tool_calls |
| `agent.rs:842` | `tool_result` for plan-buffered call |
| `agent.rs:902` | `tool_result` for cancelled / plan-rejected call |
| `agent.rs:1080` | initial `user` message at run start |

`Agent::push_message` (`src/agent.rs:1260`) is the same pattern at
the wrapper level. `AgentRuntime::run_turn` also pushes a user
message directly (`src/runtime.rs:164`); we route this through the
sink too.

This concentration is what makes g152 small: one centralised
emit-on-push, plus one sink that calls `writer.append`.

### `AgentEvent::MessageAppended`

```rust
// src/event.rs

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    // ...existing variants...

    /// A complete message was just appended to the agent transcript.
    /// Carries the full `Message` (role, content, tool_calls,
    /// tool_call_id, reasoning_content) so consumers can persist or
    /// project the whole record without reassembling it from finer
    /// `AssistantText` / `ToolCall` / `ToolResult` events.
    ///
    /// Fired exactly once per `transcript.push(msg)` inside the
    /// agent kernel and runtime.
    MessageAppended { message: crate::message::Message },
}
```

`Message` is already `Serialize + Deserialize + Clone` (see
`src/message.rs:21`), so the new variant fits the existing
`AgentEvent` derives.

This variant **is not redundant** with `AssistantText` /
`ToolCall` / `ToolResult`:

- The fine-grained variants are designed for UI streaming
  (per-step rendering, partial tokens). They omit fields the wire
  format needs (e.g. `AssistantText` has no `tool_calls`,
  `ToolCall` has no `tool_call_id`-bearing reply).
- `MessageAppended` carries the entire `Message`, which is what a
  persistence layer or a future replay layer needs.
- Existing consumers (TUI, JSON-mode CLI) ignore the new variant
  by default thanks to `#[non_exhaustive]`.

### `SessionPersistenceSink`

```rust
// src/session.rs (or src/event.rs — implementation detail)

pub struct SessionPersistenceSink {
    writer: Arc<Mutex<SessionWriter>>,
}

impl SessionPersistenceSink {
    pub fn new(writer: Arc<Mutex<SessionWriter>>) -> Self {
        Self { writer }
    }
}

#[async_trait]
impl EventSink for SessionPersistenceSink {
    async fn emit(&self, event: AgentEvent) {
        if let AgentEvent::MessageAppended { message } = event {
            let result = {
                // Hold the mutex only across the append call.
                match self.writer.lock() {
                    Ok(mut w) => w.append(&message),
                    Err(poisoned) => {
                        // Recover from poisoned lock; logging only,
                        // do not panic.
                        let mut w = poisoned.into_inner();
                        w.append(&message)
                    }
                }
            };
            if let Err(e) = result {
                // Persistence failure is non-fatal for the run, but
                // visible: a missing line on disk would silently
                // break g153 orphan detection.
                tracing::error!(
                    "session persistence: failed to append message: {e}"
                );
            }
        }
    }
}
```

Locking notes:

- `SessionWriter` is not `Send` across `.await` points, so we keep
  the mutex non-`async` (`std::sync::Mutex`). The `emit()` call
  is itself `async`, but the critical section inside is purely
  synchronous IO — that matches every other consumer of
  `Arc<Mutex<SessionWriter>>` in `main.rs` today.
- Contention is bounded: at most one writer task per session, with
  one `append` per pushed message. A turn that pushes 400 messages
  performs 400 short critical sections sequentially, each ~1 jsonl
  line of `serde_json::to_string` + `write_all` + `flush`. This
  is well below LLM and tool latencies.

### Wiring in main.rs

The runtime already accepts an `Arc<dyn EventSink>` via the
`build_runtime` helper (the JSON-mode and stream-events paths
both use it). g152 changes the `Some(Arc::new(sink))` argument to
a `CompositeSink` carrying both the existing UI sink and the new
persistence sink:

```rust
// In run_run / run_resumed (src/main.rs)

let ui_sink: Arc<dyn EventSink> = if json_mode {
    Arc::new(JsonEventSink::new(event_tx))
} else {
    Arc::new(StreamEventSink::new(event_tx))
};

let event_sink: Arc<dyn EventSink> = match &session_writer {
    Some(sw) => Arc::new(CompositeSink::new(vec![
        ui_sink,
        Arc::new(SessionPersistenceSink::new(sw.clone())),
    ])),
    None => ui_sink,
};

let mut runtime = build_runtime(
    &config,
    /* ... */
    Some(event_sink),
    Some(shutdown.clone()),
).await?;
```

`CompositeSink` (`src/event.rs`) already exists for fan-out and
preserves event ordering across inner sinks.

### Removing the batch loops

After wiring the persistence sink:

```rust
// DELETE — src/main.rs:1648-1654 (run_resumed)
if let Some(ref sw) = session_writer {
    if let Ok(mut w) = sw.lock() {
        for msg in runtime.transcript().iter().skip(pre_transcript_len) {
            let _ = w.append(msg);
        }
    }
}

// DELETE — src/main.rs:1937-1943 (run_run)
// (identical pattern)
```

These loops are not just redundant after g152; they would
**double-write** every message (once via the sink, once via the
batch). Their removal is part of the goal, not optional.

`pre_transcript_len` was only used to slice the unwritten tail;
remove it as well unless another caller still needs it (it does
not — `grep -n pre_transcript_len src/main.rs` shows two sites,
both in the loops we're deleting).

### Status field semantics

`SessionMeta.status` (`src/session.rs:218`) currently transitions:

- Created as `"active"` in `SessionWriter::create`.
- Set to `"success"` / `"incomplete"` / `"interrupted"` in
  `finalize_session_writer` (`src/main.rs:1503-1517`) at clean
  shutdown.

After g152, a process that crashes mid-run leaves
`status: "active"` (the meta file's value at session creation
time). g151's "most-recent shortcut" is documented to prefer
sessions with status ∈ `{active, interrupted}`; the `active` value
on a crashed session is therefore exactly what makes
`recursive resume` (no arg) pick the right thing.

We update the docstring of `SessionMeta::status` to reflect this:

```rust
/// Lifecycle state of the session record.
///
/// - `"active"`: writer was created but `finish` has not been
///   called. This covers both *currently-running* and
///   *crashed-before-finish* sessions; the two are
///   indistinguishable from the meta file alone (use
///   `<session_dir>/.lock` from g151 to disambiguate).
/// - `"success"`: agent run completed with `NoMoreToolCalls`.
/// - `"incomplete"`: ran to a non-success terminal state
///   (budget exceeded, transcript limit, etc.).
/// - `"interrupted"`: cancelled via shutdown signal.
pub status: String,
```

### Compaction and transcript truncation

Two existing operations mutate the in-memory transcript without
going through `push_message`:

1. **Cross-turn compaction** (`src/runtime.rs:168-184`): drains a
   prefix of `self.transcript` and inserts a `summary_msg`. The
   transcript on disk does **not** mirror this in-place edit
   today — the jsonl is append-only and stays the full history.
   g152 preserves that contract: compaction does not retroactively
   rewrite jsonl. The summary message **is** pushed via
   `push_message` and so reaches disk via the sink; the
   pre-compaction history stays in jsonl as historical context.
2. **`recursive sessions rewind`** (`src/session.rs:430`,
   `truncate_transcript_to_turn`): explicitly rewrites the jsonl
   by truncating to a turn boundary. This is an out-of-band
   admin operation; g152 does not affect it.

These two cases are documented inline in the new sink's module
doc comment so future readers don't try to "fix" the apparent
asymmetry between memory and disk.

## Tests

Unit (`src/event.rs` or `src/session.rs`):

- `message_appended_round_trips_through_sink` — push a `Message`
  with content + tool_calls + reasoning_content into a
  `SessionPersistenceSink`, reload the jsonl, assert all three
  fields survived.
- `sink_recovers_from_poisoned_mutex` — poison the writer mutex
  (deliberately panic in another thread while holding it),
  subsequent `emit` still appends and logs an error rather than
  panicking the runtime.
- `composite_sink_preserves_message_appended` — composite of UI +
  persistence sinks: persistence sink sees the message, UI sink
  sees it too, and other event variants reach both.

Integration (`tests/incremental_writes.rs`, new):

- `kill_mid_turn_persists_messages_so_far` — start an agent run
  whose first action is a controllable pause (a fake tool that
  blocks on a tokio `Notify`); after two messages have been
  emitted, kill the run via the shutdown token. Reload the
  session by id (using g151), assert `transcript.jsonl` contains
  exactly the two messages — not zero, not the full intended
  turn.
- `assistant_with_reasoning_and_tool_calls_one_line` — an
  assistant message with all three of `content`,
  `reasoning_content`, and `tool_calls` produces a single jsonl
  line with all three fields populated.
- `streaming_partial_tokens_dont_persist` — run with streaming
  enabled; assert `transcript.jsonl` line count equals message
  count (one per assistant + one per tool result), not chunk
  count.
- `compaction_summary_appears_in_jsonl` — force compaction during
  a long run; assert the synthesised summary message is present
  in jsonl, and that pre-compaction lines are also still present
  (i.e. compaction does not retroactively rewrite the file).
- `resume_after_clean_run_no_double_write` — run a session to
  clean completion, reload, count jsonl lines; resume, run another
  turn, reload — no message id is duplicated, message count grows
  by exactly the number of new messages.
- `resume_after_crash_orphan_visible` — set up a session whose
  last on-disk message is an `assistant` with `tool_calls` (no
  matching `tool` reply). Show that this **shape** survives
  round-trip via `SessionReader::load_transcript`. (This test
  asserts only that the jsonl can carry the orphan shape — the
  detection logic is g153.)

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 9 new tests (3 unit + 6 integration)
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No new external crate dependencies. (Reuses existing `EventSink`,
  `CompositeSink`, `Message`, `SessionWriter`.)
- Manual smoke:
  ```
  recursive run "<goal that triggers a long-running shell>"
  # in another shell, while it runs:
  cat ~/.recursive/.../transcript.jsonl | wc -l   # >0, growing
  kill -9 <pid>
  cat ~/.recursive/.../transcript.jsonl | wc -l   # final count > 0
  recursive sessions show <id>                    # shows partial run
  ```

## Out of scope (deferred)

- **`fsync` for power-cut safety**. Today `BufWriter::flush()`
  pushes data to the OS page cache; surviving a power cut
  additionally needs `File::sync_data()` per line plus a directory
  sync after the meta-json rewrite. Adds visible latency per
  message; trade-off worth a dedicated goal once we have evidence
  someone needs it.
- **Event-level persistence** (writing `PartialToken` / `Latency` /
  `ToolCall` / `Usage` to disk for full replay or UI scrubbing).
  Would require a parallel events-file format, schema decisions,
  and a separate read path. Different goal.
- **Splitting reasoning into its own jsonl line** (rationale:
  separable archival, separate compaction policy, separate audit).
  Possible but ripples through `TranscriptEntry`, `Message`,
  `entry_to_message` (g151), exporters, audit (g153), replay
  (g154), and the LLM wire-shape conversion. Different goal if
  ever needed.
- **Migrating in-flight `OnMessageFn` consumers** to the new
  variant. There are no such consumers in the tree
  (`grep -rn '\.on_message(' src/ tests/` is empty), so the
  deprecation path stays as-is — this goal does not delete
  `OnMessageFn`.
- **Buffering / coalescing writes** for high-throughput tool
  loops. The current single-writer-per-session model handles the
  common case; if a future workload pushes thousands of messages
  per second per session, revisit with profiler data.

## Why this is the right granularity

The choice between "write every event" and "write every completed
message" matters because of what the downstream layers want:

- **g153 orphan detection** wants a clean shape: an assistant
  message with `tool_calls`, optionally followed by matching tool
  messages. That is exactly the message-level granularity.
  Half-written assistant text would force orphan detection to
  reason about partial messages — a much harder problem.
- **g154 safe replay** uses `args_hash` over canonical-JSON tool
  call arguments. Args only become final at message-commit time;
  an event-level stream would force the replay logic to
  re-assemble messages from chunks before hashing.
- **`recursive sessions show / export`** all consume
  `TranscriptEntry`, which is a 1:1 with `Message`. Writing at
  message granularity preserves this isomorphism end-to-end.

In short: the message is already the atomic unit of the agent's
contract with the LLM (one role + one content + one set of tool
calls). Persistence at any other granularity would create a
schema we'd have to translate back at every read site. Goal 152
keeps the persistence story trivially aligned with the runtime
story.

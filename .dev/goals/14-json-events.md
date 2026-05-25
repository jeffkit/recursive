# Goal 14 — JSON event output via `--json`

## What

Add a top-level `--json` CLI flag. When set, all `StepEvent`s emitted
during `run` (and `repl`, optionally) are serialised as newline-
delimited JSON to stdout instead of the current human-readable
format. Each line is one event.

This unlocks programmatic consumption (`recursive run --json … | jq`)
and makes it trivial for external tools (CI, log aggregators, web
UI) to follow a run.

## Why

Goal 08 saved completed transcripts. Goal 09 pretty-printed them.
What's missing is **structured access to a run as it happens** — and
for a tool that aims to be embeddable, that's a real gap. Without
this, a wrapper script is stuck regexing our human-readable output,
which we keep changing.

JSON-streamed events also become the data feed for whatever
observation tools we build later. Today's `observe.sh` greps a
free-text journal; if event JSON existed it could read the
structured stream directly.

## Scope (do exactly this, no more)

### 1. `src/agent.rs`

Add `Serialize` (and `Deserialize` for symmetry; cheap) to the
event/finish types:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepEvent { … }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinishReason { … }
```

Use `#[serde(tag = "kind")]` so external readers can dispatch on the
variant name. `ToolCall` and `TokenUsage` are already serialisable
via their existing derives — no change there.

Don't change any other public API on these types. Existing matches
and field names stay.

### 2. `src/main.rs`

Add a top-level flag on the `Cli` struct:

```rust
/// Emit StepEvents as newline-delimited JSON on stdout instead of
/// the human-readable trace. Useful for piping into jq or other
/// downstream tooling.
#[arg(long, env = "RECURSIVE_JSON")]
json: bool,
```

Thread `cli.json` into `run_once(...)` (alongside the existing
`max_transcript_chars` and `transcript_out`). Inside `run_once`,
replace the current `stream_events(rx)` call with a branch:

```rust
let printer = if json_mode {
    tokio::spawn(stream_events_json(rx))
} else {
    tokio::spawn(stream_events(rx))
};
```

Add a sibling helper:

```rust
async fn stream_events_json(mut rx: mpsc::UnboundedReceiver<StepEvent>) {
    while let Some(ev) = rx.recv().await {
        match serde_json::to_string(&ev) {
            Ok(line) => println!("{line}"),
            Err(_)   => {/* drop unserialisable events silently */}
        }
    }
}
```

In `--json` mode, **also** silence the post-run "=== final ===",
`tokens: …`, `cost: …`, and `note: stopped because…` lines —
those are duplicate information already in the event stream
(`Usage`, `Finished`). The transcript-out side effect should still
fire if requested.

Mirror the flag through `repl` similarly (run loop emits JSON
instead of free text); keep the `recursive>` prompt on stderr so
users can still drive it.

### 3. Tests

In `src/agent.rs`'s test module, add:

1. `step_event_serializes_to_json` — construct an `AssistantText`
   variant, serialise, deserialise, assert equality.
2. `step_event_uses_kind_tag` — serialise a `ToolCall` variant,
   assert the JSON string contains `"kind":"tool_call"`.
3. `finish_reason_serializes_with_tag` — `FinishReason::Stuck { … }`,
   assert the JSON contains `"kind":"stuck"`.

(No `main.rs` integration tests needed — it's a thin wrapper.)

## Out of scope

- Binary / protobuf output format. JSON is universal enough.
- Streaming the `outcome.transcript` in JSON form. That's what
  `--transcript-out` is for.
- Re-using the event JSON to fully reconstruct an `AgentOutcome`.
  Outcome is a snapshot; events are a stream. Keep them distinct.
- Backfilling old captured runs into JSON. Existing logs are
  what they are.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- `recursive run --json "1+1?"` prints one JSON object per line, each
  with a `"kind"` field. The output is pipe-able to `jq -c .` without
  errors.
- `recursive run "1+1?"` still prints the human-readable trace
  unchanged.
- 3 new tests pass. No new dependencies (serde + serde_json already
  in `Cargo.toml`).

## Notes for the agent

- The serde-tag attributes are the load-bearing part. Make sure
  `#[serde(tag = "kind", rename_all = "snake_case")]` lives directly
  on the enum, not on individual variants.
- The default discriminant name "kind" is a convention — pick
  something more JSON-friendly than serde's default (which would
  produce `{"AssistantText":{…}}` rather than `{"kind":"assistant_text",…}`).
- `apply_patch` is the right tool for both `src/agent.rs` (small
  attribute add) and `src/main.rs` (CLI flag plus the dispatch
  branch). Don't rewrite either file whole.
- `mpsc::UnboundedReceiver` is in `tokio::sync` already; no new
  imports beyond `serde_json`.

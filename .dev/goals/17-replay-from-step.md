# Goal 17 — `recursive replay --resume-from <N>` to continue a saved run

## Why

Goal 08 added persistent transcripts (`--transcript-out file.json`).
Goal 09 added `recursive replay <file>` to pretty-print one. Today
that's read-only.

The natural next move is **resumable runs**: pick a saved transcript,
keep the first N messages as warm context, and let a (potentially
different) provider continue from there. Use cases:

- A run was rolled back at step 20 with budget-exceeded — replay the
  first 19 messages on a model with better step economy without
  restarting from scratch.
- Side-by-side compare two models on the same partial history.

## Scope

Touches: `src/transcript.rs`, `src/main.rs`. (Probably needs a tiny
hook in `src/agent.rs` to seed the transcript when starting.)

1. In `src/agent.rs`, add a way to construct an `Agent` with a
   pre-seeded transcript (e.g. an `AgentBuilder::seed_transcript(
   Vec<Message>)` setter, plumbed into `Agent` so `run()` starts
   with those messages already in `self.transcript`). Do **not**
   re-emit `StepEvent`s for the seeded messages; only events
   produced *during* the new run get streamed.

2. In `src/main.rs`, extend the `Replay` subcommand:
   ```
   recursive replay <transcript-file> [--resume-from <N>] [<goal>...]
   ```
   - Without `--resume-from`: keep current behavior (pretty-print).
   - With `--resume-from N` and a non-empty trailing `<goal>`:
     load the transcript, take the first N messages, build an
     agent seeded with them, then call `agent.run(goal.join(" "))`.
     Emit the new run's events with the existing
     `stream_events` / `stream_events_json` (respecting the existing
     `--json` flag).
   - If N is larger than the transcript or `goal` is empty when
     `--resume-from` is given, exit with a clear error message.

3. Add tests:
   - In `src/agent.rs`: build an `Agent` with `seed_transcript`,
     run a `MockProvider` reply, assert the final transcript
     contains both the seed messages and the new ones.
   - In `src/transcript.rs` (or `src/main.rs` if more natural):
     a round-trip test that writes a transcript, reads it back,
     and verifies "take first N" returns the expected slice.

## Acceptance

- `cargo test` green (existing 113 + ≥2 new).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- CLI help (`recursive replay --help`) shows the new `--resume-from`
  flag.

## Notes for the agent

- The existing `stream_events` / `stream_events_json` already
  handle the new run's events. Don't duplicate them.
- The transcript-file format is whatever `TranscriptFile::read_from`
  / `write_to` already produces. Don't bump the schema.
- Prefer `apply_patch` over `write_file`. This goal touches three
  files but each edit is small and surgical.
- **Don't try to verify the new flag with `cargo run | jq`** — write
  a unit test in `src/main.rs` or `src/transcript.rs` that asserts
  on the in-process behavior. See AGENTS.md "Verify behavior through
  `cargo test`" for why.

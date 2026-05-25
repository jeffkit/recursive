# Goal 32 — `recursive replay --diff A.json B.json`

## Why

When two providers run the same goal (e.g. DeepSeek vs MiniMax on
goal-23 shell-timeout), they produce two transcript JSON files with
similar structure but different decisions. Today, comparing them
means reading two pretty-printed dumps side by side. A focused diff
view — "what did A do here that B didn't?" — would speed up
characterization runs and provider A/B testing.

Add a simple message-level diff: line up `messages[i]` from each
transcript by index, print `=` if they match exactly, `≠` with the
short delta otherwise.

## Scope

Touches: `src/transcript.rs` (new `pretty_diff` fn) and
`src/main.rs` (extend `replay` cmd or add a subcommand).

1. In `src/transcript.rs`:
   - Add `pub fn pretty_diff(a: &TranscriptFile, b: &TranscriptFile)
     -> String`.
   - For each `i in 0..max(a.messages.len(), b.messages.len())`:
     - If both exist and are byte-equal: emit `"[i] = <role>:
       <one-line summary>"`.
     - If both exist but differ: emit `"[i] ≠ "` followed by two
       lines: `"  A: <role>: <one-liner>"` and
       `"  B: <role>: <one-liner>"`.
     - If only one side has a message at index `i`: emit
       `"[i] +A <role>: <one-liner>"` or `+B` accordingly.
   - Use a 60-char truncation for one-liners; preserve role for
     scanning.
   - Two unit tests in the same file:
     - Identical transcripts produce all `=` lines.
     - Transcript that diverges at index 3 shows `=`/`=`/`=`/`≠`
       with the right A/B previews.

2. In `src/main.rs`:
   - Either: extend the existing `replay` subcommand with `--diff
     <second-transcript>` (mutually exclusive with `--tail`,
     `--head`, `--resume-from`), OR add a new top-level subcommand
     `diff <A.json> <B.json>`. Pick whichever is mechanically
     simpler; the diff path doesn't need any other replay flags.
   - When invoked: load both transcripts, call `pretty_diff`,
     print to stdout. No state mutation, no LLM calls.

## Acceptance

- `cargo build` green.
- `cargo test` green (138 baseline + 2 new = 140).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- Running the binary on two real transcript files prints a sane
  diff. (No need to add an integration test — unit tests on
  `pretty_diff` are enough.)

## Notes for the agent

- This is **scoped to two files** — `transcript.rs` and `main.rs`.
  Don't touch `agent.rs`, `tools/`, or `config.rs`.
- Read `pretty()`, `pretty_tail()`, `pretty_head()` in
  `transcript.rs` first — `pretty_diff` is shaped similarly.
- The `Message` struct already derives `Eq`/`PartialEq`? If not,
  comparing by serializing each message to JSON works too and is
  cheap given how small messages are.
- Use `apply_patch` for source edits.
- `.to_string()` over `.into()` in tests.

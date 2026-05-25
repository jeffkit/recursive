# Goal 20 — `recursive replay --tail N` to inspect the end of a transcript

## Why

`recursive replay <file>` pretty-prints the entire saved transcript.
For the orchestrator's typical workflow ("what did the agent decide
in its last few steps before it stopped?") that's overkill — a 700-line
transcript dwarfs a terminal scrollback.

A `--tail N` flag that prints only the last N messages (plus a short
"...skipped M earlier messages" prefix) would close that gap. It's a
small, self-contained extension to goal-09's pretty-print path.

## Scope

Touches: `src/transcript.rs` and `src/main.rs`.

1. In `src/transcript.rs`, add a method
   `pub fn pretty_tail(&self, n: usize) -> String` that produces the
   same shape as `pretty()` but only renders the last `n` messages
   (capped at total length). If `n` is less than the total, prepend
   one line:
   `"...skipped {skipped_count} earlier messages\n\n"` followed by
   the same `=== transcript ... ===` / `saved_at:` header that
   `pretty()` produces, then only the tail.

2. In `src/main.rs`, extend `Cmd::Replay`:
   ```
   recursive replay <file> [--resume-from N <goal>...]
                           [--tail M]
   ```
   - `--tail M` is independent from `--resume-from`. If both are
     given, `--resume-from` wins (the run happens; `--tail` is
     ignored).
   - When `--tail M` is given without `--resume-from`, call
     `pretty_tail(M)` instead of `pretty()`.

3. Tests:
   - In `src/transcript.rs`: a test that builds a transcript with
     5 messages, calls `pretty_tail(2)`, and asserts the output
     contains the `"...skipped 3 earlier messages"` line and the
     content of the last 2 messages but **not** the content of the
     first 3.
   - A test for `pretty_tail(n)` where `n >= total` — output should
     be equivalent to `pretty()` (no "skipped" line).

## Acceptance

- `cargo build` green.
- `cargo test` green (115 baseline + ≥2 new = 117 total).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- `recursive replay --help` shows the new `--tail` flag.

## Notes for the agent

- The existing `Replay` variant in `Cmd` is small; you only need to
  add one new field (`tail: Option<usize>`) and an arm in the
  dispatcher.
- Use `apply_patch` for all edits. Two-file change, each edit is
  a single hunk.
- **In tests, prefer `.to_string()` over `.into()` for `Message`
  constructors** — see AGENTS.md section 5 (this exact trap caused
  goal-17 to stall).
- This goal should be a 6–10 step run.

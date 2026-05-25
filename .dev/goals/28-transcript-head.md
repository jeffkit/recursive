# Goal 28 — `recursive replay --head N`

## Why

`recursive replay --tail N` (goal-20) lets us inspect the *end* of a
saved transcript when diagnosing a Stuck/BudgetExceeded run. The
*beginning* is just as important — it shows the system prompt, the
goal, and the agent's first plan, which is often where bad decisions
get baked in. Add the symmetric `--head N` flag.

## Scope

Touches: `src/transcript.rs` (new `pretty_head` method) and
`src/main.rs` (CLI flag + dispatch in `replay` subcommand).

1. In `src/transcript.rs`:
   - Add `pub fn pretty_head(&self, n: usize) -> String` symmetric to
     the existing `pretty_tail`. If `n >= self.messages.len()`, behave
     like `pretty()`. Otherwise prefix with a line:
     `"... skipping <K> later messages\n"` where K = len - n.
   - Two unit tests in the same file:
     - `pretty_head` with `n=2` on a 5-message file: returns the
       first 2 plus the skip header showing K=3.
     - `pretty_head` with `n >= len` equals `pretty()`.

2. In `src/main.rs`:
   - Find the `replay` subcommand args struct. It already has
     `--tail <N>` (and probably `--resume-from <N>`). Add an
     analogous `--head <N>` (optional `usize`).
   - **Constraint**: `--head` and `--tail` are mutually exclusive.
     If both are set, return a clap error or an `anyhow::bail!` with
     a clear message: `"--head and --tail are mutually exclusive"`.
   - When `--head` is set, call `transcript.pretty_head(n)` instead
     of `transcript.pretty()`.

## Acceptance

- `cargo build` green.
- `cargo test` green (132 baseline + 2 new = 134).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- Read `pretty_tail` first — `pretty_head` is its mirror image.
- The CLI plumbing is small; the actual logic lives in
  `src/transcript.rs`.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- Do not touch `src/agent.rs`, `src/tools/`, or `src/config.rs`.

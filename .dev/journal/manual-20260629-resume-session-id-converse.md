# Manual edit: resume-session-id-converse

**Date**: 2026-06-29
**Goal**: Drive `recursive resume` by session id with a next-turn user
message, instead of re-injecting the saved `meta.goal`. Removes the
"goal" concept from the agent's resume path — goal stays a session
display label and a self-improve-loop (external) concept, never the
resume input. Mirrors Claude Code's model: resume = load session by
id, then send the next user message; an interrupted turn continues
via a synthetic "Continue from where you left off." message.

**Files touched**:
- `crates/recursive-cli/src/main.rs` — `Cmd::Resume` gains a
  `-p/--message` field; the `-r`/`-c` shortcuts feed `cli.prompt`
  into it; dispatch passes it to `cmd_resume`.
- `crates/recursive-cli/src/cli/resume.rs` — `cmd_resume` takes an
  optional `message` and resolves it via `resolve_resume_message`
  (explicit non-empty wins, else synthetic continue). `run_resumed`'
  s `goal` parameter is renamed `message` (it was always the
  per-turn user text); both callers updated (`cmd_resume` passes the
  resolved message; `Cmd::Replay --resume-from N <goal>` passes
  `goal.join(" ")` unchanged — that path already appended a new
  message and is what self-improve's auto-resume uses).

**Tests added**:
- `cli::resume::tests::*` (4) — `resolve_resume_message`: explicit
  wins, blank/missing → synthetic continue, non-empty whitespace
  preserved verbatim.

**Notes**:
- `meta.goal` is no longer read on the resume path; it remains a
  session label (sessions list, TUI picker, episodic recall preview,
  session writer seeding) — NOT removed (scope: small incision).
- self-improve's auto-resume uses `replay --resume-from N <goal>`,
  NOT `recursive resume <id>`, so it is unaffected.
- Fixes a latent bug: the old `meta.goal.clone()` resume appended a
  duplicate of the first user message; the synthetic-continue path
  no longer duplicates.
- Verified end-to-end against DeepSeek (openai-compatible endpoint):
  `run` → `resume <id> -p "..."` (converse, remembered prior turn)
  → `resume <id>` (no message → appended "Continue from where you
  left off.", not the original goal). Transcript stayed clean (no
  duplicate system messages).
- Quality gates: `cargo test --workspace`, `cargo clippy
  --all-targets --all-features -- -D warnings`, `cargo fmt --all
  --check` all clean.
- Downstream benefit: the AgentProc `recursive` bridge can now use
  native `recursive resume <session-id> --message <msg>` instead of
  the bespoke `replay --resume-from N <transcript>` hack — follow-up
  in the agentproc project.

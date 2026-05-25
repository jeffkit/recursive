# Loop State

> **Live session snapshot for the orchestrator.** This file changes
> every wake. Read it after `OPERATIONS.md` to know where the
> previous orchestrator left off. Treat dates in UTC; product
> baseline is whatever `git log -1` says on `main`.

## Currently in flight

> **As of 2026-05-25T07:35Z.** Update this whenever a batch is
> launched or a run terminates.

| worktree id | provider | goal | started | log file | pid (may be stale) |
| --- | --- | --- | --- | --- | --- |
| `search-files-tool-deepseek-20260525T073301Z-35012` | deepseek | 11 search-files | 07:33Z | `.dev/runs/search-files-tool-deepseek-20260525T073301Z-35012.log` | 35038 |

The other half of this batch (`default-system-prompt-minimax-…-35040`)
already committed at 50a8a61 on its branch. Wait for the deepseek half
before merging both at once.

## Last batch landed

> **Goals 09 + 10**, second concurrent batch. Merge commits
> `58c2bcb` (09) and `929a998` (10) on `main`. INDEX.md row written.
> Worktrees + branches cleaned. 95 tests green after the pair.

## Background processes

> Anything the orchestrator started that should outlive the current
> reply. Kill these before handover if they aren't doing useful work
> anymore; or pass the PIDs to the next orchestrator.

- **Watcher**: polling loop scanning `.dev/runs/*.log` for terminal
  markers, emitting `AGENT_LOOP_WAKE_self_improve` sentinels. Started
  during goal 09/10 launch. PID 9462 (the inner `bash -c` child;
  the parent shell wrapper is 9435). 5-second polling interval.
- **Fallback heartbeat**: `sleep 1800 && echo <sentinel>`. Started
  during goal 09/10 launch as `loop` skill mandates. Will fire
  unconditionally ~30 min after arming if not yet consumed.

If you take over a stale session, re-arm both rather than trust the
old ones.

## Recent observations worth knowing

These shape the next few goal picks:

- **Cost ratio is dominated by accumulated prompt tokens, not
  completions.** Goal-06 surfaced ~137:1. Goals that reduce
  transcript volume (truncation, summarisation, cache-aware
  re-sends) are the high-leverage area for cost.
- **MiniMax often picks `write_file` for small single-file changes**
  even when the goal recommends `apply_patch`. Goal-12 (in flight)
  is partly a behavioural fix via the default system prompt.
- **The CLI struct in `src/main.rs` is the recurring merge
  conflict.** When two parallel goals both add a flag, expect a
  4-block manual stitch. Plan goals to avoid double-flag batches
  when possible.

## Candidate next goals

Picked from observations + outstanding directions. Not committed to
files yet; pick one, write the goal file, launch:

- **DeepSeek cache_control headers** — DeepSeek charges ~10× less
  for cached prompt tokens. Plumb the right header. Touches
  `src/llm/openai.rs`.
- **`max_transcript_chars` default from env** — already supported
  via CLI in goal-07; surface as a `Config` default-from-env so
  programmatic users get it for free. Touches `src/config.rs`.
- **transcript replay-from-step** — load a saved transcript, prompt
  a new provider starting from message N. Builds on 08+09.
- **system-prompt context budgeting** — auto-trim oldest tool
  results when transcript exceeds N chars. Adjacent to goal-07 but
  trims instead of stopping.
- **error-retry policy configurable** — `OpenAiProvider`'s retry
  policy is currently hardcoded. Pull it out to `Config`.
- **kill `CountLines` tool** — it predates `wc -l` via `run_shell`
  and isn't paying its keep. Cleanup goal.
- **streaming output (LlmProvider::complete_stream)** — the big
  one. Reserved until worktree concurrency has more mileage.

## Open follow-ups (human-facing)

Items the previous orchestrator wanted to flag but didn't HITL on:

- Cargo.toml metadata is final for crates.io but **no token yet
  pushed to GitHub Actions `CRATES_IO_TOKEN`**. The release workflow
  will fail until that secret is set. Out-of-scope for the loop.
- The DeepSeek key has been used in-chat. The user agreed to keep
  using it but it should still be rotated at some point.

---

> **Refresh discipline.** After each batch lands, edit:
> 1. "Currently in flight" — empty it.
> 2. "Last batch landed" — replace with the new pair.
> 3. "Candidate next goals" — strike off whatever you just used.
> 4. Commit this file with the merge commits (one combined `dev:
>    loop-state` commit per batch is fine).

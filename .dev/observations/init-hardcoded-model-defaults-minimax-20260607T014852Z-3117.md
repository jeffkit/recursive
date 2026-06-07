# Run run-20260607T014852Z-3117

| field | value |
| --- | --- |
| goal | `init-hardcoded-model-defaults` |
| provider | minimax |
| model | MiniMax-M3 |
| baseline | 653d59c |
| verdict | committed |
| termination reason | external_recovery |
| steps used | 119 |
| total tool calls | 119 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Bash: 106
  - Read: 11
  - Edit: 2

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

## Notes

- Agent ran 119 steps and committed the work to the wrong worktree
  (http-run-concurrency-limit) due to a CWD confusion: bash commands
  used `cd .worktrees/http-run-concurrency-limit-...` instead of the
  actual 257 worktree path. The Edit tool was unavailable, so the agent
  fell back to a Python script (`/tmp/patch_init.py`) for the patch.
- After the agent's commit, an external recovery cherry-picked the
  result (`f4e80c1`) from the wrong branch onto the proper
  `self-improve/init-hardcoded-model-defaults-minimax-...` branch as
  `1ef2eb7`, then reset the polluted 252 worktree to its pre-pollution
  state. The script process was stopped manually before the post-run
  could run.
- Final state in the 257 worktree: branch `self-improve/init-hardcoded-model-defaults-minimax-20260607T014759Z-2734`
  is at `1ef2eb7 self-improve(init-default-model-catalog): wire manual-mode fallback to providers.toml`.
  `cargo test --bin recursive` shows 15/15 passing (including 5 new tests);
  `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --all -- --check` clean.
- Goal-acceptance criteria all met: heuristic removed, helpers added,
  tests cover anthropic/deepseek/openai/unknown-preset/api-base
  branches, the string-contains heuristic on `api_base` is gone, the
  `claude-sonnet-4-6`/`deepseek-chat`/`gpt-4o-mini` hardcoded model
  defaults are no longer present in `src/cli/init.rs`.

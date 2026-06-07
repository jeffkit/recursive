# Manual edit: recover-257-branch-pollution

**Date**: 2026-06-07
**Goal**: Goal 257 (init-hardcoded-model-defaults, minimax) committed to the wrong
branch (252 http-run-concurrency-limit) due to a CWD confusion in bash commands.
The agent ran `cd .worktrees/http-run-concurrency-limit-...` and committed
`f4e80c1` there, then the script's post-run would have rolled everything back
including any legitimate work. Manual cherry-pick recovery preserved the work
on the correct branch.

**Files touched**:
- `.worktrees/init-hardcoded-model-defaults-minimax-20260607T014759Z-2734/` —
  cherry-picked `f4e80c1` from 252 → 257 branch as `1ef2eb7`
- `.worktrees/http-run-concurrency-limit-deepseek-20260607T013312Z-66736/` —
  `git reset --hard 319d55d` to revert pollution back to pre-`f4e80c1` state
- `recovery/init-default-model-catalog` — new branch pointing at `1ef2eb7` as
  safety net
- `.dev/observations/init-hardcoded-model-defaults-minimax-20260607T014852Z-3117.md` —
  wrote observation manually (script's post-run never executed; agent was
  killed before its natural exit)
- `.dev/runs/init-hardcoded-model-defaults-minimax-20260607T014759Z-2734.pid` —
  removed stale PID file

**Tests added**: none (the agent's `f4e80c1` already added 5 new tests in
`src/cli/init.rs`: `init_default_model_uses_catalog_for_anthropic`,
`init_default_model_detect_from_api_base_deepseek`,
`init_default_model_detect_from_api_base_openai`,
`init_default_model_detect_from_api_base_unknown_is_empty`,
`init_default_model_uses_catalog_for_unknown_preset_is_empty`)

**Notes**:

The root cause is structural, not a model mistake: the agent's bash CWD was
the project root, and absolute path resolution was inconsistent because the
worktree's branch name was a substring match for the 252 path. The agent
typed `.worktrees/http-run-concurrency-limit-...` thinking it was its own
worktree. The Edit tool was unavailable to this particular minimax run
(no Edit registered in tool list), so the agent fell back to a Python
`/tmp/patch_init.py` script — which worked, but isolated the commit from
the agent's normal review flow.

For future runs:
- The script's `git -C "$WORKTREE_PATH" ...` style would prevent CWD
  confusion entirely. Currently it uses `cd` inside the bash command.
- A pre-commit sanity check (`git rev-parse --abbrev-ref HEAD` in the
  worktree) could catch commits going to the wrong branch.
- The post-run's PRODUCT_CHANGES check (`.dev/scripts/self-improve.sh:858`)
  would have skipped the commit, so the work was safe even if I hadn't
  intervened — but the script's hard-reset to BASELINE_HEAD (line 573)
  would have wiped the work *in the 257 worktree* because the commit
  landed on 252. So manual intervention was needed before the script's
  natural post-run path.
- A `recovery/` namespace convention is worth keeping — git's reflog is
  technically recoverable, but a named branch is easier to find.

The cherry-pick was clean (no conflicts) because `f4e80c1` only touched
`src/cli/init.rs` and the 257 branch was at `653d59c` (no overlap with 252's
HTTP concurrency work).

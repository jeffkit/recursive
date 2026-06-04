# Goal 235: Reviewer Agent with read_file Access

## Summary

Give the review agent in `review-changes.sh` access to the `read_file`
tool and increase its step budget from 1 to 5. This lets the reviewer
actively look up callers, definitions, and context it needs to judge the
diff accurately, rather than guessing from the diff text alone.

## Motivation

The current reviewer runs with `RECURSIVE_MAX_STEPS=1` — a single LLM
call with no tool access. If the diff references a function whose
implementation isn't visible in the diff, the reviewer must guess. This
causes both false positives (flagging valid code) and false negatives
(missing subtle bugs). Giving it `read_file` access with a small step
budget (5) lets it look up what it needs without becoming expensive.

## What to implement

In `.dev/scripts/review-changes.sh`, change the reviewer invocation:

**Before:**
```bash
export RECURSIVE_MAX_STEPS=1
...
REVIEW_OUTPUT=$("$BIN" --workspace . --log error run "$(cat "$REVIEW_PROMPT_FILE")" 2>/dev/null || true)
```

**After:**
```bash
export RECURSIVE_MAX_STEPS="${RECURSIVE_REVIEW_MAX_STEPS:-5}"
# Allow read_file but not write_file or run_shell — read-only review
REVIEW_OUTPUT=$("$BIN" --workspace . --log error \
  --allow-tools read_file,list_dir \
  run "$(cat "$REVIEW_PROMPT_FILE")" 2>/dev/null || true)
```

Also update the review prompt (`.dev/prompts/code-review.md`) to tell the
reviewer it has `read_file` access:

Add to the "Your task" section:
```
You have access to `read_file` and `list_dir` tools. Use them when you need
to understand context that is not visible in the diff — for example, to look
up a function's callers, check an interface definition, or verify a constant's
value. Limit yourself to 3-4 file reads; the goal is targeted context, not
a full codebase scan.
```

## Implementation notes

- The `--allow-tools` flag filters the tool registry to the listed tools.
  Check the current CLI API — the flag may be `--tools` or similar. If the
  flag does not exist, implement it as a new CLI option that sets an allow-list
  on the `ToolRegistry` before running.
- `RECURSIVE_REVIEW_MAX_STEPS` env var controls the budget (default 5).
  Setting it to 1 restores single-turn behaviour.
- This changes `.dev/scripts/review-changes.sh` and
  `.dev/prompts/code-review.md`. If `--allow-tools` requires a product code
  change (`src/`), implement that too.
- Keep the change minimal — don't refactor the review pipeline, just add the
  tool access and step budget.

## Acceptance

- Review agent can call `read_file` during a review run (visible in transcript).
- Review agent cannot call `write_file` or `run_shell` (blocked by allow-list).
- `RECURSIVE_REVIEW_MAX_STEPS=1` restores single-turn behaviour.
- `cargo test` green after any `src/` changes.

# Goal 232: Goal Complexity Hint for self-improve.sh

## Summary

Add support for an optional `## Complexity: hard` marker in goal files.
When `self-improve.sh` detects this marker, it escalates to the pro-tier
model and doubles the step budget before starting the agent run.

## Motivation

Some goals are inherently complex (multi-file refactors, new subsystems)
and routinely exhaust the default 200-step budget or fail on flash-tier
models. Currently the orchestrator has no way to signal this upfront —
the only option is to manually set `RECURSIVE_PROVIDER=deepseek-pro`
before the run. This creates friction and causes avoidable rollbacks.

A simple declarative marker in the goal file gives the orchestrator a
lightweight knob without changing the shell workflow.

## What to implement

In `.dev/scripts/self-improve.sh`:

1. After loading `GOAL_BODY` from the goal file, scan for the marker:
   ```
   COMPLEXITY_HARD=0
   if echo "$GOAL_BODY" | grep -qiE '^##\s*Complexity:\s*hard'; then
     COMPLEXITY_HARD=1
   fi
   ```

2. If `COMPLEXITY_HARD=1`:
   - If no provider has been explicitly set via `RECURSIVE_PROVIDER` or
     `RECURSIVE_PROVIDERS`, force `RECURSIVE_PROVIDER=deepseek-pro` (skip
     the flash-first logic entirely for this run).
   - If `RECURSIVE_MAX_STEPS` has not been overridden by the caller, set
     `RECURSIVE_MAX_STEPS=400`.
   - Log a line: `[self-improve] Complexity: hard — using pro tier, 400 steps`

3. The marker is **advisory**: if the caller already set `RECURSIVE_PROVIDER`
   explicitly, respect that — don't override the caller's choice.

## Implementation notes

- The marker format is case-insensitive (`hard`, `Hard`, `HARD` all match).
- This touches only `.dev/scripts/self-improve.sh` (the `.dev/` meta-tooling),
  not product code under `src/`. This goal explicitly targets `.dev/scripts/`.
- No new goal files need the marker yet — it's opt-in for future goals.
- Add a one-line note about the marker to `.dev/OPERATIONS.md` §3.1
  ("Pick goals and write goal files") so future orchestrators know it exists.

## Acceptance

```bash
# Create a test goal file containing "## Complexity: hard"
echo -e "# Test\n\n## Complexity: hard\n\nDo nothing." > /tmp/test-hard-goal.md

# Dry-run (RECURSIVE_NO_COMMIT=1, RECURSIVE_MAX_STEPS not set):
RECURSIVE_NO_COMMIT=1 .dev/scripts/self-improve.sh /tmp/test-hard-goal.md

# Expected: log line "Complexity: hard — using pro tier, 400 steps" appears
# Expected: agent invoked with RECURSIVE_MAX_STEPS=400
```

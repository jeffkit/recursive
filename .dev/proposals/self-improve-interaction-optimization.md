# Proposal: Self-Improve Loop Interaction Optimization

> **Status**: Queued — implement after kernel architecture Phase 3 completes
> **Created**: 2026-05-28
> **Scope**: `.dev/scripts/` infrastructure

## Problem

Current self-improve agents have no memory between attempts:
- Each run starts from scratch (reads goal + source, no context of previous attempts)
- Failure means full rollback with zero learning
- Agent can't build on partial progress from a truncated run

## Proposed Optimizations

### 1. Failure Context Injection (Priority: High)

When a run fails (rolled back), inject the failure reason into the next retry:

```bash
# In self-improve.sh, after rollback:
if [ "$RETRY" = "1" ]; then
  FAILURE_CONTEXT="Previous attempt failed: $(tail -20 $LOG)"
  # Append to system prompt or goal file
  echo -e "\n\n## Previous Attempt Failed\n$FAILURE_CONTEXT" >> "$GOAL_FILE_COPY"
fi
```

Benefits:
- Agent avoids repeating the same mistake
- Compiler errors from attempt 1 guide attempt 2

### 2. Partial Progress Preservation (Priority: Medium)

If a run produces correct code but fails at a late stage (e.g., clippy warning),
preserve the working patches and inject them as hints:

```bash
# Save diff before rollback
git diff HEAD > .dev/runs/$ID-partial.patch
# On retry, include as context
```

### 3. Warm-Start via Observation Files (Priority: Low)

Goal files can reference previous observations:
```markdown
## Context from previous goals
- Goal 125 observation: RunCore struct extracted successfully
- Goal 126 observation: kernel.rs now has run() method
```

This reduces the "re-discovery" cost where agents re-read the same files.

## Implementation Plan

1. Modify `self-improve.sh` to save failure context on rollback
2. Modify retry logic to inject failure context into goal
3. Add `--warm-start <observation-file>` flag to self-improve.sh
4. Test with a known-failing goal to measure improvement

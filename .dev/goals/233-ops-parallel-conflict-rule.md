# Goal 233: Document Parallel Conflict Rule in OPERATIONS.md

## Summary

Add an explicit rule to `.dev/OPERATIONS.md` §3.2 (Launch) that instructs
the orchestrator to serialize goals that are likely to touch the same files,
rather than running them in parallel.

## Motivation

`parallel-self-improve.sh` creates independent worktrees, but the merge step
back into `main` can produce conflicts when two concurrent goals both modify
high-contention files (typically `src/main.rs`, `src/lib.rs`, `Cargo.toml`).
These conflicts require human intervention and interrupt the loop.

Adding an explicit rule to OPERATIONS.md makes the policy clear to any
orchestrator (human or automated) picking up the loop.

## What to implement

In `.dev/OPERATIONS.md`, in section **§3.2 Launch**, add a new subsection
or note immediately after the parallel launch example:

```
### 3.2.1 Parallelism safety rule

Before running two goals in parallel, check whether their expected file
touch-sets overlap. Goals that both modify any of the following files MUST
be serialized:

  src/main.rs
  src/lib.rs
  src/agent.rs
  Cargo.toml

How to check: read each goal file and look for explicit file mentions, or
use `grep -r` on the goal description for the file names above. When in
doubt, serialize.

Rationale: parallel worktrees merge cleanly when they touch disjoint files.
Conflicts in high-contention files like main.rs require manual resolution
and stall the loop.
```

## Implementation notes

- This is a documentation-only change to `.dev/OPERATIONS.md`.
- No product code (`src/`) is touched.
- Keep the addition concise — one short subsection, not a long essay.

## Acceptance

The rule appears in OPERATIONS.md §3.2 and covers:
1. Which files trigger the serialization requirement
2. How to check (grep goal description)
3. Why (merge conflicts stall the loop)

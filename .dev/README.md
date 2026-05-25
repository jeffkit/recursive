# `.dev/` — developer workshop

Nothing in this directory is part of the shipping product. It exists only
because **this developer uses Recursive to improve Recursive itself**, and
those workflow artifacts deserve to live next to the code they evolve, not
inside the product surface.

## What lives here

- `AGENTS.md` — the contract Recursive reads when invoked against its own
  source. Out of scope for end-users who invoke Recursive on their own
  codebase.
- `goals/` — supervisor-authored task descriptions, one per planned
  self-improvement iteration.
- `journal/` — append-only record of every self-improvement run: goal,
  transcript, test outcome, verdict (committed / rolled-back). Acts as
  long-term memory across runs.
- `scripts/self-improve.sh` — wrapper that points Recursive at its own
  source, injects `AGENTS.md` + recent journal as the system prompt, runs
  the agent, verifies tests, then either commits or rolls back.

## Why a `.dev/` boundary

Self-iteration is the developer's relationship with the project — not a
feature of the product. Keeping these artifacts under `.dev/` means:

- Anyone reading `src/`, `tests/`, `README.md`, `Cargo.toml` sees a plain
  coding agent. No leaking meta-process.
- The agent's own hard limits (see `AGENTS.md`) treat `.dev/` as off-limits
  unless a goal says otherwise. Self-improvement can't accidentally rewrite
  its own scaffolding.

## How to invoke a self-improvement cycle

```bash
# from the repo root, with a clean working tree on a committed HEAD:
.dev/scripts/self-improve.sh .dev/goals/02-anti-stuck.md
```

The script will commit on success and hard-reset on failure. You only need
to read the resulting journal entry to know what happened.

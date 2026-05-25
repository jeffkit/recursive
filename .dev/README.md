# `.dev/` — developer workshop

Nothing in this directory is part of the shipping product. It exists only
because **this developer uses Recursive to improve Recursive itself**, and
those workflow artifacts deserve to live next to the code they evolve, not
inside the product surface.

## What lives here

- `AGENTS.md` — the contract Recursive reads when invoked against its own
  source. Out of scope for end-users who invoke Recursive on their own
  codebase.
- `OPERATIONS.md` — SOP for the **orchestrator** role (the one who picks
  goals, launches runs, merges results, decides what to do next). Read
  this if you're picking up the self-improve loop after the previous
  orchestrator stepped away.
- `loop-state.md` — live snapshot of the orchestrator session: what's in
  flight, last batch landed, candidate next goals, background watchers.
  Refreshed by the orchestrator on every wake.
- `goals/` — supervisor-authored task descriptions, one per planned
  self-improvement iteration.
- `journal/` — append-only record of every self-improvement run: goal,
  transcript, test outcome, verdict (committed / rolled-back). Acts as
  long-term memory across runs.
- `observations/INDEX.md` + per-run files — structured metrics
  (steps, tool-call mix, errors, cost) auto-extracted from each
  journal by `scripts/observe.sh` and pinned to a rolling
  comparison table.
- `scripts/self-improve.sh` — wrapper that points Recursive at its own
  source, injects `AGENTS.md` + recent journal as the system prompt, runs
  the agent, verifies tests, then either commits or rolls back.
- `scripts/parallel-self-improve.sh` — wraps the above in a fresh git
  worktree on a new branch, so multiple goals can run concurrently
  without touching each other.
- `scripts/observe.sh` — journal → structured metrics extractor.

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

## How to cut a release

CI on `main` must already be green. Then:

```bash
# 1. Bump version in Cargo.toml (and update Cargo.lock with `cargo build`)
vim Cargo.toml

# 2. Commit
git commit -am "chore: release 0.2.0"

# 3. Tag + push tag
git tag -a v0.2.0 -m "v0.2.0"
git push origin main v0.2.0
```

Pushing the tag fires `.github/workflows/release.yml`, which:

1. Verifies the tag (e.g. `v0.2.0`) matches `Cargo.toml`'s `version` (e.g.
   `0.2.0`). Mismatch fails the run before anything else happens.
2. Re-runs `fmt --check`, `clippy -D warnings`, and the full test suite.
3. Runs `cargo publish` using the `CRATES_IO_TOKEN` repository secret.
4. Creates a GitHub Release at that tag with auto-generated notes (skipped
   if the release already exists).

No tokens leave your machine; rotating the publish token is one click in
`https://crates.io/settings/tokens` followed by updating the secret in
`https://github.com/jeffkit/recursive/settings/secrets/actions`.

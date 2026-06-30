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
- `scripts/launch-flow.sh` — **canonical** launcher for the Flowcast
  self-improve flow (`.dev/flows/self-improve.flow.js`); see
  `.dev/flows/SELF_IMPROVE.md`. Enforces cargo test/clippy/fmt + project
  gates from `.flowcast/gates.json` (`e2e`, `tui-mutants`).
- `scripts/self-improve.sh` — ⚠️ **deprecated** legacy bash wrapper.
  Kept for historical reference; gates added after flow adoption may be
  missing here. Use `launch-flow.sh` instead.
- `scripts/parallel-self-improve.sh` — ⚠️ **deprecated** wrapper around
  the legacy bash script. The flow handles worktree isolation itself.
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
# canonical: Flowcast flow (auditable, observable, resumable)
.dev/scripts/launch-flow.sh --goal-file .dev/goals/02-anti-stuck.md --provider deepseek
# see .dev/flows/SELF_IMPROVE.md for the full flag set
```

The flow commits on success and rolls back on failure. You only need
to read the resulting journal entry to know what happened. The legacy
`.dev/scripts/self-improve.sh .dev/goals/02-anti-stuck.md` is deprecated.

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

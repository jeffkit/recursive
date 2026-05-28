# Manual landing of g141 (.dev internal cleanup) — orchestrator direct execution

- mode:   orchestrator-manual (no LLM)
- branch: chore/dev-internal-cleanup
- scope:  `.dev/` only — no product code touched

Two follow-ups from prior journals (orchestrator-notes-20260528T072202Z,
g134, g135) bundled into one chore commit.

## 1. `runs-gc` script

`.dev/runs/` is gitignored and meant to hold live `<id>.pid` /
`<id>.log` / `<id>.notified` triples while a self-improve run is in
flight. After 3+ days of activity, 140 stale `.pid` files
accumulated, all pointing at long-dead PIDs. macOS rotates PIDs
quickly, so two of them were aliased to live system processes
(`findmybeaconingd`, `SystemUIServer`) — naively diagnosing
"is this run still alive" via `kill -0 $(cat *.pid)` would have
falsely answered yes.

New `.dev/scripts/runs-gc.sh`:
- Reads each `<id>.pid`, asserts pid is alive AND its `ps`
  `command=` includes `recursive` or `self-improve.sh`.
- Anything else is stale; deletes `.pid` + `.notified` (logs
  preserved by default, deletable via `--logs`).
- `--dry-run` previews without touching the filesystem.
- Reported: 140/140 swept on first run; expect this to converge
  to 0 once each new run is followed by `runs-gc.sh` (or once
  `parallel-self-improve.sh` learns to clean up its own pid file
  on terminal-marker emission — that's a separate follow-up).

## 2. `self-improve.sh` fmt enforcement

g133 (permissions-config) merged with `cargo fmt --all -- --check`
red on main HEAD; g134's manual landing had to ship a separate
`chore: cargo fmt --all (g133 leftovers)` commit to clean it up.
Root cause: AGENTS.md L154 prescribed `cargo fmt --all` (in-place)
but `self-improve.sh` did not enforce `--check` post-run, so an
agent that forgot the fmt step could still pass the gate.

Now enforced:
- `self-improve.sh` runs `cargo fmt --all -- --check` after
  `cargo test` and before the smoke gate.
- Failure = rollback. Tail of the diff is logged so the operator
  can see exactly which lines tripped it.
- Disable knob: `RECURSIVE_FMT_CHECK=0`. Documented in AGENTS.md
  as "only with a journal note explaining why".

## 3. AGENTS.md doc update

Section 6 now explicitly says fmt is a hard gate. Previous wording
implied "you should run it"; the new wording says "the wrapper
will roll you back if you don't".

## Quality gates

| gate | result |
|------|--------|
| `bash -n .dev/scripts/runs-gc.sh` | OK |
| `.dev/scripts/runs-gc.sh --dry-run` | sweeps 140/140 stale |
| `.dev/scripts/runs-gc.sh` | actual sweep, 140 removed |
| Rust quality gates | unchanged — no Rust code touched |

## Follow-up recorded

- `parallel-self-improve.sh` post-run hook to delete its own .pid
  on terminal-marker emission. The combination would be: gc-on-write
  (per-run hook) + gc-on-demand (this script). Out of scope here.
- Could add a CI check that runs `cargo fmt --all -- --check` on
  PRs, but it's already enforced at self-improve time, which is
  closer to the source of truth (manually-pushed PRs aren't the
  primary dev flow on this repo).

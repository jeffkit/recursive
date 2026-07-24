# Goal 346 — Flow watchdog: detect a hung `recursive` process and preserve the scene fast

**Roadmap**: Hardening (self-improve loop reliability) — incident-driven, follow-up to Goal 345 §5

**Design principle check**:
- Implemented entirely in `.dev/flows/self-improve.flow.js` (tracked flow JS) + a new test
  file under `.dev/flows/test/`. No product code, no vendored-node_modules edits.
- The watchdog kills the `recursive` *child process* by PID (found via `pgrep -f
  <transcriptOut>`), so `spawnCapture`'s existing `proc.on('close')` fires and the run
  resolves normally — no spawn-primitive change.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT edit `.dev/flows/node_modules/flowcast/spawn.js` (gitignored npm dep — would
  not survive `npm install` and cannot be cherry-picked; see Notes).

## Why

Goal 331 lost everything because the `recursive` process **finished its work but never
exited** (session `.meta.json` stayed `active`; flow `events.jsonl` stuck at
`run.recursive` start). The flow then waited up to `RUN_TIMEOUT_MS` (2h) for a `close`
event that never came, and was eventually killed externally — deleting the worktree.

Goal 345 §4 added a *wall-clock exit inside the agent* (`RECURSIVE_WALL_TIMEOUT_SECS` /
`FinishReason::WallClockExceeded`) so a well-behaved agent exits on its own. But that only
helps if the agent's own deadline logic runs. The g331 failure mode was the agent **hanging
after a cross-turn compaction** — precisely when an in-process deadline might not get a
chance to fire. The flow-side defense (this goal) is the belt to those suspenders: even if
the agent is hung with no `close`, the flow should detect it within minutes and preserve
the scene, not wait 2h for an external kill.

Goal 345 §5 specified this but was not implemented (§1–4 landed as `4d9744b`; §5 deferred).
This goal closes that gap.

## Scope (do exactly this, no more)

### 1. Watchdog helper — `.dev/flows/self-improve.flow.js` (new exported function)

Add `startRecursiveWatchdog({ transcriptOut, idleMs, pollMs, graceMs, onTrigger })` that
runs concurrently with the `recursive` await and decides when to `SIGTERM` the child. It
does NOT need a child handle from `spawnCapture` — it locates the child by PID via
`pgrep -f <escaped transcriptOut>` (the `--transcript-out <path>` arg is unique per run, so
this is safe even under parallel self-improve). Sketch:

```js
import { spawnSync } from 'child_process'
import { existsSync, statSync } from 'fs'

function findRecursivePid(transcriptOut) {
  if (!transcriptOut) return null
  // escape ERE metachars so the literal path is matched, not a regex
  const pat = transcriptOut.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  try {
    const r = spawnSync('pgrep', ['-f', pat], { encoding: 'utf8' })
    if (r.status !== 0) return null
    const pids = r.stdout.split('\n').map(s => s.trim()).filter(Boolean).map(Number)
    return pids.length ? pids[0] : null
  } catch { return null }
}

function startRecursiveWatchdog({ transcriptOut, idleMs = 10 * 60 * 1000, pollMs = 15_000, graceMs = 30_000, onTrigger }) {
  const start = Date.now()
  let lastSize = -1
  let lastGrowthAt = start
  let finishMarkerSeenAt = null
  let stopped = false
  const reason = { current: null }
  const tick = () => {
    if (stopped) return
    let size = -1
    if (existsSync(transcriptOut)) {
      try { size = statSync(transcriptOut).size } catch { size = lastSize }
    }
    if (size > lastSize) { lastSize = size; lastGrowthAt = Date.now() }
    // finish-marker short-circuit: scan the file tail for the done marker
    if (size > 0) {
      try {
        const tail = readFileSync(transcriptOut, 'utf8').slice(-4096)
        if (/\[done after \d+ steps\]\s*reason:/.test(tail)) {
          if (finishMarkerSeenAt === null) finishMarkerSeenAt = Date.now()
        }
      } catch { /* file mid-write */ }
    }
    const now = Date.now()
    if (finishMarkerSeenAt !== null && now - finishMarkerSeenAt > graceMs) {
      reason.current = 'finish-marker-hang'; fire(); return
    }
    // only trip no-growth after the run has had at least idleMs to even start
    if (now - start >= idleMs && now - lastGrowthAt >= idleMs) {
      reason.current = 'no-growth-hung'; fire(); return
    }
  }
  const fire = () => {
    if (stopped) return
    stopped = true
    const pid = findRecursivePid(transcriptOut)
    if (pid != null) { try { process.kill(pid, 'SIGTERM') } catch { /* already dead */ } }
    try { onTrigger?.(reason.current) } catch { /* watcher cb must not break */ }
  }
  const timer = setInterval(tick, pollMs)
  // also do one immediate tick so a pre-existing finish marker is caught fast
  tick()
  return {
    stop() { stopped = true; clearInterval(timer) },
    get reason() { return reason.current },
  }
}
```

Rules:
- `SIGTERM` only from the watchdog — NEVER `SIGKILL`. Let `spawnCapture`'s existing close
  resolution run; the `timeout` path's `SIGKILL` backstop is the only hard-kill.
- `onTrigger` is called once, after the kill attempt, with the reason string.
- `stop()` clears the interval and idempotently disables firing.
- The idle window is gated on `now - start >= idleMs` so the cold-start gap before the
  first transcript write doesn't trip the no-growth branch.
- `readFileSync`/`statSync` errors are swallowed (the file is mid-write during a live run).

### 2. Wire the watchdog into the `run.recursive` step — `.dev/flows/self-improve.flow.js`

In `runAttempt` (line ~705), the `run.recursive` step (line ~717) becomes:

```js
let watchdogReason = null
const runMeta = await cp.step('run.recursive', async () => {
  const watchdog = startRecursiveWatchdog({
    transcriptOut,
    idleMs: (parseInt(process.env.RECURSIVE_WATCHDOG_IDLE_MS ?? '', 10) || 10 * 60 * 1000),
    pollMs: 15_000,
    graceMs: 30_000,
    onTrigger: (r) => { watchdogReason = r },
  })
  try {
    const out = await recursive(goal, { ...base(), transcriptOut })
    return out._meta
  } finally {
    watchdog.stop()
  }
})
```

No change to `recursive()` itself — it still uses `spawnCapture`. The watchdog runs
concurrently via its own `setInterval` and kills the child by PID when tripped, so
`spawnCapture`'s `proc.on('close')` fires and `recursive()` resolves normally.

### 3. Route a watchdog-triggered kill through the existing preserve path

After the `run.recursive` step, if `watchdogReason` is set, treat it like the existing
`runMeta.timedOut` path (line ~738): fall into the budget-resume / preserve flow, but tag
the preserve reason with the watchdog cause, e.g. `watchdog: finish-marker-hang` /
`watchdog: no-growth-hung`. Reuse `preserveScene` (Goal 345 §3, line ~370) — the worktree
moves to `.worktrees/preserve/<run-id>/` with a `refs/preserve/<run-id>` ref, within
~10 min instead of 2h.

- Do NOT add a new verdict. Reuse `failed-preserved` so `--resume-preserve` /
  `--land-preserve` work unchanged.
- A watchdog kill yields `exitCode` 128+15 (SIGTERM) and `timedOut: false` from
  `spawnCapture`; the existing `runMeta.panicked` check (line ~723) must NOT mis-classify
  SIGTERM as a panic — verify `panicked` is `exitCode === 101 || exitCode >= 128`? Check
  the current rule at line ~113 (`panicked = exitCode === 101 || exitCode >= 128`) and
  GUARD the watchdog case so a SIGTERM-killed run is treated as `timedOut`-equivalent
  (preserve), NOT `panic-preserved`. If the existing `panicked` rule would catch 143,
  add a `watchdogReason` short-circuit BEFORE the `runMeta.panicked` branch.

### 4. Tests — `.dev/flows/test/watchdog.test.mjs` (node:test + node:assert)

Flow JS is NOT covered by cargo gates. Add `node --test` cases (use tiny `idleMs`/`pollMs`
via the helper's direct args, NOT env, so tests are fast and deterministic):

- **no-growth short-circuit**: spawn a child that writes nothing to a temp
  `transcriptOut` and never exits (e.g. `node -e "setInterval(()=>{}, 60000)"`); assert
  `watchdog.reason === 'no-growth-hung'` within `idleMs + 2*pollMs` and that the child was
  `SIGTERM`'d (the child exits). Use `idleMs: 200, pollMs: 50, graceMs: 50`.
- **finish-marker short-circuit**: write a transcript file containing
  `[done after 3 steps] reason: NoMoreToolCalls` while a child that never exits is alive;
  assert `watchdog.reason === 'finish-marker-hang'` within `graceMs + 2*pollMs` and the
  child is `SIGTERM`'d. Use `graceMs: 100, pollMs: 50`.
- **healthy run does not trip**: a child that exits 0 quickly (writes a small transcript
  with NO finish marker then exits); assert `watchdog.reason === null` and `stop()` clears
  the interval (no SIGTERM after exit).
- **stop() is idempotent / cancels fire**: call `stop()` before any trip; assert no
  `onTrigger` fires even after `idleMs` elapses.
- **findRecursivePid escapes & matches**: assert `findRecursivePid` returns `null` for a
  non-existent path and returns the correct PID for a child spawned with a matching
  `--transcript-out <tmp>` arg (use a unique tmp path; clean up).

The agent must run `node --test .dev/flows/test/watchdog.test.mjs` and ensure all green.
Add the same command to the flow's gate list (`.flowcast/gates.json`) as a new
`flow-watchdog` gate (cmd: `node --test .dev/flows/test/watchdog.test.mjs`, onFail
`resume-fix`) so it's enforced on future runs — OR, if adding a gate is too involved,
document in the journal that the test is run manually pre-landing and leave gate-wiring
as a follow-up. Prefer adding the gate.

## Acceptance
- `cargo test --workspace` green; `cargo clippy --all-targets --all-features -- -D warnings`
  clean; `cargo fmt --all` clean. (Unaffected — no product code; run anyway to prove no
  accidental product edit.)
- `node --check .dev/flows/self-improve.flow.js` clean.
- `node --test .dev/flows/test/watchdog.test.mjs` green (all 5 cases).
- The watchdog is active by default (no env flag required to enable).
  `RECURSIVE_WATCHDOG_IDLE_MS` only tunes the idle window (default 10 min).
- `RUN_TIMEOUT_MS` (2h) remains the hard ceiling; the watchdog short-circuits the
  obvious-hung case faster and preserves data.
- A SIGTERM-killed-by-watchdog run is preserved (`failed-preserved`), NOT classified as
  `panic-preserved`.
- Signal-handler safety unchanged: the watchdog runs in normal async context (timers),
  NOT a signal handler — it must not register `process.on('SIGINT')` or throw uncaught.

## Notes for the agent
- **Read Goal 345 first** (`.dev/goals/345-g331-postmortem-preserve-work-and-compaction-slice.md`)
  — §3 (`preserveScene`, line ~370) and §5 (this goal's spec). Mirror the signal-handler
  `preserveScene`-on-no-verdict pattern at lines ~598–647.
- **`flowcast` is a remote npm dep (`^0.6.0`), NOT vendored**: `.dev/flows/node_modules/`
  is gitignored and regenerated by `npm install`. Edits there cannot be cherry-picked and
  would be overwritten. The ENTIRE change MUST live in tracked files
  (`.dev/flows/self-improve.flow.js` + `.dev/flows/test/`). DO NOT edit anything under
  `.dev/flows/node_modules/`.
- The g331 evidence: transcript's last entry was a cross-turn compaction at 16:54:37,
  session `.meta.json` stayed `active`, flow stuck at `run.recursive` start. A 10-min
  no-growth watchdog would have caught this and preserved the (already-complete) work
  instead of losing it ~34 min later.
- **DO NOT** "fix" a stdout pipe deadlock — `spawnCapture` already drains stdout via a
  flowing listener and resolves on `proc.on('close')`. The watchdog relies on that: it
  `SIGTERM`s the child so `close` fires. Do NOT reimplement spawn in `recursive()` or
  duplicate `spawnCapture`'s drain/buffer/timeout logic.
- **DO NOT** add a new verdict or a new flow step beyond wiring the watchdog into the
  existing `run.recursive` step. Reuse `preserveScene` + `failed-preserved`.
- **DO NOT** `SIGKILL` from the watchdog — `SIGTERM` only.
- **DO NOT** modify `Cargo.toml`, `src/**`, `tests/**`, `crates/**`, `e2e/**`, or
  `.dev/flows/node_modules/**`. This is flow-only hardening. If you edit product code,
  stop — you're off scope.
- The flow runs `self-improve.flow.js` from the **main checkout**, not the worktree, so
  editing it in the worktree is safe (won't affect the in-flight run); it lands via
  cherry-pick.
- `pgrep -f` matches the full argv against an ERE; the transcript path is unique per run
  (under `.flowcast/runs/<run-id>/`), so matching it is safe under parallel self-improve.
  Escape ERE metacharacters in the path before passing to `pgrep`.

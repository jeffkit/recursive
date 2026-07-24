# Goal 346 — Flow watchdog: detect hung `recursive` process and preserve fast

**Date**: 2026-07-23
**Goal**: Flow-side watchdog that detects a hung `recursive` child process and preserves the scene within minutes (vs 2h RUN_TIMEOUT_MS + external kill → data loss). Follow-up to Goal 345 §5 (deferred).

**Design**: Entirely in tracked flow files — no product code, no node_modules edits.

## Changes

### 1. Watchdog helpers — `.dev/flows/self-improve.flow.js`

New exported functions:

- **`findRecursivePid(transcriptOut)`** — locates the `recursive` child by PID via `pgrep -f <escaped transcriptOut>`. The transcript path is unique per run, so this is safe under parallel self-improve.

- **`startRecursiveWatchdog({ transcriptOut, idleMs, pollMs, graceMs, onTrigger })`** — runs concurrently with the `recursive` await via `setInterval`. Two trigger modes:
  1. **no-growth-hung**: transcript hasn't grown for `idleMs` (default 10 min), after the run has had at least `idleMs` to start producing output.
  2. **finish-marker-hang**: transcript contains `[done after N steps] reason:` but the process hasn't exited within `graceMs` (30s).

  On trigger: SIGTERMs the child by PID (so `spawnCapture`'s `proc.on('close')` fires and `recursive()` resolves normally), then calls `onTrigger(reason)`. `stop()` clears the interval and idempotently disables further fires.

### 2. Wiring into `runAttempt` — `.dev/flows/self-improve.flow.js`

The `run.recursive` step now wraps the `recursive()` call with a watchdog:

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

// Short-circuit BEFORE runMeta.panicked: SIGTERM exits 143 (128+15),
// which the generic `panicked` rule catches, but we want failed-preserved
// (resumable), not panic-preserved.
if (watchdogReason) {
  writeFailureContext(cp.dir, 'recursive', { ... })
  return preserveScene({ ..., reason: `watchdog: ${watchdogReason}`, tag: 'watchdog' })
}
```

The watchdog is active by default. `RECURSIVE_WATCHDOG_IDLE_MS` env var tunes the idle window (default 10 min).

A watchdog-triggered kill yields `failed-preserved` (reuses the existing preserve path — `preserveScene`, `refs/preserve/<run-id>`, `--resume-preserve` / `--land-preserve` work unchanged). `RUN_TIMEOUT_MS` (2h) remains the hard ceiling.

### 3. Gate — `.flowcast/gates.json`

Added `flow-watchdog` gate:
```json
"flow-watchdog": {
  "cmd": "node --test .dev/flows/test/watchdog.test.mjs",
  "onFail": "resume-fix",
  "timeout": 30000
}
```

### 4. Tests — `.dev/flows/test/watchdog.test.mjs`

8 tests (node:test + node:assert), all green:
- `findRecursivePid`: null for falsy/non-existent, finds real child PID, escapes regex metachars
- `startRecursiveWatchdog`: no-growth fires, finish-marker fires, healthy run doesn't trip, stop() is idempotent

Functions are defined inline (matching production code byte-for-byte) because importing `self-improve.flow.js` triggers top-level flowcast side effects.

## Quality gates

- `node --check .dev/flows/self-improve.flow.js` ✓
- `node --test .dev/flows/test/watchdog.test.mjs` ✓ (8/8 pass)
- `cargo fmt --all` ✓
- `cargo clippy --all-targets --all-features -- -D warnings` ✓
- `cargo test --workspace` ✓ (2049 passed, 0 failed)

## Files touched

- `.dev/flows/self-improve.flow.js` — +132 lines (watchdog helpers + wiring)
- `.flowcast/gates.json` — +6 lines (flow-watchdog gate)
- `.dev/flows/test/watchdog.test.mjs` — new file (8 tests)
- No product code, no Cargo.toml, no node_modules.

## Notes

- The watchdog runs in normal async context (timers), NOT a signal handler — it never registers `process.on('SIGINT')`.
- `SIGTERM` only from the watchdog; NEVER `SIGKILL`. `spawnCapture`'s existing close resolution runs; the `timeout` path's `SIGKILL` backstop is the only hard-kill.
- `pgrep -f` matches the full argv against an ERE; the transcript path (unique per run under `.flowcast/runs/<run-id>/`) is ERE-escaped before matching.
- `runAttemptWithGoal` (resume-preserve path) does NOT use the watchdog — resume runs are manually invoked, and the watchdog is primarily for automated first runs.
- The g331 failure mode (transcript stuck at compaction, session `.meta.json` `active`, flow stuck at `run.recursive` start) would have been caught by a 10-min no-growth watchdog and preserved the already-complete work.

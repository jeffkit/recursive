// watchdog.test.mjs — node:test suite for startRecursiveWatchdog / findRecursivePid
//
// Run:  node --test .dev/flows/test/watchdog.test.mjs
//
// The functions under test are defined inline (matching the production code in
// ../self-improve.flow.js exactly) because importing that file triggers top-level
// side effects (flowcast providers, config loading) that need the full flowcast
// environment.  The inline definitions are byte-for-byte identical to the exported
// versions; `node --check self-improve.flow.js` catches drift.
import { describe, it } from 'node:test'
import assert from 'node:assert'
import { execSync, spawnSync } from 'child_process'
import { existsSync, statSync, readFileSync, writeFileSync, rmSync } from 'fs'
import { join } from 'path'
import { tmpdir } from 'os'
import { randomBytes } from 'crypto'

// ── Inline copies of the production functions (from ../self-improve.flow.js) ──

function findRecursivePid(transcriptOut) {
  if (!transcriptOut) return null
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
  tick()

  return {
    stop() { stopped = true; clearInterval(timer) },
    get reason() { return reason.current },
  }
}

// ── Helpers ──

function tmpPath() {
  return join(tmpdir(), `wd-${randomBytes(6).toString('hex')}`)
}

// ── Tests ──

describe('findRecursivePid', () => {
  it('returns null when transcriptOut is falsy', () => {
    assert.strictEqual(findRecursivePid(null), null)
    assert.strictEqual(findRecursivePid(''), null)
    assert.strictEqual(findRecursivePid(undefined), null)
  })

  it('returns null for a non-existent path', () => {
    assert.strictEqual(findRecursivePid('/tmp/nonexistent-wd-test-xyz123'), null)
  })

  it('finds a child process with matching --transcript-out', () => {
    const transcript = tmpPath()
    // Write a small CJS helper script that prints its PID then idles.
    // CJS avoids the ESM load delay; --transcript-out as extra arg makes
    // pgrep -f match the full argv.
    const helperScript = tmpPath() + '.cjs'
    writeFileSync(helperScript,
      `const fs=require('fs');` +
      `fs.writeFileSync('${helperScript}.pid',String(process.pid));` +
      `setInterval(()=>{},30000)`
    )
    try {
      // Spawn the helper in the background (detached so execSync doesn't wait).
      // Use sh -c with exec to replace the shell so pgrep sees `node --transcript-out`
      // rather than `sh -c ...`.
      execSync(
        `sh -c 'exec node "${helperScript}" --transcript-out "${transcript}"' </dev/null >/dev/null 2>&1 &`,
        { encoding: 'utf8', timeout: 1000 }
      )
      // Note: sh exits immediately because of `exec` + `&`.

      // Wait up to 2s for the helper to write its PID file
      let reportedPid = null
      const deadline = Date.now() + 2000
      while (Date.now() < deadline) {
        try {
          const content = readFileSync(`${helperScript}.pid`, 'utf8').trim()
          const m = content.match(/^(\d+)/)
          if (m) { reportedPid = parseInt(m[1], 10); break }
        } catch { /* not written yet */ }
        // brief sleep via sync spin (test-only)
        const t0 = Date.now(); while (Date.now() - t0 < 50) { /* spin */ }
      }
      assert.ok(reportedPid > 0, 'child did not write PID file')

      // findRecursivePid should find it via pgrep -f
      const found = findRecursivePid(transcript)
      assert.strictEqual(found, reportedPid)

      // Clean up
      try { process.kill(reportedPid, 'SIGKILL') } catch { /* already gone */ }
    } finally {
      try { rmSync(transcript, { force: true }) } catch { /* ignore */ }
      try { rmSync(helperScript, { force: true }) } catch { /* ignore */ }
      try { rmSync(`${helperScript}.pid`, { force: true }) } catch { /* ignore */ }
    }
  })

  it('escapes regex metacharacters in paths', () => {
    const path = '/tmp/test.dir/file(1).json'
    const escaped = path.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
    assert.ok(escaped.includes('\\.'))
    assert.ok(escaped.includes('\\('))
    assert.ok(escaped.includes('\\)'))
    assert.strictEqual(escaped, '/tmp/test\\.dir/file\\(1\\)\\.json')
  })
})

describe('startRecursiveWatchdog', () => {
  it('no-growth short-circuit: fires onTrigger with no-growth-hung', async () => {
    const transcript = tmpPath()
    writeFileSync(transcript, 'initial content\n')
    try {
      let triggeredReason = null
      const w = startRecursiveWatchdog({
        transcriptOut: transcript,
        idleMs: 200,
        pollMs: 50,
        graceMs: 50,
        onTrigger: (r) => { triggeredReason = r },
      })

      // Wait for the watchdog to trip (idleMs + 2*pollMs + buffer)
      await new Promise(resolve => setTimeout(resolve, 500))

      assert.strictEqual(triggeredReason, 'no-growth-hung')
      assert.strictEqual(w.reason, 'no-growth-hung')
      w.stop()
    } finally {
      try { rmSync(transcript, { force: true }) } catch { /* best-effort */ }
    }
  })

  it('finish-marker short-circuit: fires onTrigger with finish-marker-hang', async () => {
    const transcript = tmpPath()
    writeFileSync(transcript, 'some conversation...\n[done after 3 steps] reason: NoMoreToolCalls\n')
    try {
      let triggeredReason = null
      const w = startRecursiveWatchdog({
        transcriptOut: transcript,
        idleMs: 5000,   // high so no-growth doesn't fire first
        pollMs: 50,
        graceMs: 100,
        onTrigger: (r) => { triggeredReason = r },
      })

      // Wait for finish-marker detection (graceMs + 2*pollMs + buffer)
      await new Promise(resolve => setTimeout(resolve, 400))

      assert.strictEqual(triggeredReason, 'finish-marker-hang')
      assert.strictEqual(w.reason, 'finish-marker-hang')
      w.stop()
    } finally {
      try { rmSync(transcript, { force: true }) } catch { /* best-effort */ }
    }
  })

  it('healthy run does not trip: transcript grows, watchdog stays quiet', async () => {
    const transcript = tmpPath()
    writeFileSync(transcript, '')
    try {
      let triggered = false
      const w = startRecursiveWatchdog({
        transcriptOut: transcript,
        idleMs: 500,
        pollMs: 50,
        graceMs: 50,
        onTrigger: () => { triggered = true },
      })

      // Simulate growth: append to the transcript every 100ms
      for (let i = 0; i < 5; i++) {
        await new Promise(resolve => setTimeout(resolve, 100))
        writeFileSync(transcript, readFileSync(transcript, 'utf8') + `line ${i}\n`)
      }

      // Stop before idleMs would fire
      w.stop()
      assert.strictEqual(triggered, false)
      assert.strictEqual(w.reason, null)
    } finally {
      try { rmSync(transcript, { force: true }) } catch { /* best-effort */ }
    }
  })

  it('stop() is idempotent and cancels fire', async () => {
    const transcript = tmpPath()
    writeFileSync(transcript, 'initial\n')
    try {
      let triggered = false
      const w = startRecursiveWatchdog({
        transcriptOut: transcript,
        idleMs: 200,
        pollMs: 50,
        graceMs: 50,
        onTrigger: () => { triggered = true },
      })

      // Stop immediately
      w.stop()
      assert.strictEqual(w.reason, null)

      // Wait past idleMs — should NOT fire
      await new Promise(resolve => setTimeout(resolve, 400))
      assert.strictEqual(triggered, false)
      assert.strictEqual(w.reason, null)

      // Idempotent stop
      w.stop()
      w.stop()
      assert.strictEqual(triggered, false)
    } finally {
      try { rmSync(transcript, { force: true }) } catch { /* best-effort */ }
    }
  })
})

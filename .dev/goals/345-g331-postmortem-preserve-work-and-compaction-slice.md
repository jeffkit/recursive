# Goal 345 — Post-mortem g331: preserve in-flight work on flow kill + compaction slice correctness + wall-clock exit

**Roadmap**: Hardening (self-improve loop reliability + compaction correctness) — incident-driven

**Design principle check**:
- Compaction fix: implemented in `src/compact/mod.rs` (split-point selection) — no new branch in `run_core.rs::RunCore::run_inner`.
- Wall-clock exit: implemented as a CLI flag + runtime deadline check in the step loop's existing budget/termination decision points — NOT a new capability branch inside `run_inner`.
- Flow data-preservation: implemented in `.dev/flows/self-improve.flow.js` signal handlers — dev/flow code, not product.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.

## Why

On 2026-07-23 the self-improve flow ran Goal 331 (circuit breaker for compaction failures).
The agent **finished the work** — transcript ends with `# Goal 331 — Complete 🎉`, all quality
gates passed (cargo test ok, clippy clean). But the run **lost everything**: HEAD stayed at
`fff351b` (Goal 330), the worktree + journal were deleted, no commit landed.

Root cause (reconstructed from evidence, NOT the "stdout pipe deadlock / 5h timeout" story
an earlier analysis told — that story is wrong on both counts):

1. **The `recursive` process never exited cleanly.** Hard evidence:
   - session `.meta.json` status stayed `"active"` — `close()` / `SessionEnd` never ran;
   - flow `events.jsonl` / `state.json` stuck forever at `run.recursive` **start** (no `done`);
   - transcript's last entry (16:54:37) is a cross-turn **compaction** that produced a
     **garbage summary**: `[compacted: 1 messages → 1448 chars at step 0]` →
     "No conversation found to summarize. The provided content consists entirely of project
     configuration files (AGENTS.md, CLAUDE.md, a journal entry, and skill listings)".
   - Total elapsed was ~60 min (16:28→17:28), NOT 5h. The flow's `run.recursive` timeout is
     `RUN_TIMEOUT_MS = 7_200_000` (2h, `.dev/flows/self-improve.flow.js:180`) — it never fired.
2. **The flow was killed externally at ~17:28** (worktree dir mtime). On SIGTERM/SIGINT the
   flow's signal handler (`self-improve.flow.js:598-599` `onSigint`/`onSigterm`) calls
   `cleanupWt()` unconditionally → `git worktree remove --force` → **deletes the worktree
   and the journal inside it**, destroying in-flight work. The `finally` block (line 647)
   correctly skips cleanup for `*-preserved` verdicts, but the **signal handlers don't check
   run state** — they always delete.
3. **The "stdout pipe deadlock" theory is wrong.** `spawnCapture`
   (`.dev/flows/node_modules/flowcast/spawn.js:45-92`) attaches a flowing `proc.stdout.on('data', append)`
   listener that continuously drains the pipe — the `recursive` process cannot block on stdout.
   Completion is detected via `proc.on('close')` (process exit + stdio close), NOT by parsing
   stdout. The g331 log freezing at step 55 (16:31) while the agent ran until 16:54 is the
   **documented stdout FILE buffering** (`launch-flow.sh` header comment warns about exactly
   this "日志冻住的假象"). Do NOT "fix" a pipe deadlock — there is none.

The deep irony: Goal 331 was about taming compaction failures, but the running binary was the
**baseline (Goal 330) release binary** built at preflight — the agent's new circuit-breaker code
was never compiled into the running process. The end-of-run compaction mis-fired (summarized the
system prompt into garbage) and the baseline code had no breaker to skip it.

## Scope (do exactly this, no more)

### 1. Compaction slice correctness — `src/compact/mod.rs`

The bug: `safe_split_point` + `apply_to_transcript` can select a slice that is **just the system
prompt** (or a degenerate 1-message slice), producing a useless "no conversation found" summary
and corrupting the transcript at the worst possible moment (end of run).

Fix the split-point selection so it NEVER produces a degenerate compaction:
- **Never compact the system prompt into the summary.** The system message (and any
  project-context preamble) must be retained verbatim in the kept segment, not fed to the
  summarizer. If `safe_split_point` would put the system message in the `older` (to-summarize)
  slice, advance the split past it so the system message stays in `kept`.
- **Reject degenerate slices.** If after adjustment the `older` slice contains **no User/Assistant
  conversation** (e.g. only system/config messages, or fewer than a small minimum of real
  conversational messages), `apply_to_transcript` must return `Ok(None)` — treat it as "too short
  / nothing useful to compact" rather than emitting a garbage summary. Define a sensible minimum
  (e.g. at least 2 non-system conversational messages in `older`).
- Keep the existing tool-call/tool-result pairing invariant (`safe_split_point`'s back-up logic
  for `Role::Tool` and `Assistant`-with-`tool_calls`) intact.

### 2. Tests — `src/compact/mod.rs` (`#[cfg(test)] mod tests`)

- `safe_split_point` keeps the system message in `kept` when `keep_recent_n` would otherwise
  push it into `older`.
- `apply_to_transcript` returns `Ok(None)` when the only thing in `older` is the system prompt /
  config (the g331 scenario: a transcript where compaction would summarize just the system
  message → must refuse instead of producing "no conversation found").
- `apply_to_transcript` still compacts normally when `older` has real conversation.
- Existing pairing-invariant tests still pass.

### 3. Flow: don't delete in-flight work on kill — `.dev/flows/self-improve.flow.js`

The signal handlers (`onSigint`/`onSigterm`, ~lines 598-599) and the `exit` handler call
`cleanupWt()` unconditionally. When the flow is killed mid-`run.recursive` (no verdict yet),
this destroys the worktree + journal.

Fix: when a signal arrives **before a terminal verdict** (i.e. `result` is still unset / the
run hasn't reached `committed` / `*-preserved`), **preserve the scene instead of deleting it**.
Concretely:
- Track whether `runAttempt` has produced a verdict (`result`).
- In the signal handlers, if no verdict yet, call `preserveScene({ worktreeDir, baseline, reason:
  'killed by signal (SIGTERM/SIGINT) mid-run', failureOutput: '<signal>', tag: 'killed' })`
  (move worktree to `.worktrees/preserve/<run-id>/`, tag `refs/preserve/<run-id>`) — same
  preservation path failures already use — and write a `failure-context` so `--resume-preserve`
  can pick it up. Only `cleanupWt()` if a non-preserved terminal verdict was already reached.
- Make sure `preserveScene` is safe to call from a signal handler (it currently does git ops +
  file writes — wrap in try/catch, never throw out of the handler; if preserve fails, fall back
  to leaving the worktree in place rather than force-removing it).

### 4. Wall-clock exit for the agent — `crates/recursive-cli` + `src/runtime.rs` (or wherever the step loop's termination is decided)

Today the CLI has `max_steps` and `shell_timeout_secs` but **no overall wall-clock deadline**.
If the agent hangs (e.g. on a compaction LLM call that never returns, or any await that never
resolves), the process never exits, the flow's `proc.on('close')` never fires, and the run is
lost. Add a wall-clock deadline:
- New CLI flag `--wall-timeout <secs>` (and env `RECURSIVE_WALL_TIMEOUT_SECS`), default 0 = unset
  (no wall-clock cap, preserves current behavior). The flow will pass a value < 2h.
- When set, the step loop checks the deadline at its existing termination-decision point
  (alongside `enforce_transcript_budget` / `max_steps`). On deadline exceeded: stop the loop,
  persist the transcript, print the normal finish marker (`[done after N steps] reason: ...`
  with a `WallClockExceeded`-style finish reason — **data, not an error**, per invariant #7),
  and **exit cleanly** so the flow's `close` event fires.
- Add the finish reason as a new `FinishReason` variant in `src/agent/types.rs` if needed,
  following invariant #7 (the CLI persists transcript BEFORE deciding exit code; never
  short-circuit the transcript save). Add a regression test mirroring
  `tests/invariants/finish_reason_data.rs` expectations.

### 5. Flow: don't wait 2h when the agent is done but `close` didn't fire — `.dev/flows/self-improve.flow.js`

Even with a wall-clock exit, add a watchdog in the flow's `run.recursive` step so a hung
`recursive` process (close never fires) is detected and preserved promptly:
- Watch the transcript file (`transcriptOut`): if it has shown a finish marker
  (`[done after N steps] reason:`) OR has not grown for N minutes (e.g. 10) while `close`
  hasn't fired, actively `SIGTERM` the `recursive` process (so `close` fires), then treat the
  outcome as a **preserved scene** (`preserveScene`, tag `timeout`/`hung`), NOT a 2h wait.
- Keep the existing `RUN_TIMEOUT_MS` 2h as the hard ceiling; the watchdog just short-circuits
  the obvious-hung case faster and preserves data instead of letting an external kill delete it.

## Acceptance
- `cargo test --workspace` green; `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --all` clean.
- New compaction tests in `src/compact/mod.rs` pass and pin the g331 scenario (system-prompt-only
  slice → `Ok(None)`, not a garbage summary).
- New wall-clock test: deadline exceeded → clean exit with transcript persisted + finish marker
  printed (finish reason is data, not an error).
- `tests/invariants/*` all still pass (loop_size, sandbox, pairing, finish_reason_data).
- Flow changes are JS — NOT covered by cargo gates. **The agent must `node --check
  .dev/flows/self-improve.flow.js`** after editing it, and reason carefully about the signal
  handler change (it runs in a signal context — no throws, no async that can reject uncaught).

## Notes for the agent
- **Read `.dev/AGENTS.md` invariants first** — especially #1 (loop stays small), #5 (no unwrap),
  #7 (finish reasons are data), #8 (tool-call pairing).
- The compaction slice bug lives in `Compactor::safe_split_point` / `apply_to_transcript`
  (`src/compact/mod.rs`). The g331 transcript evidence: `compacted: 1 messages → 1448 chars at
  step 0` summarizing "project config files (AGENTS.md, CLAUDE.md, journal, skill listings)" =
  the system prompt. Reproduce that shape in a unit test first.
- The flow signal-handler bug is at `.dev/flows/self-improve.flow.js:594-600` (`cleanupWt` /
  `onSigint` / `onSigterm`); `preserveScene` is at line 370; the `finally` skip-pattern is at
  line 647 — mirror it in the signal handlers.
- The flow runs `self-improve.flow.js` from the **main checkout**, not the worktree, so editing
  it in the worktree is safe (won't affect the in-flight run). It lands via cherry-pick.
- **DO NOT** "fix" a stdout pipe deadlock — `spawnCapture` already drains stdout via a flowing
  listener and detects completion via `proc.on('close')`. The log-freezing is documented file
  buffering. Touching `spawn.js` is out of scope.
- **DO NOT modify** `src/run_core.rs::RunCore::run_inner` body (add the wall-clock check at the
  existing termination-decision point, not as a new branch), `Cargo.toml` (no new deps), or
  anything under `e2e/` unless a test genuinely requires it.
- The lost Goal 331 work is recoverable from the session transcript at
  `~/.recursive/workspaces/d5340806ecbf/sessions/...selfimprove-1784795307389.../transcript.jsonl`
  (1.28MB, contains the full circuit-breaker patch) — but that is a SEPARATE follow-up, NOT this
  goal. This goal is the hardening so it can't happen again.

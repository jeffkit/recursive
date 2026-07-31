---
type: Skill
name: loop-supervise
description: "Monitor+intervene playbook for the event-driven /loop. Use when the user wants to run a long-running command and watch it, intervening only when it needs a decision or fix it can't make itself. Generic pattern first; includes a dedicated section for Recursive's own self-improve flow (.dev/flows/self-improve.flow.js) with the flow-specific launch args, event schema, and intervention rules (the recursive-loop skill is referenced where it exists, but this skill is self-contained for the flow too)."
mode: trigger
triggers: supervise, monitor, watch, 盯, 盯着, 盯住, 长跑, loop, 跑着, 看着
---

# loop-supervise — Monitor + intervene for the event-driven loop

## When to use

The user wants to **run a long-running command and watch it**, stepping in only
when it needs a decision or a fix the command can't make itself — and otherwise
letting it run to a terminal outcome. This skill teaches the *pattern*; the
command itself comes from the user's natural-language prompt.

This is the generic pattern. It also carries a **dedicated section below**
("Recursive self-improve flow — flow-specific rules") for Recursive's own
self-improve flow (`.dev/flows/self-improve.flow.js`): the launch args, event
schema, and the flow-specific traps that the generic SOP doesn't cover. If
the `recursive-loop` skill exists in this checkout it has the same info in
more depth; either is fine to follow.

## Tools (use them; if one isn't in your eager tool list, `tool_search` for it by name)

- `run_background` — spawn the command non-blocking; you get a `job_id`. The
  loop arbiter is woken **automatically when this job terminates**
  (success / fail / timeout).
- `check_background` — poll a job's status/output. Captured stdout/stderr is
  capped at 128 KB; for long logs, read the file the command tees to instead.
- `watch_file` — register a file (e.g. an events log) for **mid-run event
  wakes**. The arbiter polls it and wakes you only when new bytes appear — this
  is how you get *timely* intervention without burning a turn every tick.
- `schedule_wakeup` — your fallback heartbeat. Call it at end of turn to re-arm
  the next wake after N seconds (1–3600). If you arm neither a wakeup nor a
  watch, and no bg job is pending, the loop idles until the user speaks.
- `stop_loop` — end the loop yourself. Call it when the supervised command has
  reached a final outcome and you've reported the verdict, **or** when the user
  asks to stop / exit the loop in natural language ("停", "stop", "退出循环").
  The loop stops after the current turn; the user doesn't need to type
  `/loop stop`.

## SOP

1. **Derive the command.** From the user's natural-language prompt, work out
   the actual shell command to run. If it's ambiguous, ask before launching.
2. **Launch.** Run the command via `run_background`, teeing output to a known
   log file: `run_background` with command `sh -c '<command> 2>&1 | tee <log-path>'`.
   Pick `<log-path>` next to the command's run dir.
3. **Arm event-watch.** If the command emits structured events to a file (one
   JSON event per line), call `watch_file` on that file so you're woken on each
   event. Otherwise `watch_file` the tee'd log (you wake on each new chunk).
4. **Arm a heartbeat.** `schedule_wakeup` with a delay matching the command's
   cadence — long enough that idle ticks aren't pure overhead (e.g. 120–300s).
   This is your safety net if neither bg-completion nor watch fires.
5. **On each wake** (bg-complete / event-watch / heartbeat / user):
   - **Probe liveness FIRST.** Before concluding "healthy / no intervention",
     verify the supervised process is actually still alive — a **dead**
     process produces no new log/event bytes, which looks identical to
     "alive but slow". Check the `run_background` job status (or, for a
     tmux/nohup launch, `pgrep -f <run-id>` / the tmux pane's foreground
     process). Only say "healthy, no intervention" if the process is alive
     OR you've seen a terminal event/verdict. If the process is gone AND
     no terminal event (e.g. `verdict` / `fatal` / a non-zero exit) was
     emitted AND `state.json` still says `running` → that's a **crash,
     intervene** (see below), not "idle". This is the single most
     important check — without it you will narrate "No intervention
     needed" over a corpse (observed 2026-07-23: a flow crashed in
     preflight, supervisor idled indefinitely watching its corpse's
     events.jsonl).
   - Read the new log lines / event payload since last check.
   - **Healthy progress** (process alive OR terminal event seen) → re-arm
     the heartbeat (maybe lean the delay longer). Do NOT intervene.
   - **Crashed** (process gone, no terminal event, state still `running`)
     → **intervene**: read the tmux pane / tee'd log tail for the stack
     trace, diagnose, apply the minimal fix (PATH/dep/config), then
     re-launch or resume. Do NOT just re-arm the heartbeat over a dead
     process.
   - **Recoverable problem** (missing prerequisite, transient error, a config
     the command can't fix itself) → **intervene**: diagnose (read the command's
     source/config), apply the minimal fix (edit, install dep, start a service),
     then re-launch or resume. Prefer the command's own resume mechanism if it
     has one.
   - **Decision only a human can make** (an opt-out policy choice, a destructive
     action, an ambiguous spec) → **ask**: state the question crisply as your
     final message and **do NOT arm `schedule_wakeup`** — the loop pauses, and
     the user's reply drives the next turn. If a HITL MCP tool (e.g.
     `send_and_wait_reply`) is in your tool list, you may call it to block for a
     reply instead.
   - **Command terminated** → read its verdict / exit code, handle the result,
     then stop (see step 6).
6. **Stop.** When the supervised command has reached a final outcome and you've
   handled it, call `stop_loop` (the loop exits after this turn) and report the
   outcome to the user. Likewise call `stop_loop` if the user asks in natural
   language to stop / exit the loop. Do not arm any further wake.

## Discipline

- Don't intervene on every hiccup — only on problems the command can't
  self-heal. Many long-running commands retry internally; let them.
- **For gated commands** (a test/lint/build suite run in stages with an
  `onFail` policy): learn the policy before judging. `onFail: resume-fix`
  means a red gate is fed back to the worker as more work — the worker is
  *already* fixing it, often by re-invoking the very gate script you see
  "failing" in its transcript. That is the mechanism working, not the worker
  flailing. Step in only when the fix-round budget is exhausted (watch the
  `gate.<name>.fix-N` counter and the configured `MAX_FIX_ROUNDS`) or when
  the failure is environmental and the worker structurally cannot fix it
  (e.g. a missing service it can't install).
- Keep interventions minimal and surgical.
- In one or two lines per wake, note what you observed and what you did, so the
  user can follow along.

## Recursive self-improve flow — flow-specific rules

The generic SOP above covers launch → watch → intervene → stop. When the
supervised command is **Recursive's own self-improve flow**
(`.dev/scripts/launch-flow.sh` → `.dev/flows/self-improve.flow.js`), these
extra rules apply. They are distilled from real supervised runs; the generic
SOP alone will steer you into at least four avoidable traps.

The flow runs inside a **tmux session**, is **resumable by `--run-id`**, and
writes everything under `<repo>/.flowcast/runs/<run-id>/` (`state.json`,
`run.log.jsonl`, `report.md`, `system-prompt.md`, `transcript.json`).
Verdicts: `committed` (green + review passed → on `main`) ·
`failed-preserved` (gate/review loop exhausted → worktree +
`refs/preserve/<run-id>` kept, **not** rolled back) · `skip-commit` ·
`panic-preserved`.

**Flow-specific traps (do not learn these the hard way):**

1. **Never pre-compile before launch.** The flow's `preflight.build` step
   builds `recursive-cli` itself. If you run `cargo build --release` in
   parallel, you contend on the `target/` directory lock and `preflight.build`
   fails with a misleading error (`invalid character ' ' in package name:
   ' recursive-cli'` — a corrupted lock state, not a real package-name bug).
   Let the flow own the build. (Observed 2026-07-31.)

2. **Each launch writes a NEW timestamped log file.** `launch-flow.sh`
   generates `.flowcast/logs/flow-<TS>.log` afresh on every invocation, so
   after a resume/re-launch the previous log path is stale. Derive the current
   log from the run dir's `events.jsonl` mtime, or glob the newest
   `.flowcast/logs/flow-*.log` — never assume the last run's path still
   applies.

3. **Resume requires `--goal-file` again.** `--run-id <id>` alone errors with
   `缺少 --goal 或 --goal-file`. The run-id reuses the run *directory* for
   outputs; it does **not** restore the goal text. Always pass both:
   `launch-flow.sh --run-id <id> --goal-file .dev/goals/NN-*.md --provider ...`

4. **Diagnose without polluting `main`.** The flow refuses a dirty `.dev/`
   (`withSelfModGuard`), so any patch to `.dev/flows/*.js` must be committed
   before it can run — and forgotten `wip(diag)` commits then sit on `main`
   ahead of the real goal commit (observed: two throwaway diagnostic commits
   polluted history). Prefer a **standalone reproducer in `/tmp`** (a CJS
   `node -e` or `.js` file) over patching the flow script. If you must patch
   the flow, commit → run → `git revert` (never `reset`; the goal commit sits
   on top and revert keeps history honest).

These four are flow-specific; the two cross-cutting rules — **probe liveness
before declaring healthy** (SOP step 5) and **don't intervene on a gate the
worker is already fixing** (Discipline, `onFail: resume-fix`) — are already
covered above and apply to the flow too.

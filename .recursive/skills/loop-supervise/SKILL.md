---
type: Skill
name: loop-supervise
description: "Generic monitor+intervene playbook for the event-driven /loop. Use when the user wants to run a long-running command and watch it, intervening only when it needs a decision or fix it can't make itself. Project-agnostic; for Recursive's own self-improve flow, prefer the recursive-loop skill."
mode: trigger
triggers: supervise, monitor, watch, 盯, 盯着, 盯住, 长跑, loop, 跑着, 看着
---

# loop-supervise — Monitor + intervene for the event-driven loop

## When to use

The user wants to **run a long-running command and watch it**, stepping in only
when it needs a decision or a fix the command can't make itself — and otherwise
letting it run to a terminal outcome. This skill teaches the *pattern*; the
command itself comes from the user's natural-language prompt.

This is the generic, project-agnostic version. If the user is asking to run
**Recursive's own self-improve flow** (`.dev/flows/self-improve.flow.js`), use
the `recursive-loop` skill instead — it has the project-specific launch args,
event schema, and intervention rules.

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
- Keep interventions minimal and surgical.
- In one or two lines per wake, note what you observed and what you did, so the
  user can follow along.

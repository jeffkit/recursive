# Issue #47 triage criteria — "0% CPU + all threads parked" is NOT a deadlock

Date: 2026-09-29
Goal: Record the diagnostic criteria that distinguish a healthy agent waiting
on an LLM network response from a genuine hang, so future triage (#40-style
reports) does not misclassify normal parked waiting as deadlock.

## Criteria

**0% CPU + all threads parked + "no network" ≠ deadlock.** Async task state
lives on the heap, not the stack; while the agent awaits an LLM network
response, every tokio worker being parked is the *normal* state. In the #47
investigation, the "stuck" round self-healed at 315s and completed normally
at 435s — it was simply a long gateway response.

To decide, look at (in order):

1. **Confirm a request is actually in flight** — is there a pending LLM call
   for this turn? If yes, park is expected.
2. **Connection byte activity** to the LLM gateway: `nettop`, `tcpdump`,
   `netstat -b`, or `lsof -i`. Incrementing bytes ⇒ alive, keep waiting.
3. **Provider timing logs** — `src/llm/openai.rs:66-72` sets a reqwest
   timeout of 180s; a genuinely hung request errors out by then. If 180s has
   passed with no timeout error AND no byte activity, then and only then
   suspect a hang.

Only when (1) fails or (2)+(3) both show no activity for > 180s should the
run be treated as hung.

## Errata for #40 ("无 TCP" / "no TCP connections" observation)

The #40 report classified a round as deadlocked partly on "no TCP
connections to the gateway". That observation was a sampling artifact: the
snapshot moment actually had 3 ESTABLISHED connections to the gateway (the
report said "无 TCP"; re-inspection of the same capture shows them). The
correct criterion is **byte-activity deltas** on those connections, not a
single instantaneous connection listing. With that correction, the #40
"deadlock" evidence is consistent with normal parked waiting on an in-flight
LLM response, which self-resolved.

*(This file is the in-repo errata record; the orchestrator mirrors this
correction to the #40 entry in the issue tracker.)*

## Addendum (2026-09-30, from #48 re-verification): assert the binary contains the fix BEFORE judging it

A #48 re-verification ran against a stale `target/debug/recursive` (built before
the fix commit), produced a confident false "the fix doesn't work" conclusion,
and cost a full wasted round — third occurrence of this exact mistake in one
session. Symmetric with the byte-activity rule above (validate the measuring
instrument before trusting its verdict), re-verification must first assert the
artifact under test actually contains the fix:

1. **Build postdates the fix**: `stat -f '%Sm' <binary>` must be newer than
   `git log -1 --format=%ci <fix-commit>`.
2. **Fix marker present**: `strings <binary> | grep -c '<literal introduced by
   the fix>'` must be ≥ 1 (for #47④ that literal is `plan approval timed out`).

A binary failing either check is not evidence about the fix — rebuild and
re-run before drawing any conclusion.

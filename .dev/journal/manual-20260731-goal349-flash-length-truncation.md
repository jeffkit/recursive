Date: 2026-07-31
Goal: 349 (TUI select & copy agent output) — first self-improve run postmortem
Files touched: none (this is a journal of a failed run, not a code change)
Tests added: none

## What happened

First flash run `selfimprove-1785484982049` (provider=deepseek-v4-flash,
reviewer=deepseek-v4-flash) completed in 231.9s with verdict=`skip-commit`,
files_changed=0, "no changes produced".

Root cause is NOT context-window exhaustion and NOT a crash. It is a
**per-turn output-budget failure on the agent's final turn**:

- `run.recursive` exitCode 0, finishReason=`provider_stop:length`, 32 messages / 13 steps.
- Last assistant message (msg 31): `content=""` (empty — no tool call emitted)
  but `reasoning_content` = **60677 chars**. The agent spent its entire
  single-turn output budget on a sprawling internal plan ("Let me plan the
  implementation: 1. arboard 2. App fields 3. App::new 4. clear-on-scroll ...")
  and was truncated by provider_stop:length BEFORE it could emit any tool call
  to actually edit files.

So the agent understood the goal, read all the referenced files (chat.rs,
commands.rs 340 lines, lib.rs, mod.rs, harness.rs, transcript.rs, model.rs,
events.rs, keymap.rs, ui/mod.rs) and reasoning'd itself into a 60k-char plan,
then ran out of output tokens mid-plan. No edit ever landed.

## Why provider_stop:length didn't auto-resume

flow only auto-resumes on `BudgetExceeded` (matched by
RECURSIVE_BUDGET_EXCEEDED_RE), not on `provider_stop:length`. So the run
terminated as skip-commit instead of resuming. This is a gap worth noting
but not fixed here.

## Supervisor intervention

User directed to retry with flash (original command) rather than switch to
deepseek-pro or split the goal. Second run `selfimprove-1785485427092`
launched 2026-07-31T16:10:26 with the same flash provider.

Watch signal for the retry: if run.recursive's finishReason is
provider_stop:length again AND the last assistant content is empty with a
huge reasoning_content, it's the same failure — flash's per-turn output cap
is too small for an agent that reasons expansively before acting on a
detailed multi-section goal. Candidate fixes if it recurs:
  (a) split goal 349 into 349a (deps + App fields + init) and 349b (mouse
      selection + render + key bindings) so each run reasons over fewer
      sections;
  (b) switch to deepseek-pro (larger per-turn output);
  (c) check whether recursive exposes a reasoning_effort / max-tokens knob
      to cap reasoning length.

## Lesson

A detailed goal (lots of precise file:line + 6 implementation subsections)
is good for directing reads, but on a reasoning-mode model with a modest
per-turn output cap it can cause the agent to burn the whole output budget
on planning reasoning and never emit the edit. Symptom signature:
finishReason=provider_stop:length + empty content + large reasoning_content
+ files_changed=0.

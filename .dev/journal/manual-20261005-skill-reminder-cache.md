# Manual edit: skill-reminder-cache

**Date**: 2026-10-05
**Goal**: issue #95 (P1) — the per-step skill `<system-reminder>` was appended
as a `user` turn at the **tail** of the per-request message copy. Its byte
offset therefore drifted forward by one history turn per step, so the
provider's prefix cache could only ever hit up to the end of the *previous*
step's history and the whole catalog (≤ `RECURSIVE_SKILL_INDEX_BUDGET` bytes,
8000 by default) was re-billed at full price on every step — ~40k fresh tokens
over a 20-step turn. The same function also deep-copied the whole transcript on
every step (`messages.iter().cloned()`), including on the no-skills path.

**Files touched**

- `src/agent/types.rs` — `inject_skill_reminder` now appends the reminder to
  the **system message** of the per-request copy (fixed offset → step N's
  request is a byte-prefix of step N+1's). The system prompt is located by role
  within the leading two slots, because `RunCore::call_llm` prepends an
  `<available-deferred-tools>` user block when the registry has deferred tools;
  the bound also stops globs-matched skill injections (later `System` messages)
  from being mistaken for the prompt. Returns `Cow<'_, [Message]>` so the
  no-skills path borrows instead of copying. The static assembled system prompt
  (`system_prompt::assemble_system_prompt`) still excludes the catalog — the
  reminder stays a per-request decoration, so the stored transcript is never
  rewritten when skills load/unload.
- `src/run_core.rs` — `call_llm` consumes the `Cow` (deref-coerced) instead of
  a `Vec`; doc comments updated to describe the new placement.
- `src/skills.rs` — `skill_reminder` doc: no longer "placed in a user turn".
- `src/system_prompt.rs`, `src/http/handlers.rs` — test/doc comments that said
  the per-turn reminder "keeps the `system` field stable" corrected.

**Tests added** (`src/agent/types.rs`)

- `skill_reminder_is_appended_to_the_system_message` — content/role/position,
  no extra message, later messages untouched.
- `skill_reminder_targets_the_system_prompt_not_the_first_message` — the
  deferred-tools shape (`[user(deferred), system, ...]`).
- `consecutive_steps_share_a_byte_identical_request_prefix` — acceptance: two
  consecutive steps of one session, step N's serialized message list is a
  byte-identical prefix of step N+1's.
- `no_skills_borrows_the_transcript` — `Cow::Borrowed`.

**Rebase onto `origin/main`**

The branch was cut from a base that predates the loop-retry suppression, so
`git rebase origin/main` conflicted in `src/runtime.rs`: `origin/main` already
disables the per-step retry inside `run_loop` (`run_loop_inner`, see
`.dev/journal/manual-20261005-loop-retry-vs-step-retry.md`). The conflict was
resolved in favour of `origin/main` — this branch's `src/runtime.rs` hunk and
its `manual-20261005-loop-step-retry-stacking.md` journal are duplicates of that
landed fix and were dropped. The rebased commit touches only the files below.
`origin/main` then advanced to 4e1aee98 (#91, `session/reader.rs` +
`cli/resume.rs`) and the branch was rebased onto that too; no overlap, no
conflict.

**Gates** (on the rebased tree, base 4e1aee98)

- `cargo fmt --all -- --check` clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo test --workspace`: green, exit 0 — 4088 passed / 0 failed / 10 ignored.
  The `run_loop_wakeup_persist.rs::run_loop_gives_up_after_the_retry_budget`
  failure seen before the rebase came from the missing loop-retry fix and is
  gone.
- One unrelated flake on the way: `tests/resume_by_id.rs::
  lock_thread_safety_serialises_open_existing` is timing-dependent (20 ms head
  start racing a 150 ms hold) and failed once while the box was running several
  self-improve pipelines (load average ~80). It passes in isolation and in the
  final run; left untouched.

**Notes**

- Out of scope (issue #95 建议 2, partially): the skills-present path still
  clones the message vector once per step, and the Anthropic serializer clones
  it again (`extract_system_message` / `filter_leading_assistant`). Removing
  that needs a provider-trait signature change (`&[&Message]` or `Arc`-backed
  messages) — a much wider blast radius than this fix.

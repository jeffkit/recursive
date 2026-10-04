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

**Gates**

- `cargo fmt --all -- --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo test --workspace --no-fail-fast`: one failing target, **pre-existing and
  unrelated** — `tests/run_loop_wakeup_persist.rs::run_loop_gives_up_after_the_retry_budget`.
  `dispatch_llm_step_with_retry` (`src/run_core.rs`) re-issues the LLM call inside
  a turn (issue #100, `max_retries` default 2) while `MockProvider` pops one
  injected error per call, so a single `run_loop` turn eats all three injected
  errors and then gets the scripted completion — the turn succeeds and
  `run_loop` returns `Ok`, which is what the assertion rejects. Both involved
  commits (#99 `87451fb1`, #100 `22a973e9`) predate this change.
- `cargo test --lib` separately: 2639 passed, 1 failed —
  `http::auth::tests::auth_inscure_ok_toggles` is a cross-module env-var race
  (`src/http/handlers.rs` tests `set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1")`
  process-globally) and passes in isolation.

**Notes**

- Out of scope (issue #95 建议 2, partially): the skills-present path still
  clones the message vector once per step, and the Anthropic serializer clones
  it again (`extract_system_message` / `filter_leading_assistant`). Removing
  that needs a provider-trait signature change (`&[&Message]` or `Arc`-backed
  messages) — a much wider blast radius than this fix.

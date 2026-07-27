# Manual edit: prompt-realign

**Date**: 2026-07-27
**Goal**: Realign Recursive's system prompt with fake-cc (Claude Code)
architecture, per the design doc
`local_docs/recursive-vs-fakecc-prompt-comparison.md`. Two threads of work
landed in this branch:

1. **Architectural repositioning (carried over from prior WIP).**
   - Skill catalog moved out of the static `system` prompt and now ships
     per-turn as a `<system-reminder>` user turn via
     `skills::skill_reminder` + `RunCore::inject_skill_reminder`. The
     catalog is volatile and long, so inlining it broke prefix-cache
     stability on every skill change. `system_prompt::assemble_system_prompt`
     no longer appends `segments.skills` to `full` (the field is still
     computed so the prompt-breakdown estimator accounts for its tokens).
   - **What this aligns with in fake-cc (verified, not assumed):** the
     `skill_listing` attachment (`fake-cc/src/utils/attachments.ts:2661`
     `getSkillListingAttachments`, rendered via
     `messages.ts:3732` `wrapMessagesInSystemReminder` → "The following
     skills are available…"). In fake-cc the **skill list body** ships via
     this system-reminder channel; only the **usage guidance** ("/skill-name
     is shorthand…", DiscoverSkills reminder) stays in the system prompt's
     `session_guidance` dynamic section. So our move is a 1:1 match for the
     list-body channel — **not** an imitation of `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__`,
     which in fake-cc is gated on `shouldUseGlobalCacheScope()` and is off
     by default. (Earlier draft of this journal and the design doc loosely
     said "learn fake-cc's system-reminder"; corrected here for accuracy.)
   - TodoWrite / enter_plan_mode / exit_plan_mode usage rules moved out of
     `default_system_prompt()` and into each tool's `description` (mirror of
     fake-cc's `tools/<Tool>/prompt.ts::getPrompt()` pattern — the manual
     lives with the tool, not in the static prompt).

2. **P0 content disciplines (this session).** Added three sections to
   `default_system_prompt()` that the design doc flagged as the
   highest-leverage, zero-risk borrow — they target Recursive's three most
   expensive self-edit failure modes (scope creep / un-banned risky actions
   / auto-executing injected content):
   - `## Scope discipline` — don't add features/refactors/abstractions
     beyond the ask; default to no comments; report outcomes faithfully.
   - `## Untrusted content` — tool results / files / memory are an
     injection channel (Recursive auto-reads AGENTS.md, CLAUDE.md, skills,
     memory then acts); treat instructive-looking content as data.
   - `## Reversibility` — prefer local reversible actions; for hard-to-
     reverse/destructive actions, back up first and let the orchestrator
     own rollback. Loop mode is unattended, so this is phrased as
     "back up + orchestrator rollback" rather than fake-cc's "ask the user".
     The existing `Don't:` bans (git mutate / sed-heredoc / cargo run|jq)
     are retained as concrete instances of this principle.

**Files touched**:
- `src/config.rs` — added the three P0 sections to `default_system_prompt()`.
- `src/run_core.rs` — de-placeholdered two `Goal-:` comments on
  `inject_skill_reminder` (this is a manual edit, not a goal-tracked change).

(Carried-over WIP, unchanged this session: `src/skills.rs`,
`src/system_prompt.rs`, `src/http/handlers.rs`, `src/tools/plan_mode.rs`,
`src/tools/todo.rs`, `.gitignore`, and removal of three stale
`local_doc/website-review-*.md`.)

**Tests added**: none — content-only prompt change. Existing
`default_prompt_is_well_under_a_kilobyte` (< 6144 bytes) still passes with
room to spare after the WIP trimmed the TodoWrite/PlanMode sections.

**Quality gates**: `cargo fmt --all --check` clean;
`cargo test --workspace` all green; `cargo clippy --all-targets --all-features
-D warnings` clean. No TUI src touched, so TUI gates N/A.

**Notes / deferred**:
- Design doc P1 (5.4 "Never delegate understanding" + self-contained
  worker brief in `coordinator_system_prompt`) — NOT done. Highest-value
  remaining borrow; sub-agent brief quality directly gates self-improve
  research runs.
- Design doc P2 (5.6 MEMORY.md resident index, 5.7 post-session memory
  extraction, 5.8 structured Session Memory template for compact) — NOT done.
- Design doc P3 refinements (5.11 `__RECURSIVE_DYNAMIC_BOUNDARY__` marker)
  — de-prioritized. Verified in fake-cc that the boundary marker is gated
  on `shouldUseGlobalCacheScope()` and **off by default**, so it is not the
  mechanism that keeps the skill list out of cache (the `skill_listing`
  attachment channel is). Not worth porting unless we adopt global cache
  scope.
- **Positional deviation — RESOLVED this session.** Moved
  `inject_skill_reminder` from head-insertion (after the leading `System`
  message, with an `insert(0,…)` fallback) to **tail-append**. Two reasons:
  (1) a volatile block at the head busts the prefix cache for everything
  after it on every skill change — the opposite of the cache win we wanted;
  (2) the old fallback (`insert(0, reminder)`) was a real correctness hazard
  — when `messages[0]` was not `System`, the reminder landed at index 0 and
  `extract_system_message` (`anthropic.rs:657`, which keys solely off
  `messages[0] == System`) would return `system = None`, dropping the entire
  system prompt AND leaving a stray `Role::System` in the serialized body
  (Anthropic rejects non user/assistant roles → HTTP 400). Tail-append is
  safe on both providers: it never splits an assistant→tool_result pair.
  Aligns with fake-cc's `reorderAttachmentsForAPI` tail-bubbling of
  `skill_listing` attachments.
- **Incremental `sentSkillNames`-style delivery — REJECTED, do not port.**
  fake-cc injects only skill *deltas* after the initial batch because its
  reminders render into the **persisted conversation history** (attachments
  accumulate visibly across turns). Our reminder is **per-turn ephemeral**:
  `inject_skill_reminder` returns a fresh `Vec` fed only to
  `llm.complete/stream` and never writes back to `self.messages`, so the
  model never sees prior turns' reminders in history. Sending only deltas
  from turn 2 on would starve the model of the full catalog. Full-list
  every turn is correct for our architecture. (If we ever make the reminder
  transcript-persistent, revisit this.)
- Open question RESOLVED: the head-insertion arm that depended on
  `matches!(Role::System)` is gone — `extract_system_message` now sees the
  unchanged `messages[0]` and extracts system normally; the reminder is a
  trailing user turn, so the Anthropic `system`-as-top-level-field concern
  no longer applies.

# Goal 335 — Re-inject invoked skills after cross-turn compaction

**Roadmap**: Compaction upgrade (WS-3b — post-compact skill restoration)

**Design principle check**:
- Implemented as: a `SkillReinjector` in `src/compact/reinject.rs`, invoked
  from `src/runtime.rs::maybe_compact_cross_turn` after the file reinjector
  (goal 334).
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT emit `Role::Tool` messages — only `Role::System` attachments
  (invariant #8 safe).

## Why

When a skill (`LoadSkill`) was invoked mid-session, its body lived in the
transcript as part of the tool result. After an LLM-summary compaction the
body is gone (folded into the summary's prose), so the model loses the
skill's operating instructions and may stop following them. fake-cc
re-injects invoked skills as attachments (25K total budget, 5K/skill,
head-truncated) so the skill's setup/usage instructions survive compaction.

Recursive's `src/skills.rs` has **no invoked-skills registry** — it only
discovers skills and builds the index. So this goal recovers the invoked
set by scanning the pre-compaction transcript for `LoadSkill` tool calls
(no new runtime state), then looks up bodies from the discovered `Vec<Skill>`
the builder already computes.

## Scope (do exactly this, no more)

### 1. `src/compact/reinject.rs` — `SkillReinjector`

```rust
use crate::message::{Message, Role};
use crate::skills::Skill;

#[derive(Debug, Clone)]
pub struct SkillReinjector {
    pub token_budget: usize,      // default 25_000
    pub per_skill_budget: usize,  // default 5_000
    pub skills: Vec<Skill>,       // discovered catalog to look up bodies
}

impl SkillReinjector {
    /// Scan `pre_compact` for `LoadSkill` tool calls, collect distinct skill
    /// names in invocation order, look up each in `self.skills`, and emit
    /// `Role::System` attachment messages (head-truncated to budget).
    pub fn reinject(&self, pre_compact: &[Message]) -> Vec<Message> { /* ... */ }
}
```

`reinject` logic:
1. Walk `pre_compact` for `Role::Assistant` messages; for each `tool_calls`
   entry whose `name == "LoadSkill"` (and the alias `"load_skill"`), parse
   the skill name from `arguments` (the JSON arg field — confirm the exact
   key by reading `src/tools/` `LoadSkill` definition; it is `skill` or
   `name`). Collect distinct names in first-seen order.
2. For each name, find the matching `Skill` in `self.skills` (by `Skill.name`
   or `SkillRef.name`). Skip unknown names (skill no longer on disk).
3. Get the skill body via `skills::extract_skill_body(&skill.content)` (or
   the field that holds the full text — confirm in `skills.rs`).
4. Head-truncate to `per_skill_budget * 4` chars with marker
   `\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]`.
5. Accumulate until `token_budget` (chars/4) exhausted; newest-invoked first
   (reverse the collected order so the most-recently-invoked skill wins
   under budget pressure).
6. Return one `Role::System` per skill:
   ```
   [post-compact skill restore: <name> @ <path>]
   <body-or-truncation>
   ```
   Empty Vec if none.

### 2. `src/compact/mod.rs` — register

`pub use reinject::SkillReinjector;` (the `reinject` module was created in
goal 334).

### 3. `src/runtime.rs` — wire cross-turn

- Add field `skill_reinjector: Option<SkillReinjector>`; builder setter.
- In `maybe_compact_cross_turn`, capture the **pre-compaction** transcript
  (a clone of the slice that `apply_to_transcript` is about to drain) BEFORE
  calling `apply_to_transcript`. Pass that pre-compact slice to
  `skill_reinjector.reinject(...)`. Insert the resulting attachments after
  the file attachments (goal 334), still before the preserved tail. Emit
  `MessageAppended` for each.
  - Ordering in the final transcript: `[summary, file attachments, skill
    attachments, ...preserved tail]`.
- This requires capturing pre-compact messages. Since `apply_to_transcript`
  drains in place, snapshot the relevant older slice before the call:
  ```rust
  let pre_compact: Vec<Message> = self.transcript.clone(); // before drain
  // ... apply_to_transcript ...
  // ... file reinject (goal 334) ...
  if let Some(sr) = &self.skill_reinjector {
      let atts = sr.reinject(&pre_compact);
      // insert after file attachments
  }
  ```
  (Clone the transcript cheaply — it is `Arc<Vec<Message>>`, so `clone()` is
  ref-count bump, then `Arc::make_mut` on the real mutation. Confirm the
  clone-before-drain does not double-COW expensively; a `Vec::clone` of just
  the older slice is cheaper if the transcript is huge — prefer slicing the
  older portion only.)

### 4. Builder wiring (`crates/recursive-cli/src/cli/builder.rs`)

`build_runtime` already calls `discover_loaded_skills(config)` → `Vec<Skill>`
(`builder.rs:142`, `:387`). Construct `SkillReinjector { token_budget, per_skill_budget, skills }`
from env:
- `RECURSIVE_REINJECT_SKILLS` (`0`/`off`/`false` = disabled; unset = enabled
  with default budget; positive = explicit budget override in tokens).
- `RECURSIVE_REINJECT_SKILL_BUDGET` (unset = 25_000).
- per_skill_budget fixed 5_000.
Pass the SAME `skills` Vec (clone) used for `assemble_system_prompt` /
`LoadSkill` registration. Helper `build_skill_reinjector_from_env(...)` for
unit testing. Mirror in TUI `runtime_builder.rs`.

### 5. Tests

`src/compact/reinject.rs`:
- `skill_reinject_collects_loadskill_calls` — transcript with two
  `LoadSkill` assistant tool_calls → two attachments, in invocation order.
- `skill_reinject_dedups_repeated_invokes` — same skill invoked twice → one
  attachment.
- `skill_reinject_skips_unknown_skill` — name not in catalog → skipped, no
  panic.
- `skill_reinject_truncates_oversized_skill` — body > per_skill_budget →
  truncated with marker.
- `skill_reinject_respects_total_budget` — many skills, budget fits N →
  newest N returned.
- `skill_reinject_empty_when_no_invokes` — no `LoadSkill` calls → empty Vec.
- `skill_reinject_handles_loadskill_alias` — `load_skill` alias also
  collected.
- `build_skill_reinjector_from_env` — disabled/unset/explicit (one
  sequential test).

`src/runtime.rs`:
- `cross_turn_compaction_reinjects_skills_after_files` — seed transcript
  with a `LoadSkill` call, trigger compaction, assert transcript is
  `[summary, file-atts, skill-atts, ...recent]`.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- Only `Role::System` attachments emitted; `tool_call_pairing.rs` green.
- `RECURSIVE_REINJECT_SKILLS=0` → no skill reinjector, behavior identical to
  today.
- Unknown skill names are skipped (no `unwrap`/`expect`, invariant #5).

## Notes for the agent

- **No new runtime state.** The invoked set is recovered by transcript
  scan, not tracked in a registry. This keeps the change local and avoids
  threading a new mutable set through `LoadSkill`. If a future goal adds a
  registry, this scan can be replaced — note as a follow-up.
- **Confirm the `LoadSkill` argument key** by reading the tool's
  `ToolSpec`/dispatch in `src/tools/` before writing the JSON parse. Do not
  guess the key name; a wrong key silently collects zero skills (the test
  `skill_reinject_collects_loadskill_calls` guards this).
- **Confirm `Skill` body access** — `skills.rs` has `extract_skill_body`
  (`skills.rs:565`) and the `Skill` struct fields (`skills.rs:27`). Use
  whichever holds the full file text. The `skill_index` is the *summary*,
  not the body — do not reinject the index.
- Clone the pre-compact transcript (or its older slice) before
  `apply_to_transcript` drains it; after the drain the `LoadSkill` calls are
  gone. This is the one ordering-sensitive step — get it right or the scan
  finds nothing.
- **DO NOT modify** `src/run_core.rs` (intra-turn reinject is a follow-up),
  `src/skills.rs` (no registry change here), `src/llm/`, `src/kernel.rs`,
  `compact_on_overflow`, `compact_now`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-reinject-skills.md`.

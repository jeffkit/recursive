# Journal — 2026-10-01 — issue #61 continuation: residual "8 invariants" copies

## Date
2026-10-01

## Goal
Continuation of the pipeline-61 run (run id `pipeline-61-1001180648`, based on
`v2-pipeline-61-1001142105` / commit `0ee7ef8`). The numbering conflict itself
was already resolved by the two prior WIP commits (`7706ef2`, `0ee7ef8`):
route B adopted — ten invariants, `.dev/AGENTS.md` canonical,
`docs/architecture/invariants.md` renumbered to match, root `AGENTS.md`
demoted to an unnumbered pointer, and
`tests/invariants/invariant_registry.rs` extended with
`invariant_numbering_agrees_across_documents` +
`every_numbered_invariant_has_automated_enforcement`. This run swept the
**residual prose copies** the prior commits missed — documents that still
said "the 8 invariants" after the list became ten.

## Files touched
- `docs/architecture/index.md:54` — "the eight invariants every change must
  respect" → "the ten invariants".
- `docs/architecture/agent-loop.md:69` — "all eight invariants" → "all ten
  invariants".
- `.recursive/skills/self-improve-cycle/SKILL.md:219` — "do the 8 invariants
  still hold?" → "do the 10 invariants still hold?".
- `.zcode/skills/self-improve-cycle/SKILL.md:190` — same fix (duplicate skill
  copy; the guard test cannot see skill prose, so this was a manual sweep).
- `.dev/proposals/compaction-upgrade.md:39` — "preserving Recursive's 8
  invariants" → "10 invariants" (historical proposal; corrected so future
  readers don't re-import the stale count).

## Tests added
None — documentation-only sweep. The guard tests added in `7706ef2`/`0ee7ef8`
cover the two numbered documents; prose occurrences outside them have no
mechanical contract (and hard-coding "10" in a guard would re-create the same
stale-copy problem the next time the list grows to eleven).

## Verification
- `grep -rn "8 invariants|eight invariants|Eight Invariants"` across docs/,
  .recursive/, .zcode/, .dev/, README.md, AGENTS.md → only the journal
  entries that describe this fix remain.
- Negative tests (re-run by hand this session, against the prebuilt test
  binary while sibling worktrees held the cargo build lock):
  - Corrupted `## Invariant #2 — Orthogonality` title in the architecture doc
    → `invariant_numbering_agrees_across_documents` FAILED with a full
    (number → normalized-title) diff of both maps; restored → green.
  - Replaced the `Automated test:` lines of invariant #6 in `.dev/AGENTS.md`
    → `every_numbered_invariant_has_automated_enforcement` FAILED; restored →
    green.
  - `invariants` test binary: 46 passed, 0 failed.
- `git status` clean between negative tests (both files restored before
  proceeding).

## Notes
- The issue's suggested archive path `.dev/issues/09-invariant-numbering-conflict.md`
  does not exist in this worktree (only 02/03/ci/gate archives are present);
  not recreated.
- Acceptance criteria check against the issue:
  1. ✅ (number → title) mapping consistent across `.dev/AGENTS.md` and
     `docs/architecture/invariants.md`; root `AGENTS.md` carries no numbers.
  2. ✅ Automated guard: numbering mismatch fails
     `cargo test --test invariants`.
  3. ✅ Every numbered entry in `.dev/AGENTS.md` names an `Automated test:` or
     `Enforced by:` line, guarded by
     `every_numbered_invariant_has_automated_enforcement`.
  4. ✅ 8-vs-10 decision recorded (route B — ten invariants; #9/#10 are
     mechanically enforced by `invariant_registry.rs` module-layout tests,
     so the "give them tests" part of route B was already satisfied by
     `tool_files_are_registered_in_mod_rs` /
     `providers_live_in_llm_and_implement_the_trait`).
- Code references to invariant numbers (`src/runtime.rs`, `src/kernel.rs`,
  test files) all use numbers whose meaning is now identical in both
  documents (#1–#8) or refer to the newly consistent #9/#10.

# Journal — 2026-10-01 — issue #61 invariant numbering conflict

## Date
2026-10-01

## Goal
Resolve #61 (P1): `.dev/AGENTS.md` and `docs/architecture/invariants.md`
disagreed on what invariants #2/#4/#6 mean — while these documents are the
rollback criteria of the self-improve loop. Acceptance: single (number →
title) mapping across docs, an automated guard, root `AGENTS.md` demoted to a
pointer, author decision on 8 vs 10 recorded.

## Author decision (route B, adopted)
The canonical list is the **ten** invariants, i.e. the issue's route B:
#9 (new tool → new file) and #10 (new provider → new file + trait) stay
first-class numbered invariants, matching what
`tests/invariants/invariant_registry.rs` mechanically enforces (tool files
registered in `src/tools/mod.rs`, ChatProvider impls confined to `src/llm/`).
This was already the working state on `main`: the fix landed in commit
`7706ef2` ("WIP: pipeline-61-1001110457") as part of the pipeline-61 run; this
journal records it against the issue archive.

## Files touched
- `.dev/AGENTS.md` — invariants list expanded 8 → 10 (#9/#10 added with their
  automated tests); "the 8 invariants" wording updated; numbering-guard note
  appended.
- `docs/architecture/invariants.md` — retitled "The Ten Invariants";
  #2 re-pointed to Orthogonality (was "Error Variants Live in error.rs",
  now folded into #5), #4 → Tests Are Non-Negotiable (New Tool → New File
  moved to #9), #6 → No New Dependencies (New Provider → … moved to #10);
  added #9/#10 sections + sync note; Quick Reference table updated.
- `AGENTS.md` (root) — dropped the numbered 5-item copy entirely (it silently
  skipped the conflicted #2/#4/#6); now a pure pointer to
  `.dev/AGENTS.md` as single source of truth, with an unnumbered short list.
- `tests/invariants.rs` — header now documents the #1–#10 → test-module map.
- `tests/invariants/invariant_registry.rs` — new guard tests:
  `invariant_numbering_agrees_across_documents` (parses `N. **Title.**` from
  `.dev/AGENTS.md` and `## Invariant #N — Title` from the architecture doc,
  asserts equal (number → normalized-title) maps) and
  `every_numbered_invariant_has_automated_enforcement` (each numbered entry
  must carry an `Automated test:` or `Enforced by:` line), plus the #9/#10
  module-layout checks.

## Verification
- Negative test: temporarily changed `Invariant #2` title in the architecture
  doc → `invariant_numbering_agrees_across_documents` failed with a clear
  diff of both maps; reverted → green.
- `cargo test --test invariants` → 46 passed, 0 failed.

## Notes
- Local archive `.dev/issues/09-invariant-numbering-conflict.md` referenced by
  the issue does not exist in this worktree; no attempt to recreate it.
- Root `AGENTS.md` now lists no invariant numbers at all, so the "third hand
  copy" failure mode cannot recur there; the guard test covers the two docs
  that do carry numbers.
- The conflicted-invariant code references (`invariant #2` orthogonality in
  `tests/invariants/loop_size_orthogonality.rs`, `invariant #4` test
  coverage, `invariant #6` dep justification, `#9/#10` registry) all now
  point at rules with identical titles in both documents.

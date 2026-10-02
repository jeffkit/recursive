# #61 — Invariant numbering conflict between the two contract documents

> Status: **RESOLVED** (route B — ten invariants). Original issue text is in
> the GitHub issue; this is the local archive required by the issue's
> "本地存档" line. Resolution journals:
> `manual-20261001-issue61-invariant-numbering.md`,
> `manual-20261001-issue61-residual-8-invariants-prose.md`,
> `manual-20261002-issue61-guard-verification.md`.

## Summary (as filed)

`.dev/AGENTS.md` (canonical per root `AGENTS.md`) and
`docs/architecture/invariants.md` (agent knowledge base) disagreed on what
invariants #2/#4/#6 mean, while both documents are the rollback criteria of
the self-improve loop:

| # | `.dev/AGENTS.md` (before) | `docs/architecture/invariants.md` (before) |
|---|---|---|
| 2 | Orthogonality | Error Variants Live in error.rs |
| 4 | Tests are non-negotiable | New Tool → New File |
| 6 | No new dependencies without justification | New Provider → New File + Trait |

Root `AGENTS.md` listed only the 5 non-conflicted numbers, silently skipping
the disagreement.

## Resolution

- **Route B adopted** — the canonical list is the **ten** invariants:
  #9 (new tool → new file) and #10 (new provider → new file + trait) are
  first-class numbered invariants, mechanically enforced by
  `tests/invariants/invariant_registry.rs`
  (`tool_files_are_registered_in_mod_rs`,
  `providers_live_in_llm_and_implement_the_trait`).
- `.dev/AGENTS.md` is the single source of truth; numbering expanded 8 → 10
  with per-entry `Automated test:` / `Enforced by:` lines.
- `docs/architecture/invariants.md` renumbered to match (#2 Orthogonality,
  #4 Tests Are Non-Negotiable, #6 No New Dependencies; the old #2's
  "error variants live in error.rs" folded into #5's body).
- Root `AGENTS.md` demoted to an unnumbered pointer — no third hand copy.
- Guard tests in `tests/invariants/invariant_registry.rs`:
  `invariant_numbering_agrees_across_documents` (parses `N. **Title.**` vs
  `## Invariant #N — Title`, asserts equal number→normalized-title maps) and
  `every_numbered_invariant_has_automated_enforcement`.

## Acceptance criteria (final state)

1. ✅ (number → title) mapping identical across both numbered documents;
   root `AGENTS.md` carries no numbers.
2. ✅ Automated guard: any renumber drift fails `cargo test --test invariants`
   (negative-tested 2026-10-02, see
   `.dev/journal/manual-20261002-issue61-guard-verification.md`).
3. ✅ Every numbered invariant in `.dev/AGENTS.md` names its automated
   enforcement; unlisted enforcement fails the guard.
4. ✅ Author decision recorded: ten invariants (route B).

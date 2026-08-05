# Goal 391 — Document packaging split triggers

**Roadmap**: Architecture decision hygiene (from
`.dev/HANDOFF-2026-07-06-arch-review-wrap.md`). Prevent future reviews from
re-litigating a `recursive-kernel` / `recursive-platform` split before there
is evidence that the split pays for itself.

**Design principle check**:
- Implemented as: one short product architecture document under
  `docs/architecture/`, with verified code references and no code moves.
- ❌ Does NOT create new crates, change Cargo workspace membership, or alter
  product behaviour.
- ❌ Does NOT edit `.dev/` supervisor policy from inside self-improve. The
  proposed weixin freeze marker is a supervisor follow-up, not agent scope.
- ❌ Does NOT branch `run_core.rs::run_inner`.

## Why (verified 2026-08-04)

1. The 2026-07-06 handoff postponed both crate splitting and a Session
   companion split, and listed decision triggers, but those criteria are not
   in a durable product architecture document.
2. The 2026-08-04 review reaches the same conclusion: split cost is immediate,
   while no external platform consumer or measured CI win currently demands
   it.
3. `docs/review/REVIEW_STATUS.md:16` already links the 2026-08-04 strategic
   review and goals 385–391. Do not add a duplicate review-status link.
4. The review also recommends freezing autonomous investment in `src/weixin/`.
   That is an orchestration policy and must be applied separately by the
   supervisor in `.dev/ROADMAP-v4.md` / `.dev/AGENTS.md`, not by the product
   agent in this goal.

## Scope (do exactly this, no more)

### 1. Verify the current architecture claims

Before writing prose, locate and record exact `file:line` evidence for:

- current workspace/library/CLI/TUI packaging;
- the goal/todo/plan state that is already `Arc`-shared;
- the `Mutex<AgentRuntime>` path that serializes `run` / `enqueue`;
- why that serialization is intentional today rather than an accidental lock
  that should be removed.

If the last two claims are not supported by current code, do not copy them
from the handoff. State the verified current design and add the discrepancy to
the journal.

### 2. Add `docs/architecture/packaging.md`

Keep it concise (about 80 lines) and include:

- Current packaging: `recursive-agent` library plus `recursive-cli` and
  `recursive-tui` binaries/workspace crates.
- A clear **do not split yet** decision.
- The three evidence-based triggers inherited from the handoff:
  1. an actual third-party platform implementation needs the kernel boundary;
  2. the kernel must be reused outside Recursive's own tools/providers/message
     types;
  3. CI compile time is measured as a bottleneck and an experiment shows a
     crate split would materially improve it.
- Session companion guidance based only on the verified references from §1.
- Links to `docs/INTERNALS.md`, the 2026-07-06 handoff, and the 2026-08-04
  architecture review.
- A short "how to revisit" checklist requiring an owner, consumer, or
  benchmark rather than another speculative review.

### 3. Supervisor follow-up only — do not implement here

In the journal, record this exact follow-up for the supervisor:

- add a `weixin` freeze marker to `.dev/ROADMAP-v4.md`;
- optionally add `Do not edit src/weixin/ unless the goal explicitly says so`
  to `.dev/AGENTS.md` hard limits;
- no autonomous weixin goal until a human explicitly unfreezes it.

Do not modify those `.dev/` policy files in this self-improve goal.

## Files NOT to touch

- Any Rust source, including `src/weixin/**`.
- Any `Cargo.toml`, `Cargo.lock`, or workspace membership.
- `.dev/ROADMAP-v4.md`, `.dev/AGENTS.md`, `.dev/flows/`, or `.flowcast/`.
- `docs/review/REVIEW_STATUS.md`; its latest-review link already exists.

## Acceptance

- `docs/architecture/packaging.md` exists and contains all three split
  triggers.
- Every claim about Arc-sharing or runtime serialization has an exact current
  `file:line` citation, or the unsupported historical claim is omitted and
  documented in the journal.
- `git diff -- Cargo.toml Cargo.lock` is empty and no workspace member changed.
- `git diff -- src/weixin .dev/AGENTS.md .dev/ROADMAP-v4.md` is empty.
- No duplicate 2026-08-04 link is added to `docs/review/REVIEW_STATUS.md`.
- Docs links referenced by `packaging.md` resolve to files in the repository.
- Journal: `.dev/journal/manual-20260804-goal391-packaging-freeze.md` records
  the verified code references and the supervisor-only weixin follow-up.

## Notes for the agent (traps)

- This goal is intentionally boring. Do not "helpfully" start a crate split.
- Freeze is an orchestration policy, not a reason to delete weixin code.
- A historical handoff is evidence of a decision, not proof that every
  implementation detail is still true. Verify current code before turning a
  claim into durable architecture documentation.
- Do not edit `.dev/` to enforce the policy yourself; that would let the
  examinee rewrite the exam.

# Manual edit: coordinator-brief

**Date**: 2026-07-27
**Goal**: Teach the Recursive coordinator how to write worker briefs — per design doc P1
(§5.4/5.5). The coordinator prompt previously only explained *how* to dispatch
(`spawn_worker`, `spawn_workers_parallel`, `team_add_role`), not *how* write the
prompt that goes into those calls. This gap directly hurts self-improve runs that
rely on sub-agent research.

**Change**: Extended `coordinator_system_prompt()` (`src/multi.rs:428`) with a new
`## Writing worker prompts` section (~+150 lines) covering:
1. **Never delegate understanding** — the "based on your findings" anti-pattern vs
   synthesised specs with concrete `file:line` references.
2. **Self-contained brief requirements** — target, root cause, done-criterion,
   purpose statement.
3. **Continue vs spawn decision table** — when to use `send_message` (continue an
   existing worker) vs `spawn_worker` (fresh worker), based on context overlap.
4. **Verification bar** — "prove it works, don't just confirm it exists" (design
   doc §5.5), lifted verbatim from fake-cc (`coordinatorMode.ts:220-228`).

**Source mapping**:
- Core structure and anti-pattern → fake-cc `coordinatorMode.ts:251-335` "Writing
  Worker Prompts" + `AgentTool/prompt.ts:101-112` "Writing the prompt".
- Tool names adapted to Recursive (`spawn_worker`/`spawn_workers_parallel` instead
  of `AgentTool`, `send_message` instead of `SendMessageTool`).
- Verification subsection is a near-verbatim borrow (design doc §5.5).

**Files touched**: `src/multi.rs` (`coordinator_system_prompt` + test
`coordinator_system_prompt_teaches_worker_briefing`).

**Tests added**: `coordinator_system_prompt_teaches_worker_briefing` — pins the
load-bearing phrases ("Never delegate understanding", "cannot see this
conversation", "send_message"/"spawn_worker" both present, "prove" for verification)
so a future trim can't silently drop the methodology.

**Quality gates**: `cargo fmt --all --check` clean; `cargo test --workspace` all
green; `cargo clippy --all-targets --all-features -D warnings` clean. No TUI src
touched, so TUI gates N/A.

**Notes**:
- Pure text change; function signature (`&'static str`) unchanged. Impact analysis
  showed CRITICAL only due to wide call-graph propagation, not because the change
  can break callers. All existing callers (`assemble_system_prompt` → runtime
  builders) see the same return type.
- This is the **P1** deliverable from the design doc
  `local_docs/recursive-vs-fakecc-prompt-comparison.md` (§5.4/5.5). P0
  disciplines landed in the prior session (commit `d5e27aa`).
- The prompt now approaches fake-cc coordinator thickness: fake-cc's
  coordinator prompt is ~360 lines; Recursive's is now ~170 lines (was ~30).
  Recursive doesn't need the fake-cc's "Example Session" or "AgentTool Results"
  sections (different notification shape), so the diff is expected.
- No `default_prompt_is_well_under_a_kilobyte` test was added for the coordinator
  prompt (the existing test only covers `default_system_prompt`). The coordinator
  prompt is gated behind `RECURSIVE_SUBAGENT_ENABLED=1` and lives outside the
  hot prefix cache, so length pressure is lower.

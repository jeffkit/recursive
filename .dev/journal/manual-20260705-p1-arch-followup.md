# Manual edit: p1-arch-followup (P2/P3 + P1-1 prep)

**Date**: 2026-07-05
**Goal**: Push the next slice of the 2026-07-05 architecture review forward
on top of the P0 commit (`81290b1`). Land low-risk wins (README example,
AGENTS.md cross-reference, two P3 cleanups, document self-improve failure
modes) plus the protective test that gates the future HIGH-risk P1-1
`run_inner` split.
**Branch**: `fix/p1-arch-followup` (worktree `.worktrees/p1-arch-followup`)
**Base**: `81290b1` (P0-A/B/C from the architecture review)

## Files touched

- `README.md` — replaced the v0.5.0 `Agent::builder()` example with a
  v0.7 `AgentRuntime::builder()` snippet that compiles against the
  current public API (`OpenAiProvider::new` returns `Result`,
  `RuntimeOutcome::final_text` not `final_message`). Deleted the stale
  "What's New in v0.5.0" section. Verified by temporarily compiling the
  snippet as `examples/_readme_smoke.rs`.
- `AGENTS.md` — added a "two files, two audiences" header pointing at
  `.dev/AGENTS.md` so the next reader knows which is which. Did NOT
  rename / delete: `src/config.rs::load_project_context` hard-reads
  `AGENTS.md` at the workspace root into the agent's system prompt, so
  the file name is a runtime contract, not just documentation.
- `.dev/AGENTS.md` — symmetric header pointing back at root `AGENTS.md`.
- `CLAUDE.md` — new "Known self-improve failure modes" subsection in
  the Parallel workflow context. Three bullets lifted from the user's
  auto-memory: silent rollback failure on agent crash, phantom
  deletions from cross-PR landing, and the `parallel-self-improve.sh`
  vs deprecated `self-improve.sh` rule. Goal: stop treating the flow
  as a reliable pipeline in our own docs.
- `src/kernel.rs` — `with_tools` was hand-cloning 11 fields; adding a
  new field would silently drop it on the clone path (git history
  shows this has bitten once). Replaced with `let mut clone =
  self.clone(); clone.tools = tools; clone`. Behaviour identical.
- `src/tools/shell.rs` — `RunShell` now accepts an LLM-supplied
  `max_output_bytes` arg per call, clamped to a 2 MiB hard cap. The
  128 KiB default truncated `cargo build` diagnostics before the
  relevant lines. Two new tests pin both the raise and the clamp.
  Also added `with_max_output_bytes` builder for hosts that want a
  different baseline.
- `tests/invariants/loop_size_orthogonality.rs` — new
  `run_inner_function_body_stays_small` test. Brace-walks
  `RunCore::run_inner` and asserts ≤ 400 lines (current baseline
  ~394). This is the protective gate for the future P1-1 split —
  without it, "just one more branch" can land in the loop body
  silently. When it fires the answer is "extract a phase helper",
  not "bump the threshold".

## Tests added

- `tools::shell::tests::per_call_max_output_bytes_can_raise_within_hard_cap`
- `tools::shell::tests::per_call_max_output_bytes_clamped_to_hard_cap`
- `loop_size_orthogonality::run_inner_function_body_stays_small`

## Quality gates

- `cargo test --workspace` — green (1071+ tests across all crates)
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all --check` — clean

## Notes (non-obvious decisions)

### Why P3-1 and P3-2 were skipped

The hand-off list had four P3 items. Only two landed:

- **P3-1** (`effective_step_limit(0) → usize::MAX`) is intentional
  behaviour, not a bug. There is an explicit unit test
  (`effective_step_limit_zero_means_unbounded`) pinning the semantics,
  and the `0 = unlimited` contract is how `recursive loop` supports
  long-running autonomous sessions. Adding a hidden 1000-step hard cap
  would silently change the contract. If we want a production cap, it
  should be an explicit env var (`RECURSIVE_HARD_STEP_CAP`) with a
  documented default — not a secret constant. Defer to its own design
  discussion.
- **P3-2** (`multi::SharedMemory::set` using `SystemTime::now()`) has
  small blast radius (timestamp is only used for display + ID hashing,
  not for ordering) and the obvious fix (switch to monotonic) breaks
  the wire format of `MemoryEntry` which serialises `timestamp` as
  wall-clock seconds since UNIX_EPOCH. Not worth the churn in this
  pass. The right fix is to add a separate internal `seq: u64` for
  ordering and keep `timestamp` as wall-clock — defer.

### Why root `AGENTS.md` is kept despite the hand-off suggesting a rename

The hand-off proposed renaming root `AGENTS.md` to `AGENTS.brief.md`
or deleting it in favour of `.dev/AGENTS.md`. That would break
`src/config.rs::load_project_context` (and the test
`system_prompt.rs::project_context_includes_agents_md`), which
hard-codes the path `AGENTS.md` as the file Recursive injects into
its own system prompt. The two files have **different audiences** —
root is the agent's runtime contract (patch format, stuck detection),
`.dev/AGENTS.md` is the source-code invariant list. Cross-references
added in this commit make the split explicit without breaking the
runtime path.

### P1-1 itself is NOT done in this session

`run_inner` is 394 lines, 69 indirect callers via `App::submit_prompt`
and friends. Splitting it in a single session is HIGH risk and the
kind of change that wants its own worktree, its own design pass
(state machine vs phase helpers), and staged commits per phase. What
this session does is **land the gate** so the next session (or the
self-improve flow) can't quietly grow the body further while deciding
on the split strategy.

## Next session pickup

P1-1 (the actual `run_inner` split) is now unblocked. Recommended
sequence:

1. `gitnexus_impact RunCore upstream` for the full caller graph
2. Decide: state-machine (`StepStart → DrainMailbox → ...`) or
   sibling-helpers (`dispatch_tool_batch`, `handle_completion`,
   `check_budget`). My lean is sibling-helpers — smaller diff,
   easier to review, and the loop body remains a literal `loop {}`
   over calls.
3. Land one phase at a time, run the new
   `run_inner_function_body_stays_small` test after each commit —
   it should stay green throughout because each phase extraction
   *reduces* the body line count.
4. The threshold of 400 was chosen as "current baseline + ε". Once
   the split lands, drop it to whatever the new baseline is plus the
   same ε, so the gate stays tight.

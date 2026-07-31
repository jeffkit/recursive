# Manual edit: goal 352 — fix TUI Thinking duration accumulating across thoughts

**Date**: 2026-07-31
**Goal**: 352 — Fix TUI Thinking duration accumulating across thoughts in a turn
(Roadmap: TUI UX). A single turn containing more than one reasoning block
(e.g. think → tool call → think again) showed the second thought's
`∴ Thought for Xs` header and the live `∴ Thinking…` spinner with the
**wrong elapsed time** — both were anchored to the whole-turn clock, so the
second thought inherited the first thought's time plus the tool gap.

**Root cause**:
- `finalise_streaming_reasoning` computed `duration_ms` from
  `self.turn.started_at` (set once per turn) — the second thought's
  duration included the first thought + tool execution.
- The live spinner read `turn.step_started_at`, which was re-armed only on
  `ToolCall`, never when a *later* reasoning block began — the live
  `Thinking…` counter kept climbing from the previous step.

**Fix (recursive-tui only)**:
- `crates/recursive-tui/src/model.rs` — added `started: Option<Instant>`
  to `TranscriptBlock::Reasoning`: the wall-clock instant this block began
  streaming. `None` only for reconstructed/resume blocks (no live timing).
- `crates/recursive-tui/src/app/event_loop.rs`:
  - `append_streaming_reasoning` — when creating a *new* Reasoning block,
    call `turn.start_step()` (live spinner restarts from 0.0 per thought)
    and stamp `started: Some(Instant::now())` adjacent to it.
  - `finalise_streaming_reasoning` — measure `duration_ms` from the block's
    own `started` via `checked_duration_since` (never subtraction/unwrap;
    a backwards clock yields `None` → `∴ Thought`, fail safe). `now` is
    captured once before the loop. Non-streaming path stays untimed
    (`started: None, duration_ms: None`).
- `crates/recursive-tui/src/app/render.rs` — session-resume path
  (`blocks_from_messages`) sets `started: None`: resumed reasoning has no
  live timing, renders `∴ Thought` exactly as before.
- `crates/recursive-tui/src/ui/transcript.rs` — `render_block` match arm
  updated for the new field; test fixtures gain `started: None`.

**Files touched**:
- `crates/recursive-tui/src/model.rs`
- `crates/recursive-tui/src/app/event_loop.rs`
- `crates/recursive-tui/src/app/render.rs`
- `crates/recursive-tui/src/ui/transcript.rs`

**Tests added** (in `app/event_loop.rs`):
- `second_thought_duration_does_not_include_first` — structural regression:
  with `app.turn.started_at = None`, two thoughts separated by a ToolCall
  still each get `Some(duration_ms)`, proving duration is sourced from the
  block's `started`, not the turn clock (old code → `None`).
- `second_thought_timing_is_independent_of_first` — `#[ignore]`d real-timing
  variant: sleeps 100ms/60ms/100ms; asserts second thought `d1 < d0 * 2`
  (bug would give d1 ≈ d0 + tool gap + d1). Verified locally via
  `cargo test -p recursive-tui --lib -- --ignored`.
- `reasoning_block_stamps_started_on_creation` — `started` is `Some` after
  the partial; finalise stamps `duration_ms` and does NOT overwrite
  `started`.
- `resumed_reasoning_has_no_duration` — a `started: None, duration_ms:
  None` block (as `blocks_from_messages` builds) renders `∴ Thought`, never
  `∴ Thought for`.
- Updated `reasoning_partials_stream_then_finalise_above_answer` to assert
  `started.is_some()` mid-stream and `started` unchanged after finalise.

**Verification**:
- `cargo test --workspace` — green (2171 + 809 TUI + others; 0 failed).
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean.
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change).
- `.dev/scripts/tui-mutants.sh` scoped to the goal's acceptance files
  (`crates/recursive-tui/src/app/event_loop.rs`,
  `crates/recursive-tui/src/model.rs`) — **exit 0, 16 mutants tested, 16
  caught** (all new Goal-352 logic is pinned by tests; model.rs has no
  mutants).
- A broader explicit run over all four touched files
  (event_loop.rs, model.rs, render.rs, transcript.rs) found 123 mutants
  with **only 2 survivors, both pre-existing debt**:
  - `render.rs:164:13` and `render.rs:165:13` — `replace || with && in
    parse_v4a_patch`. `parse_v4a_patch` is untouched by this goal
    (identical to HEAD); my render.rs diff is only the `started: None`
    field in `blocks_from_messages`. These survivors are documented per
    tui-acceptance.md rather than chased (adding tests for an untouched
    pre-existing function is outside Goal 352 scope). No survivors exist
    in the goal's new code.
- Manual `-- --ignored` timing test passes.
- `.dev/scripts/e2e-gate.sh` — **PASS (exit 0, smoke suite green)** after
  two environmental hiccups, neither a code failure:
  1. First flow run: the gate's `argus-build` performed a cold ~594s Docker
     release rebuild (my crates/ change invalidated the `COPY crates/`
     layer even though `recursive-cli` does not depend on `recursive-tui`,
     so the built binary is byte-identical), which blew the gate's 600s
     timeout right as the build finished. Fixed by pre-building
     `recursive:e2e` from the worktree source (`docker build -f
     e2e/Dockerfile -t recursive:e2e .`) — BuildKit deduplicated to the
     same content-addressed image and the cache is now warm (gate build
     took 299ms).
  2. Leftover MCP session from the timed-out gate run caused
     `SESSION_EXISTS` on `argus-init`; stopped it via
     `mcp2cli --session-stop argusai-wt-38eb33a` and re-ran — gate green.
  - Note for future runs: TUI-only changes still invalidate the e2e Docker
    build cache via `COPY crates/ crates/`; pre-warm with the docker build
    above if a later goal hits the 600s timeout again.

**Notes**:
- Per-block timing lives on the block (`started`), NOT on
  `TurnState` (no `reasoning_started_at` field — that would repeat the
  original shared-state mistake). The status-bar `⏱ Xs` total-turn timer
  (`ui/status.rs`) still uses `turn.started_at` and is intentionally
  untouched.
- No runtime/kernel/tool/provider edits; no new `UiEvent`/`UserAction`
  variants. `Instant` implements `PartialEq`/`Eq`, so `TranscriptBlock`'s
  derives remain valid.
- Changes are staged in the worktree (not committed) so the flow's
  `commit.prep` still lands them; `git status --porcelain` shows staged
  changes so `worktreeDirty` remains true.
- Pre-existing mutants survivors (render.rs parse_v4a_patch, lines
  164-165): `||` → `&&` mutations in the V4A patch header prefix check.
  Not related to Goal 352; left as-is.

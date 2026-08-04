# Manual edit: Goal-384 — Todo panel keeps the in-progress task visible

**Date**: 2026-08-04
**Goal**: TUI usability — the task-list panel (above the input box) used to
hardcode `.take(6)` + `.min(6)`, silently truncating the in-progress task
off-screen once a session had more than 6 todos. Fix: render-layer windowing
that guarantees the in-progress item stays visible, plus a screen-relative
height cap so the panel can grow for long lists.

**Files touched**:
- `crates/recursive-tui/src/ui/chat.rs` (only file)

**Changes**:
1. New pure helper `todo_window(total, anchor, content_rows) -> (usize, usize)`:
   computes the visible slice `[start, end)` over the todo list. Centres the
   in-progress item (`idx.saturating_sub(content_rows / 2)`, clamped with
   `ideal_start.min(total - content_rows)`); pins to the tail when there is
   no in-progress item (mirrors transcript scroll-to-bottom). Returns `(0, 0)`
   for empty list / zero-height panel. This is the goal's headline logic and
   is unit-tested by name (`todo_window_centers_anchor`).
2. `render_todo_panel` now slices `current_todos[start..end]` instead of
   `.take(6)`. No `Paragraph::scroll` — the rendered slice IS the window.
   Title gains a truncation indicator when items are hidden:
   ` Tasks (5/9 done) ↑3 ↓0 ` (↑N = N hidden off the top, ↓M = M off the
   bottom). Item icon/style/label mapping unchanged.
3. `todo_panel_height(app, screen_height)` is now screen-relative: grows up to
   ~1/3 of the screen (`(screen_height - 8).max(3) / 3`) but never below the
   old 6-item default (`.max(6)`). Caller passes `frame.area().height`.
   `+2` border accounting unchanged.

**Tests added/updated** (`crates/recursive-tui/src/ui/chat.rs` `debt_tests`):
- Updated `todo_panel_height_zero_when_empty` to the new signature (still
  kills the `-> 0` mutant).
- Renamed/rewrote `todo_panel_height_grows_with_items_caps_at_six` →
  `todo_panel_height_grows_with_items_up_to_screen_cap`: tall screen (40) →
  9 items = 11 rows; short screen (24) → cap 6 kicks in, 9 items = 8 rows.
- New `todo_window_centers_anchor`: `(9, Some(7), 4) == (5, 9)`; anchor at 0 →
  `(0, 4)`; anchor at last → `(5, 9)`; fits → `(0, 3)`; no anchor → tail
  `(5, 9)`; degenerate inputs `(0, 0)`.
- New `render_todo_panel_shows_in_progress_when_beyond_viewport`: 9 todos on a
  24-row screen (panel = 6 content rows), in-progress at index 7. Asserts the
  active_form label `DoingTask7` is painted (old `.take(6)` truncated it) and
  the truncation indicator `↑3 ↓0` appears.

**Notes / decisions**:
- **Centering math**: `idx - content_rows/2` puts the anchor roughly middle of
  the panel; `min(total - content_rows)` clamps so the window never overshoots
  the list end (anchor near the tail pins the window to the bottom, not past
  it). Verified: anchor 7 of 9 with 4 rows → start = min(7-2, 5) = 5.
- **No manual scroll controls** (deliberate): auto-follow is stateless
  (computed each render, no `App` field). Arrow-key/mouse scroll would need a
  new `App` field + key/mouse handlers + state — a separate enhancement.
- **No event-loop hook**: the design mention of an optional recompute hook is
  unnecessary because the window is recomputed on every render; `TodoUpdated`
  still just replaces `current_todos`.
- **Known limitation (accepted)**: window math counts items, not wrapped rows.
  A long todo that wraps (`Wrap { trim: true }`) can push the anchor below the
  last visible row. Todos are typically short; over-engineering wrapped-row
  math is out of scope for this goal.
- **No `Paragraph::scroll`** because the panel has no cursor to track (that's
  the input box's reason for `.scroll`); slicing the visible window also
  avoids ratatui computing wrap heights for off-screen lines.
- All three quality gates green locally: `cargo fmt --all`, clippy
  `-D warnings`, `cargo test --workspace`; plus TUI gates
  (`cargo test -p recursive-tui`, `.dev/scripts/tui-test-presence.sh`).

## E2E gate round (flow resume-fix 1/3 — failure was environmental, not code)

The flow's `e2e` gate failed once and fed the failure back. Investigation
showed NO test failure: the gate had been killed by the flow's 10-min
timeout while doing a **cold docker image build for a brand-new commit**
(`recursive:e2e-wt-dc895b6` did not exist yet). Two compounding problems,
both pre-existing infrastructure issues, not goal-384 code:

1. **Zombie `docker build` processes from earlier killed gate runs were
   still running** (one from 09:13 for a different worktree's image, one
   from 09:24 for the SAME image tag). They contended with the new build
   over buildkit/cache, inflating the build to ~28 min (vs ~2-3 min warm).
   Fix: `kill` the stale docker-build PIDs (identify via
   `ps aux | grep 'docker build'`), let the current build proceed.
2. **The mcp2cli session daemon died during the 28-min build** (session
   vanished: `mcp2cli --session-list` showed nothing; subsequent
   argus-setup/run failed with "session not found"). Once the image exists
   and builds are short, the daemon survives — the flow re-run passed.

Remedy applied (per AGENTS.md known failure modes #4/#5):
`mcp2cli --session-stop argusai-wt-<sha>`, `docker rm -f <wt>-aimock`,
`docker network rm argusai-wt-<sha>-network`, `rm -rf e2e/.argusai`.

Result: `sh .dev/scripts/e2e-gate.sh` → `smoke PASSED ✓` (3/3 cases,
failed=0, total=3 — real green, not the total=0 false-green). Verified the
docker build itself succeeds with warm layers (all CACHED, exit 0);
argus-build's per-run "Build exited with code 1" is its own orchestration
quirk that the gate explicitly tolerates (build failure non-fatal, existing
image reused).

Lesson for future runs: after a killed e2e gate, ALWAYS check
`ps aux | grep 'docker build'` for zombie builds before re-running —
otherwise the re-run contends with ghosts and blows the 10-min budget
again. (Note: the other worktree's flow may run its own e2e gate
concurrently under a different `argusai-wt-*` session — only touch
resources matching YOUR worktree's sha.)

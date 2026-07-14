# Manual edit: mutants-gate-doc-policy

**Date**: 2026-07-14
**Goal**: Reframe `tui-mutants.sh` from a hard per-change gate to
"recommended but advisory" for **manual edits**, while keeping the
Flowcast self-improve flow's hard-gate enforcement unchanged. Motivation:
mutation testing as a blocking gate after every TUI change is too heavy
(~minutes per run, `--jobs 4`) and produces false friction — it mutates
whole files, so touching a large file surfaces pre-existing survivors
unrelated to the change (observed this run: 4 `weixin_final_text` + 1
equivalent `>=` out of 8 missed were not about the diff).

**Policy split**:
- `tui-test-presence.sh` — hard gate for both manual edits and the flow.
- `tui-mutants.sh` — recommended/advisory for manual edits (non-zero exit
  does not block); survivors inside the diff hunks are real weak-test
  signals to fix, survivors in untouched regions are pre-existing debt to
  note. Still a hard gate in the Flowcast autonomous flow
  (`.flowcast/gates.json` `onFail: resume-fix`) — intentionally, to guard
  the self-improving agent against landing shallow tests.

**Files touched**:
- CLAUDE.md — "Mandatory quality gates" section reworded.
- .dev/AGENTS.md — added a "Manual edits vs. the autonomous flow" note
  after the TUI mutation-gate paragraph.

**Tests added**: none (doc-only).
**Notes**: Script/flow config intentionally NOT changed (per "doc_only"
scope). A future step could add `tui-mutants.sh --diff-aware` to filter
survivors to diff hunks and a fast diff-line-coverage hard gate; not done
here. Worktree `.worktrees/tui-offline-status`, branch `tui-offline-status`.

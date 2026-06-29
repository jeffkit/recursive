# Manual edit: tui-test-harness stage 5 (self-improve workflow integration)

**Date**: 2026-06-29
**Goal**: Wire stages 1–4 into a reusable SOP + goal template so the
self-improve loop (and any AI) automatically follows the harness workflow
when a goal touches `crates/recursive-tui/`.

**Files touched**:
- `.dev/skills/tui-acceptance.md` (new) — canonical TUI-testing SOP.
- `.dev/goals/_TEMPLATE-tui-acceptance.md` (new) — goal template that
  embeds the SOP into Acceptance + Notes.
- `crates/recursive-tui/src/harness.rs` — module doc points at the SOP.

**Why `.dev/` and not `.claude/skills/`**:
`.claude/` is gitignored in this repo (the existing `recursive-loop`
skill lives there untracked). The Recursive self-improve agent reads
goal files from `.dev/goals/` (tracked), and discovers skills from
`.recursive/skills/` + `~/.recursive/skills/`. The most reliable
**committable** lever is therefore a tracked SOP under `.dev/skills/`
plus a goal template under `.dev/goals/` whose Acceptance/Notes sections
reference it — the agent reads the goal, follows the embedded workflow.

**The SOP** (`.dev/skills/tui-acceptance.md`) — three layers, use all:
1. Logic + render: in-process `Harness` (`#[cfg(test)]`), visual
   assertions via `Screen`.
2. Effectiveness: `.dev/scripts/tui-mutants.sh` (scope to touched files,
   exit non-zero on survivors).
3. Integration: `tui-pty` binary tours the real `recursive-tui` under a
   PTY (`--bin` absolute path, `--keys` grammar, `--snap numbered|json`).
Then the mandatory fmt/clippy/test gates.

Anti-patterns spelled out: internal-state-only assertions, `has_bg` for
highlight detection (use `row_has_bg_color` with the specific colour),
skipping the mutation gate, relative `--bin` path, whole-crate mutation.

**The goal template** (`.dev/goals/_TEMPLATE-tui-acceptance.md`):
copied for any TUI goal; its Acceptance section requires the mutation
gate to exit 0 (with an explicit exception clause for `lib.rs`
terminal-IO survivors, which the PTY tour covers) and a PTY-tour
assertion of exact screen content. Notes section tells the agent to
follow `.dev/skills/tui-acceptance.md`.

**Quality gates** (in `.worktrees/feat-tui-test-harness`):
- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui` — 276 passed, 0 failed (no
  `--features recursive/test-utils` flag needed; the stage-1 dev-dep
  makes the suite self-contained)

**Stage 5 complete — all five stages landed on `feat/tui-test-harness`.**
Branch commits:
- `1db4ce7` stage 1 (in-process harness)
- `5c40025` stage 2 (visual acceptance + manual mutation proof)
- `a71b141` stage 3 (cargo-mutants effectiveness loop)
- `b73a793` stage 4 (tui-pty integration harness)
- `<this>`   stage 5 (self-improve workflow SOP + goal template)

The observation loop (1–2) + effectiveness loop (3) + integration loop
(4) + automation (5) together let an AI write TUI tests that are
verifiably effective and acceptance-checked against the real binary.

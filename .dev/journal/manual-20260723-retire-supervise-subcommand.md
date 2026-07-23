# Manual edit: retire hardcoded `/loop supervise` → loadable `loop-supervise` skill

**Date**: 2026-07-23
**Goal**: The `supervise` subcommand was hardcoded in the Recursive binary
(one fixed monitor SOP via `include_str!`), which contradicts the converged
design — the loop is the only generic primitive, and playbooks are loadable,
customizable skills. Retire the subcommand and move the generic monitor SOP
into a skill; keep `/loop <prompt>` as the single natural-language entry.
**Files touched**:
- `crates/recursive-tui/src/commands.rs` — removed the `supervise` match arm
  and the `SUPERVISE_SOP` const + its doc comment + `include_str!`. `/loop
  supervise <cmd>` now falls through to the default `_` arm and is treated as
  a natural-language goal (the agent loads the `loop-supervise` skill from the
  goal text). Updated the `/loop` command summary/usage to drop `supervise`.
- `crates/recursive-tui/src/supervise_sop.md` — deleted.
- `.claude/skills/loop-supervise/SKILL.md` (new) — the generic monitor+intervene
  playbook as a `mode: trigger` skill (triggers: supervise/monitor/watch/盯…).
  Command-agnostic; the command comes from the user's natural-language prompt.
  Points to `recursive-loop` for the project-specific self-improve flow.
- `.claude/skills/recursive-loop/SKILL.md` — §3/§3.5 reworked to recommend
  `/loop <自然语言>` + the `loop-supervise` skill instead of `/loop supervise`.
**Tests added**:
- `cmd_loop_supervise_now_natural_language_goal` — `/loop supervise <cmd>`
  now yields goal = the whole line (no SOP injected), unlimited.
- `cmd_loop_default_does_not_parse_max_suffix_unlike_start` (additive) — the
  default path must NOT parse a `max N` suffix (unlike `/loop start`), so a
  goal ending in "max 5" is kept verbatim. This is the added `#[test]` that
  satisfies `tui-test-presence`.
- Removed the two old `cmd_loop_supervise_*` SOP-injection tests.
**Notes**:
- `stop_loop` tool, `watch_file`, `run_background`, `schedule_wakeup` and the
  arbiter are unchanged — only the hardcoded subcommand + SOP moved to the
  skill layer.
- Gates: fmt / clippy `-D warnings` / `cargo test --workspace` clean;
  `tui-test-presence` PASS. `tui-mutants` (advisory) to be run scoped to
  `commands.rs` after commit.
- UX now: `/loop <自然语言>` is the only entry; `start`/`stop`/`trigger` remain
  as explicit lifecycle overrides. Generic monitoring = `loop-supervise`
  skill; self-improve = `recursive-loop` skill. Rust has only the loop primitive.

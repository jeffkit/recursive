# Manual edit: skill-tools-consolidation

**Date**: 2026-06-29
**Goal**: Consolidate Recursive's skill tool surface from 4 tools down to 2
(`Skill` + `install_skill`), removing `find_skills` and `run_skill_script`,
via the flowcast self-improve flow. Prioritize MiniMax-M3, fall back to
DeepSeek. Also fix a flow infra bug that blocked the first run.

**Files touched**:
- `.dev/flows/self-improve.flow.js` — preflight.build fix (manual)
- `src/tools/load_skill.rs` — `${SKILL_DIR}` substitution (goal 320, `3069f21`)
- `src/tools/run_skill_script.rs` — deleted (goal 321, `9754cc4`)
- `src/tools/find_skills.rs` — deleted (goal 319, `388c056`)
- `src/tools/mod.rs`, `src/tools/registry.rs`, `src/tools/install_skill.rs`
- `src/skills.rs` — budget-aware `skill_index` + three-channel injection (goal 319)
- `crates/recursive-cli/src/cli/builder.rs`, `crates/recursive-tui/src/runtime_builder.rs`
- `docs/architecture/**` — skills-tools / skills / index docs synced

**Tests added**: per-goal unit tests inside `load_skill.rs` and `skills.rs`
(`substitute_skill_dir`, budget-aware index). Workspace suite green
(1037 passed, 0 failed).

**Notes**:
- The first Goal 320 run yielded a bogus `skip-commit` verdict. Root cause:
  flow's `preflight.build` ran bare `cargo build --release`, which at the
  workspace root only builds the root package (`recursive-agent` lib) and
  NOT `recursive-cli`'s `recursive` bin — the binary the flow later spawns.
  Result: `spawn .../target/release/recursive ENOENT`, agent never ran.
  Fixed by changing preflight.build to `cargo build --release -p recursive-cli`
  (commit `fec86c9`). Without this fix, any goal touching CLI code would
  test against a stale/missing binary.
- After the fix, all three goals landed on the first minimax attempt — no
  deepseek fallback needed.
- Final skill tool surface: `Skill` (load_skill, with `${SKILL_DIR}` +
  `${RECURSIVE_SKILL_DIR}` substitution so scripts run via `Bash`) and
  `install_skill` (skillhub search/install, TUI-only). `find_skills`
  discovery is now consolidated into the budget-aware `skill_index`
  injected into the system prompt (Always/Trigger/Globs three-channel).
- Cost: 320 $0.38 / 321 ~$0.5 / 319 ~$0.6, all MiniMax-M3.

**Quality gates (run on main after merge)**:
- `cargo fmt --all --check` — clean
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo test --workspace` — 0 failed

## Polish follow-up (`4be0f65`)

Post-merge review flagged two small blemishes; both fixed:

1. **Goal 320 — literal-placeholder escape.** `substitute_skill_dir` did
   plain string replacement, so a skill body that documented the feature
   with literal `${SKILL_DIR}` text would have it replaced by the path.
   Added backslash escape support: `\${SKILL_DIR}` / `\${RECURSIVE_SKILL_DIR}`
   render as the literal placeholder (not substituted); a lone backslash
   is preserved. Single char-boundary-safe pass, no panic on `\` at EOF.
   Refs remain exempt. +3 tests.
2. **Goal 319 — honest byte-budget naming.** `skill_index_with_budget`'s
   `char_budget` param + "character budget" rustdoc was inaccurate — the
   budget is measured in bytes (`str::len`, matching
   `SKILL_INDEX_PER_ENTRY_DESC_BYTES` and `truncate_str`). Renamed to
   `byte_budget` and fixed the rustdoc to state the byte semantics
   (conservative proxy for prompt-token cost). No behavior change.

Gates after polish: fmt/clippy/test green (1040 passed, +3).

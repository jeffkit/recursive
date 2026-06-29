# Manual edit: tui-skill-panic

**Date**: 2026-06-29
**Goal**: Fix two TUI issues reported by the user — (1) a panicking tool call
left stray log text around the input box that no redraw could clear;
(2) installed skills were not surfaced to the agent in the TUI, forcing it to
call `find_skills(query="*")` to discover them (and tripping the panic above).

**Files touched**:
- `src/tools/find_skills.rs` — root cause of the panic: description truncation
  used `&desc[..79]` (byte slice). When byte 79 landed inside a multi-byte
  UTF-8 codepoint (e.g. CJK text like `CMS图文操作`), it panicked with
  `byte index 79 is not a char boundary`. Replaced with the existing
  `crate::truncate_str` (char-boundary-safe) helper. Added a regression test
  `find_skills_multibyte_description_does_not_panic`.
- `crates/recursive-tui/src/lib.rs` — installed a process-wide panic hook
  (`install_tui_panic_hook`) that, while the TUI owns the terminal
  (`is_tui_quiet()` true), appends the panic message to
  `<user_data_dir>/logs/tui-panic.log` instead of letting the default hook
  write to stderr. The default hook writes to fd 2, which is the same surface
  the alternate screen renders onto; that raw text is not part of ratatui's
  diff buffer so no redraw ever clears it (the reported "stuck logs"). When
  the TUI is not active, the previous (default) hook runs unchanged. Added
  `append_panic_log` + test `append_panic_log_writes_under_recursive_home`.
- `crates/recursive-tui/src/runtime_builder.rs` — inject `skill_index` into
  the TUI system prompt via a new `tui_system_prompt` helper, matching what
  the CLI (`recursive-cli/src/cli/builder.rs`) and HTTP
  (`src/http/handlers.rs`) already do. This was the root cause of issue (2):
  the TUI was the only surface that omitted the catalog, so the agent had to
  discover skills by calling `find_skills`. Verified against the Claude Code
  reference (`~/Downloads/fake-cc/src/tools/SkillTool`), which injects the
  skill listing into system-reminder messages (not into the tool description)
  and keeps the `Skill` tool as a name-based invoker. Added tests
  `tui_system_prompt_appends_skill_index` /
  `tui_system_prompt_unchanged_when_no_skills`.
- `src/tools/estimate_tokens.rs` — added the missing
  `with_session_roots_opt` method. The in-progress `session_roots` refactor
  had added `with_session_roots_opt(...)` callers for 5 sibling tools but
  omitted `EstimateTokens`, so the workspace no longer compiled. Added the
  same method shape used by `src/tools/glob.rs` to unblock the build.
- `crates/recursive-tui/src/runtime_builder.rs` (call sites) — updated both
  `build_standard_tools_with_roots` calls to pass the new `session_roots`
  argument (`None` — the TUI does not wire a session-roots slot here),
  matching the updated function signature.

**Tests added**:
- `tools::find_skills::tests::find_skills_multibyte_description_does_not_panic`
- `recursive_tui::tests::append_panic_log_writes_under_recursive_home`
- `recursive_tui::runtime_builder::tests::tui_system_prompt_appends_skill_index`
- `recursive_tui::runtime_builder::tests::tui_system_prompt_unchanged_when_no_skills`

**Notes**:
- The two `estimate_tokens.rs` / `runtime_builder.rs` call-site fixes are not
  part of the reported bugs; they were pre-existing WIP compile breakages in
  the working tree (the `session_roots` refactor) that blocked verification.
  They are minimal and follow the existing sibling-tool pattern, but the user
  should review them in case the WIP refactor had a different intent.
- Design decision for issue (2): chose system-prompt injection (Option A)
  over embedding the catalog in the `find_skills` tool description
  (Option B). Token cost is equivalent (tool specs are sent every turn too),
  and injection matches the CLI/HTTP surfaces plus the Claude Code reference.
  `find_skills` stays as the local fuzzy-search tool; `load_skill` is the
  name-based invoker (≈ Claude Code's `Skill` tool). No rename.
- Quality gates run: `cargo fmt --all`, `cargo clippy -p recursive-agent -p
  recursive-tui --all-targets -- -D warnings` (clean), `cargo test -p
  recursive-agent -p recursive-tui` (all green, 267 TUI + recursive-agent
  lib/integration tests).

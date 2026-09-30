# Journal — issue #65: `--allow-tools` silently ineffective on `recursive http`

**Date:** 2026-06-14
**Goal:** Make `--allow-tools` / `RECURSIVE_ALLOW_TOOLS` gate the HTTP server's
tool surface so a "no fs / no shell" tool face is actually enforceable.

## Files touched

- `src/runtime/builder.rs` — new `with_todo_tool(bool)` builder option
  (default `true`, backwards compatible). When `false`, `build()` skips the
  unconditional `TodoWriteTool` registration. Test:
  `build_with_todo_tool_disabled_leaves_registry_untouched`.
- `crates/recursive-cli/src/main.rs` — `Cmd::Http` branch now applies
  `tools.retain_tools(&config.allow_tools)` after `build_tools` /
  `register_subagent_if_enabled` and before `tool_infos` is derived, so
  `GET /tools` reflects the restriction.
- `src/http/mod.rs` — `AppState::session_tool_registry()` re-applies
  `retain_tools` after any per-session (container-tier) rebuild, so the
  container tier cannot resurrect tools outside the allow-list. Test:
  `session_tool_registry_applies_allow_tools_filter` (allow=[Skill,HttpCall]
  over a registry with Read/Bash → exactly the two allowed tools).
- `src/http/handlers.rs` — `build_session_runtime` passes
  `.with_todo_tool(!strict_surface)` when `allow_tools` is non-empty.
- `src/http/cold_load.rs` — restored-session runtime does the same.

## Tests added

- `runtime::builder::tests::build_with_todo_tool_disabled_leaves_registry_untouched`
- `http::goal_396_persistence_tests::session_tool_registry_applies_allow_tools_filter`

## Notes

- CLI `run_loop` path already filtered (`main.rs:2089`); coordinator already
  filtered. Only HTTP was missing.
- `todo_list` Arc is still created in `build()` even when the tool is skipped
  (the runtime field and PlanTodoReinjector share it) — no behavior change.

## Gates

`cargo test --workspace` — all green (2386 lib tests, 0 failed).
`cargo clippy --all-targets --all-features -- -D warnings` — clean.
`cargo fmt --all` — applied.

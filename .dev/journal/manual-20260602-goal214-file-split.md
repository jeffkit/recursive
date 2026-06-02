# Manual edit: Goal 214 — extract CLI helpers into src/cli/ submodules

**Date**: 2026-06-02
**Goal**: Split src/main.rs (3140 lines) by extracting helper functions into a new src/cli/ directory to make the codebase more maintainable.
**Files touched**:
- `src/main.rs` (3140 → 1565 lines, -1575 lines)
- `src/cli/mod.rs` (new)
- `src/cli/builder.rs` (new, 420 lines) — build_tools, build_runtime, register_mcp_tools, register_mcp_server_tools, discover_loaded_skills, resolve_tool_permissions
- `src/cli/init.rs` (new, 188 lines) — run_init
- `src/cli/output.rs` (new, 273 lines) — get_pricing, print_usage, print_finish_note, save_transcript, save_session, exit_for_finish, finalize_session_writer, finalize_cost_tracker, stream_events, stream_events_repl, stream_events_json
- `src/cli/resume.rs` (new, 487 lines) — cmd_resume, run_resumed, resolve_resume_target, legacy_resume_error, prompt_orphan_choice, OrphanPolicy enum
- `src/cli/session.rs` (new, 256 lines) — cmd_migrate, cmd_session_migrate_legacy, cmd_session_rewind, resolve_session_path

**Tests added**: none (all existing tests pass)
**Notes**:
- resume.rs calls `crate::shutdown_signal()` (defined in main.rs) — this is valid since submodules can reference private items in ancestor modules.
- run_init was also extracted to cli/init.rs (not in the original goal spec) to satisfy the ≤1600 line constraint on main.rs.
- RetryPolicy is re-exported at the crate root (`recursive::RetryPolicy`), not under `recursive::llm`.
- All functions marked `pub(crate)` in submodules.
- `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all` all clean.

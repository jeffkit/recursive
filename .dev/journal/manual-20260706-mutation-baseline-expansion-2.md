# Manual edit: mutation baseline expansion (round 2)

**Date**: 2026-07-06
**Goal**: 继续全量变异测试基线建设，针对存活变异体补写单元测试
**Files touched**:
- `src/checkpoint.rs` — +3 tests (count_diff_files, validate_session_id)
- `src/checkpoint_log.rs` — +5 tests (is_zero_i64 direct + serialization)
- `src/compact.rs` — +2 tests (render_structured)
- `src/config.rs` — +21 tests (memory file truncation, bypass permissions, api keys, layer separators, require_api_key)
- `src/http/handlers.rs` — +8 tests (format_timestamp, sanitize_thread_id_for_session)
- `src/llm/search.rs` — +10 tests (word_boundary_match direct, parse_tool_name edge cases)
- `src/mcp.rs` — +8 tests (parse_sse_endpoint & parse_sse_response edge cases)
- `src/tools/load_skill.rs` — +1 test (depth boundary at MAX_DEPTH)
- `src/tools/memory.rs` — +5 tests (is_leap direct tests + chrono range check)

**Tests added**: 63 new tests across 9 files (1383 → 1400 total)
**Key mutants killed**:
- `checkpoint_log.rs`: `is_zero_i64 → true`, `== → !=`
- `checkpoint.rs`: `count_diff_files → 0/1` (files_changed >= 2 assertion)
- `config.rs`: `* → +` in MAX_MEMORY_FILE_SIZE, `> → >=` in truncation check, `|| → &&` in bypass permissions, `delete !` on empty-string filtering, `require_api_key → Ok("")`, `> → <` in layer separator loop
- `http/handlers.rs`: arithmetic `/→*/%` in format_timestamp, `|| → &&` in sanitize_thread_id
- `llm/search.rs`: `+ → -` in word_boundary_match positional calculations, `|| → &&` boundary conditions
- `mcp.rs`: `&& → ||` in SSE parser, `delete !` on is_empty checks, `delete -` in error code default
- `tools/memory.rs`: all 13 is_leap arithmetic/bool mutants (1972, 1900, 2000, 1971 test vectors)
- `tools/load_skill.rs`: `>= → >` in depth limit check (depth exactly at MAX_DEPTH succeeds)

**Notes**:
- Baseline run started at 10:07 AM was stale — captured survivors from before test additions
- Many baseline survivors are already killed by tests added during this session
- Equivalent mutants identified: `validate_session_id` lines 715-716 (|| vs && is moot since !all() catches both / and \)
- `tests/bin/mock_mcp_server.rs` survivors (4): match arms in test helper code — acceptable to leave
- `checkpoint.rs` gc/snapshot_for_session survivors: git-operation-dependent, difficult to test in pure unit tests
- Next priority files: `src/llm/anthropic.rs`, `src/runtime.rs` (large but complex async), `src/session/mod.rs`

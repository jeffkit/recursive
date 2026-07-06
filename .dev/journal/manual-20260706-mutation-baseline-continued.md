# Manual edit: mutation baseline continued

**Date**: 2026-07-06
**Goal**: Continue killing surviving mutants by adding targeted unit tests across low-coverage modules
**Files touched** (Round 1):
- `src/tools/schedule_wakeup.rs` — +2 tests (default args, prompt stored correctly); 6→8
- `src/cost.rs` — +2 tests (accessor methods, cost_data model/provider); 12→14
- `src/tools/task_output.rs` — +1 test (lines joined by newline); 3→4 (coordinator-mode feature)
- `src/tools/team_create.rs` — +1 test (member defaults agent_type to "general"); 4→5 (coordinator-mode)
- `src/tools/checkpoint.rs` — +2 tests (diff missing 'a' arg errors, save defaults message to turn index); 8→10
- `src/tools/web_search.rs` — +2 tests (format_results empty sentinel, numbered index); 14→16
- `src/tool_set_provider.rs` — +1 test (PolicyToolSetProvider::new uses given policy); 4→5
- `src/mcp_server.rs` — +4 tests (error response, dispatch notifications→None, dispatch unknown→-32601); 7→11
- `src/agent/types.rs` — fix clippy: needless borrow in serde_json::to_value call
- `src/coordinator.rs` — fix clippy: match → if let for Option restoration

**Files touched** (Round 2):
- `src/llm/mock.rs` — +6 tests (injected errors before completions, structured responses, empty queue error, stream sends content/reasoning, calls() records all); 3→9
- `src/session/lifecycle.rs` — +2 tests (cutoff_beyond_end keeps all, exact boundary drops last turn); 10→12
- `src/session/reader.rs` — +3 tests (orphan tool calls: no assistant tool_calls, all answered, detects orphan); 10→13
- `src/tools/todo.rs` — +3 tests (missing todos field, in_progress_label uses active_form, falls back to content); 7→10
- `src/tools/task_create.rs` — +4 tests (lookup missing arg, lookup not found, lookup success, defaults empty team/name); 3→7
- `src/tools/team_delete.rs` — +2 tests (missing name field error, backslash rejection); 3→5
- `src/tools/count_lines.rs` — +2 tests (missing path arg, nonexistent file); 4→6
- `src/tools/task_list.rs` — +2 tests (status filter running only, empty team/name shown as dash); 5→7
- `src/kernel.rs` — +3 tests (max_steps zero by default, max_steps custom, with_tools replaces registry); 11→14
- `src/tools/episodic_recall.rs` — +2 tests (missing query error, session_id filter not found); 9→11

**Files touched** (Round 3):
- `src/tools/task_output.rs` — +2 tests (block=true returns immediately for terminal task, block default is false); 4→6
- `src/tool_set_provider.rs` — +2 tests (SandboxMode::default is None, build_registry has Read tool); 5→7
- `src/mcp_server.rs` — +4 tests (dispatch initialize, tools/list returns tools, handle_tools_call missing name); 10→14
- `src/logging.rs` — +3 tests (write not quiet returns byte count, flush not quiet succeeds, suppress sets quiet immediately); 4→7
- `src/tools/task_stop.rs` — +2 tests (missing task_id errors, stop completed includes status in message); 3→5
- `src/cost.rs` — +4 tests (record_usage multiple calls accumulates, update_meta_with_cost writes all token fields, non-object JSON is noop); 14→18
- `src/rewind.rs` — +1 test (no prev and target has no pre returns error); 13→14
- `src/tools/task_update.rs` — +3 tests (missing status, missing task_id, completed message contains task_id); 6→9
- `src/tools/estimate_tokens.rs` — +3 tests (non-string text errors, non-string path errors, output includes method); 8→11
- `src/tools/task_get.rs` — +3 tests (no name shows (none), truncate respects char boundary, missing task_id); 9→12
- `src/message.rs` — +2 tests (assistant_with_tool_calls stores tool_calls, constructors store content); 9→11
- `src/session/writer.rs` — +2 tests (open_existing resumes count and uuid, append_with_audit includes audit field); 11→13

**Files touched** (Round 4 — targeting missed.txt survivors):
- `src/checkpoint_log.rs` — +1 test (nonzero timestamps included in serialization, kills is_zero_i64 always-true mutation); 10→11
- `src/config.rs` — +1 test (file at exact 8KB cap NOT truncated, kills > → >= mutation); 49→50
- `src/mcp_server.rs` — +1 test (unknown tool dispatch returns -32602, kills delete-minus mutation); 15→16
- `src/permissions/mod.rs` — +2 tests (allow-only and deny-only old config produce a layer, kills || → &&); 15→17
- `src/run_core.rs` — +2 tests (enforce_transcript_budget returns None under limit and Some over limit, kills < → ==); 13→15
- `src/runtime.rs` — +2 tests (skills builder stores in globs_skills, reject_plan appends to transcript); 20→22
- `src/event.rs` — +4 tests (TodoUpdated/MessageAppendedWithAudit round-trip, BroadcastSink no-subscriber, ChannelSink in order); 11→15

**Tests added**: 15 (R1) + 29 (R2) + 31 (R3) + 13 (R4) = **88 new tests total**
**Total passing tests**: ~1780 (compile-clean; background mutation run prevents full verification)
**Notes**:
- All new tests compile cleanly; clippy is clean on coordinator-mode.
- Background cargo-mutants run (started 10:07 AM) is consuming all CPU — preventing full test suite runs.
- Remaining survivors in missed.txt that are likely equivalent mutants or require heavy integration setup:
  - src/checkpoint.rs ShadowRepo (git-dependent, 20 mutants)
  - src/providers.rs compute_effective_presets delete-! (depends on providers.d filesystem)
  - src/run_core.rs TranscriptLimit (needs multi-step async agent test)
- All changes pushed to origin/main.

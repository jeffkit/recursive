# Manual edit: claude-json-align

**Date**: 2026-07-09 / 2026-07-10
**Goal**: Align CLI `--output-format json|stream-json` with Claude Code's wire protocol (default compatible), implement the full bidirectional control channel (`SDKControlRequestInner` subtypes), then wire multi-turn stdin `user` messages plus CLI→host `hook_callback` / `elicitation`. Keep legacy via `--output-format recursive-json`.
**Files touched**:
- `crates/recursive-cli/src/cli/claude_json.rs` (Claude JSON adapter)
- `crates/recursive-cli/src/cli/control.rs` (bidirectional control demux, Host→CLI handlers, CLI→host adapters)
- `crates/recursive-cli/src/cli/output.rs` (`JsonEventTask`)
- `crates/recursive-cli/src/cli/builder.rs` (shared `session_roots` + MCP elicitation slot)
- `crates/recursive-cli/src/cli/resume.rs`, `main.rs` (wiring, `--input-format stream-json`, follow-up turns)
- `src/hooks/external.rs` (`SdkHookForwarder` + dispatch precedence)
- `src/mcp.rs` (`ElicitationHandler`, `-32042` intercept)
- `src/tools/registry.rs` (`session_roots`, `elicitation` slots)
- `src/runtime.rs` (`set_sdk_hook_forwarder`)
- `src/lib.rs` / `src/hooks/mod.rs` (re-exports)
- `website/en/cli/run.md`
**Tests added**:
- `claude_json::tests` (6)
- `control::tests` (permission parse, bridge, interrupt, subtype smoke, initialize hooks, inbound FIFO, hook_result mapping, …)
**Notes**:
- Permission prompts use bidirectional `control_request` / `control_response` (`can_use_tool`).
- Host→CLI: all `SDKControlRequestInner` subtypes recognised; several MCP / `set_model` / `rewind_files` / `stop_task` handlers are protocol-compatible acks without full mid-run semantics.
- CLI→host: `can_use_tool` via `StdioPermissionHook`; `request_user_dialog` for plan approval; `hook_callback` via `ControlSdkHookForwarder` (registered from `initialize.hooks`); `elicitation` via shared MCP slot on `-32042`.
- `--input-format stream-json` buffers stdin `type:user` and drains them as follow-up `runtime.run` turns until stdin EOF or interrupt.
- `json` emits a single terminal `result` object (Claude semantics); use `recursive-json` for legacy AgentEvent NDJSON.
- E2E: `e2e/tests/40-claude-json-stream.yaml` + `e2e/fixtures/40-claude-json.json` (registered as suite `claude-json-stream`). ArgusAI-native pattern: three `recursive run` invocations run in `setup` (stdout → `/workspace/cj-*.jsonl`), cases `grep` the captured NDJSON. Covers stream-json wire shape (`system/init`, `assistant` tool_use, `user` tool_result, terminal `result`), single-object `json` mode (no stream events), and `--input-format stream-json` follow-up user consumption (distinct second-turn answer + `num_turns:3`). Gotcha: aimock `userMessage` matches the **latest** user message, so the follow-up frame's text must contain a keyword with its own fixture entry (`"just create"` → third fixture) or the follow-up turn 404s.

**Hardening (trap → code, not memory):**
- Fix: `stream-json`/`json` mode now always emits a terminal `result` envelope, even when `runtime.run` errors mid-run (e.g. LLM 404 on a follow-up turn). Previously `?`-propagation skipped `JsonEventTask::finish` and left the host with an unterminated stream — a Claude SDK contract violation. `run_once` now wraps the goal + follow-up turns in a captured-`Result` block; on `Err` it `drop`s the runtime (so the stream task's `rx` drains), emits a `result` with `FinishReason::ProviderStop(err)` → `is_error:true`/`subtype:error_during_execution`, then propagates. Codified by two new `error:` E2E cases (unmatched-goal 404 → still closes with `result`).
- `e2e/fixtures/README.md`: documents aimock's `userMessage`-matches-latest-message semantics + the multi-turn fixture rule (worked example), and links the authoritative source <https://aimock.copilotkit.dev/multi-turn> so the trap is read-at-the-point-of-authoring, not remembered. (No upstream PR needed — aimock's own multi-turn docs already spell this out, including the `extractLastUserMessage` behaviour and Gotchas; we'd just missed them.)
- `.dev/scripts/e2e-run.sh <suite>`: wraps `argusai setup → run → clean`, `unset WORKTREE_ID` (avoids the namespace container-name trap that bites the `argusai` CLI), and parses the summary line for pass/fail (`argusai run` always exits 0). For parallel worktree runs, keep using `e2e-gate.sh` / the flowcast flow (MCP path resolves namespaced names).

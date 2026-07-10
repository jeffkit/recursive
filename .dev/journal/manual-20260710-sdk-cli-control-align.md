# Manual edit: sdk-cli-control-align

**Date**: 2026-07-10
**Goal**: Align `query()` with Claude Agent SDK bidirectional control (canUseTool, streamInput, interrupt, hooks) and emit per-turn `result` in CLI streaming-input mode.
**Files touched**:
- `crates/recursive-cli/src/main.rs` — per-turn `result` + `finish_without_result`
- `crates/recursive-cli/src/cli/claude_json.rs` — `build_turn_result` test
- `crates/recursive-cli/src/cli/output.rs` — `finish_without_result` (prior)
- `sdk/typescript/src/controlSession.ts` — new control transport
- `sdk/typescript/src/query.ts` — control-backed `query()`
- `sdk/python/recursive_sdk/control_session.py` — new control transport
- `sdk/python/recursive_sdk/query.py` — control-backed `query()`
- SDK READMEs + tests
**Tests added**: TS query/control argv tests; Python control argv + interrupt; CLI `build_turn_result_matches_emitter`
**Notes**: `Agent.*` still uses headless `-H` one-shot; only `query()` opens the control channel (no `-H`).

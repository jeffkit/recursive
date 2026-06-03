# Manual edit: permission-hook-unify

**Date**: 2026-06-03
**Goal**: Fix B2 (dual PermissionHook types) and H1 (God Object AgentRuntime)
**Files touched**:
- `src/agent/types.rs` — removed fn-alias `PermissionHook`; kept `PermissionDecision`
- `src/tools/mod.rs` — extended `PermissionHook` trait: `ask_permission() -> bool` → `check() -> PermissionDecision`; made `args_preview_for_permission` pub; updated invoke/invoke_with_audit to handle Deny/Transform/Allow
- `src/lib.rs` — re-export `PermissionHook` from `tools` instead of `agent`
- `src/kernel.rs` — updated `TurnContext.permission_hook` to `Option<Arc<dyn PermissionHook>>`
- `src/run_core.rs` — updated `permission_hook` field type and call-site to `.check(...).await`
- `src/runtime.rs` — extracted `CheckpointState` struct (shadow, session_id, turn_index, writer, touched_files + snapshot methods); `AgentRuntime` now holds `checkpoints: CheckpointState` instead of 5 raw fields; updated builder and all call sites
- `src/tools/spawn_worker.rs` — updated `PermissionHook` import and field type
- `src/tools/sub_agent.rs` — updated `PermissionHook` import and field type
- `src/tui/backend.rs` — updated `TuiPermissionHook::ask_permission` → `check`; returns `PermissionDecision` via bool reply channel bridge
- `tests/integration.rs` — replaced fn-closure hook with struct implementing the trait

**Tests added**: none (existing tests updated to new trait method name)
**Notes**:
- B2: `tools::PermissionHook` is now the single hook type. The old fn-alias in `agent/types.rs` is deleted. The trait now returns `PermissionDecision` (Allow/Deny/Transform) instead of `bool`, preserving the transform capability previously only available via the fn-alias path.
- H1: `CheckpointState` reduces `AgentRuntime` from 19 fields to 15. The snapshot logic (pre/post snapshot + log append) lives in `CheckpointState` methods, keeping `AgentRuntime::run()` at a higher level of abstraction.

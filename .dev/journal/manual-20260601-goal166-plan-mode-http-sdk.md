# Manual edit: goal166-plan-mode-http-sdk

**Date**: 2026-06-01
**Goal**: Implement Goal 166 — Plan Mode 2.0 HTTP API & Python SDK support

## Files touched

### Rust
- `src/runtime.rs` — Added `pub fn plan_approval_gate(&self) -> Arc<PlanApprovalGate>` accessor so HTTP handlers can clone the gate without taking the runtime Mutex.
- `src/http.rs` — Multiple additions:
  - Import `crate::tools::plan_mode::PlanApprovalGate`
  - `SessionState`: added `pub plan_approval_gate: Arc<PlanApprovalGate>` field
  - `SseEvent`: added `PlanProposed { plan: String }` variant
  - `SessionDetailResponse`: added `status: String` and `pending_plan: Option<String>` fields
  - SSE `event_type` match: added `PlanProposed { .. } => "plan_proposed"` arm
  - `map_agent_event()`: added `AgentEvent::PlanProposed` → `SseEvent::PlanProposed` mapping
  - `create_session()`: extract gate from runtime before moving into Mutex
  - `get_session()`: reads status/pending_plan from session gate without locking runtime (avoids deadlock); uses `try_lock()` for messages/todos
  - Added `PlanConfirmRequest`, `PlanRejectRequest` structs
  - Added `session_plan_confirm` and `session_plan_reject` handlers
  - Registered `/sessions/{id}/plan/confirm` and `/sessions/{id}/plan/reject` routes
- `tests/http.rs` — Added 10 new plan-mode integration tests:
  - 404 tests for unknown sessions (confirm + reject)
  - 409 tests when no plan is pending (confirm + reject)
  - Status field tests (idle + plan_pending_approval)
  - Approve returns 200, reject returns 200
  - Edits parameter updates plan text
  - `map_agent_event` unit test for PlanProposed

### Python SDK
- `sdk/python/recursive_client/models.py` — Added `Optional` + `Literal` imports; added `status`/`pending_plan` fields (with defaults) to `SessionDetail`; added `PlanProposedMessage` dataclass.
- `sdk/python/recursive_client/client.py` — Imported `PlanProposedMessage`; added `approve_plan()` and `reject_plan()` methods.
- `sdk/python/recursive_client/__init__.py` — Exported `PlanProposedMessage`.
- `sdk/python/tests/test_client.py` — Added 8 new unit tests covering `approve_plan`, `reject_plan`, `PlanProposedMessage`, and updated `SessionDetail` fields.

## Tests added

- Rust: 10 new integration tests (in `tests/http.rs`)
- Python: 8 new unit tests (in `sdk/python/tests/test_client.py`)
- Total new tests: 18

## Deviations from spec

1. **TypeScript SDK skipped** — as specified, only Python SDK implemented (no TS source code in repo, only `node_modules`).
2. **`plan_approval_gate` accessor added** — spec assumed the field was already `pub`, but it was private. Added a `pub fn plan_approval_gate()` method to `AgentRuntime` instead of changing field visibility, which is a cleaner encapsulation boundary.
3. **`get_session()` uses `try_lock()`** — instead of blocking `lock().await`, the handler uses `try_lock()` for messages/todos and falls back to empty vectors when the agent is running. Status/pending_plan are always available because they're read from `SessionState.plan_approval_gate` without the Mutex. This avoids deadlock when the agent is suspended in `exit_plan_mode`.
4. **Tests in `tests/http.rs`** — new plan tests added to the existing `http.rs` integration test file (which follows the established project convention) rather than a new `tests/plan_mode_http.rs` file, to avoid duplicating the boilerplate module structure.

# Goal 166 — Plan Mode 2.0: HTTP API & SDK Support

> **Roadmap**: Phase 18 — Advanced Agent Patterns (18.3) / Phase 19 — SDK
> **Depends on**: Goal 165 (Plan Mode 2.0 core)
> **Design principle check**:
> - **Layered UI**: Core emits events; HTTP layer translates to SSE; SDK
>   layer exposes async approval methods. No core change needed.
> - **Additive**: New endpoints and SDK methods; existing behavior unchanged.
> - **Orthogonal**: HTTP and SDK layers don't know about TUI internals.

## Why

Goal 165 makes plan mode work in the TUI. But SDK users building automations
need to handle plan approval programmatically. Without HTTP/SDK support:

```python
# SDK user can't handle plan mode — agent would hang waiting for approval
async for event in run.stream():
    if event.type == "plan_proposed":
        # No way to approve or reject! Session hangs.
        pass
```

With this goal:

```python
async for event in run.stream():
    if event.type == "plan_proposed":
        print(event.plan)            # Show the plan
        decision = await ask_user()  # Or auto-approve
        if decision == "approve":
            await run.approve_plan()
        else:
            await run.reject_plan("Please use a simpler approach")
```

## What this goal does

### 1. HTTP API: `plan_proposed` SSE event

When the agent calls `exit_plan_mode(plan)` and the `PlanProposed` event is
emitted, the HTTP streaming endpoint
(`GET /sessions/:id/stream` / `GET /runs/:id/stream`) must emit a new
SSE event before suspending:

```
event: plan_proposed
data: {"type":"plan_proposed","session_id":"<id>","plan":"# My Plan\n\n..."}

```

The session stays alive (not terminated) — the HTTP session is in
`PlanPendingApproval` state. Subsequent SSE reconnects replay the
`plan_proposed` event so the client can re-display the plan.

### 2. HTTP API: `POST /sessions/:id/plan/confirm`

```
POST /sessions/:id/plan/confirm
Authorization: Bearer <token>
Content-Type: application/json

{ "edits": "optional replacement plan text" }
```

- If `edits` is present, the approved plan text used is the edited version
  (stored in the session's `PlanApprovalGate`)
- Calls `session.confirm_plan(edited_plan?)` on the runtime
- Returns: `{"status": "approved", "session_id": "..."}`
- Error 409 if session is not in `PlanPendingApproval` state

### 3. HTTP API: `POST /sessions/:id/plan/reject`

```
POST /sessions/:id/plan/reject
Authorization: Bearer <token>
Content-Type: application/json

{ "reason": "Please use a different approach" }
```

- Calls `session.reject_plan(reason)` on the runtime
- Returns: `{"status": "rejected", "session_id": "..."}`
- Error 409 if session is not in `PlanPendingApproval` state

### 4. Session state: `PlanPendingApproval`

`src/server/session.rs` (or equivalent HTTP session management):

Add `PlanPendingApproval` as a session status alongside `Running` / `Idle` /
`Completed` / `Error`. The `/sessions/:id` GET endpoint returns this status
so clients can discover in-progress plan reviews without streaming.

```json
{
  "session_id": "abc123",
  "status": "plan_pending_approval",
  "pending_plan": "# My Plan\n\n...",
  "created_at": "...",
  "updated_at": "..."
}
```

### 5. Python SDK: `plan_proposed` event + `approve_plan` / `reject_plan`

`sdk/python/recursive_sdk/models.py` — add `PlanProposedMessage`:

```python
@dataclass
class PlanProposedMessage:
    type: Literal["plan_proposed"] = "plan_proposed"
    plan: str = ""
    session_id: str = ""
```

`sdk/python/recursive_sdk/run.py` — `Run.stream()` yields `PlanProposedMessage`
when `event.type == "plan_proposed"`.

`sdk/python/recursive_sdk/run.py` — new methods:

```python
async def approve_plan(self, edits: str | None = None) -> None:
    """
    Approve the pending plan. Call after receiving a PlanProposedMessage.
    
    Args:
        edits: Optional replacement plan text if you want to modify the plan.
    """
    await self._http.post(
        f"/sessions/{self._session_id}/plan/confirm",
        {"edits": edits} if edits else {},
    )

async def reject_plan(self, reason: str = "") -> None:
    """
    Reject the pending plan. The agent will be notified with the reason
    and may revise and re-propose.
    """
    await self._http.post(
        f"/sessions/{self._session_id}/plan/reject",
        {"reason": reason},
    )
```

Also add `approve_plan` / `reject_plan` to `Agent.prompt()` — if a
`PlanProposedMessage` is received during a `prompt()` call, auto-approve
by default (to preserve the existing "fire and forget" semantics). Add an
optional `on_plan_proposed` callback parameter:

```python
result = await Agent.prompt(
    "help me refactor auth",
    options={"on_plan_proposed": lambda plan: print(f"Plan:\n{plan}") or "approve"},
)
```

### 6. TypeScript SDK: same pattern

`sdk/typescript/src/models.ts` — add `PlanProposedMessage`:

```typescript
export interface PlanProposedMessage {
  type: "plan_proposed";
  plan: string;
  sessionId: string;
}
```

`sdk/typescript/src/run.ts` — `Run.stream()` yields `PlanProposedMessage`.

`sdk/typescript/src/run.ts` — new methods:

```typescript
async approvePlan(edits?: string): Promise<void>;
async rejectPlan(reason?: string): Promise<void>;
```

`sdk/typescript/src/agent.ts` — `Agent.prompt()` gains an optional
`onPlanProposed` callback (default: auto-approve).

### 7. Integration test

`tests/plan_mode_http.rs`:

- Start the HTTP server with a `MockProvider` configured to:
  1. On first completion: return a `tool_use` for `enter_plan_mode`
  2. On second completion: return a `tool_use` for `exit_plan_mode` with a
     sample plan text
  3. On third completion: return an assistant text "Implementation complete."
- Stream the session events over HTTP
- Assert `plan_proposed` SSE event is received
- Call `POST /sessions/:id/plan/confirm`
- Assert the stream continues and completes

## Files to change

| File | Change |
|------|--------|
| `src/server/routes.rs` (or equivalent) | New `plan/confirm` and `plan/reject` endpoints |
| `src/server/session.rs` | `PlanPendingApproval` status; `pending_plan` field |
| `src/server/stream.rs` | Emit `plan_proposed` SSE event when `PlanProposed` received |
| `sdk/python/recursive_sdk/models.py` | `PlanProposedMessage` dataclass |
| `sdk/python/recursive_sdk/run.py` | `approve_plan` / `reject_plan` methods; yield `PlanProposedMessage` |
| `sdk/python/recursive_sdk/agent.py` | `on_plan_proposed` callback in `prompt()` |
| `sdk/python/tests/test_agent.py` | Tests for plan approval flow |
| `sdk/typescript/src/models.ts` | `PlanProposedMessage` type |
| `sdk/typescript/src/run.ts` | `approvePlan` / `rejectPlan` methods; yield `PlanProposedMessage` |
| `sdk/typescript/src/agent.ts` | `onPlanProposed` callback in `prompt()` |
| `sdk/typescript/tests/agent.test.ts` | Tests for plan approval flow |
| `tests/plan_mode_http.rs` (new) | Integration test for HTTP plan approval flow |

## Out of scope

- CLI (one-shot) plan approval via stdin (can be added as a small follow-on)
- Plan history / plan versioning (keep current plan; revision replaces it)
- Parallel plan proposals from sub-agents (sub-agents cannot use plan mode
  per Goal 165's `EnterPlanModeTool.call()` check)

## Acceptance

1. `cargo test --workspace` green
2. `cargo clippy --all-targets -- -D warnings` clean
3. Python SDK tests: `pytest sdk/python/tests/` green (including new plan tests)
4. TypeScript SDK tests: `npm test` in `sdk/typescript/` green
5. HTTP integration test: `plan_mode_http.rs` suite passes
6. `GET /sessions/:id` returns `"status":"plan_pending_approval"` while
   agent is waiting for plan confirmation
7. `POST .../plan/confirm` on a session NOT in plan-pending state returns
   HTTP 409 (not a panic)

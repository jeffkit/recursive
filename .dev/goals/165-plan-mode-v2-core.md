# Goal 165 — Plan Mode 2.0: Agent-Driven Read-Only Planning

> **Roadmap**: Phase 18 — Advanced Agent Patterns (18.3 Hierarchical Planning)
> **Design principle check**:
> - **Agent loop stays small**: Plan mode enforcement is a pre-dispatch gate
>   in the tool execution path, not a branch in `Agent::run`.
> - **Orthogonal**: `enter_plan_mode` / `exit_plan_mode` are standard tools;
>   the agent's loop treats them identically to any other tool call.
> - **Additive**: Existing `/plan on|off` user command and `PlanFirst` mode
>   continue to function. New tools augment, not replace.
> - **Layered UI**: Core emits `PlanProposed` / `PlanConfirmed` / `PlanRejected`
>   events (already exist). TUI adapts; HTTP/SDK adaptation is a follow-on
>   goal (166).

## Why

The current `PlanFirst` mode (`/plan on`) buffers tool calls and shows them
for user approval — this is "tactical" planning at the tool-call level. It
requires the user to manually enable plan mode before starting.

What's needed is **strategic planning**: the agent decides when to plan,
writes a complete proposal document, and waits for human approval before
executing *any* write operations. This matches Claude Code's `EnterPlanMode` /
`ExitPlanMode` pattern and is how complex tasks should be approached:

```
User: "help me refactor the authentication module"
  → Agent detects complexity → calls enter_plan_mode()
  → Agent reads codebase, designs approach (read-only)
  → Agent calls exit_plan_mode("Here's my plan: ...")
  → User reviews plan, edits if needed, approves
  → Agent executes with full tool access
```

Key improvements over current `PlanFirst`:

| | Current `PlanFirst` | Plan Mode 2.0 |
|---|---|---|
| Trigger | User `/plan on` | **Agent calls `enter_plan_mode` tool** |
| Timing | Manual | **Agent decides based on task complexity** |
| Read-only | No enforcement | **Write tools blocked during planning** |
| Plan content | Shows tool call list | **Full markdown document from agent** |
| User iteration | Not supported | **User can chat to refine plan** |

## What this goal does

### 1. `enter_plan_mode` tool

New file `src/tools/plan_mode.rs` — `EnterPlanModeTool`:

```rust
pub struct EnterPlanModeTool {
    runtime: Arc<Mutex</* planning mode flag */>>
}
```

- `name()`: `"enter_plan_mode"`
- `description()`: "Enter read-only planning mode to explore the codebase and
  design an implementation approach before making any changes. Call this when
  a task requires careful design or has significant scope. While in plan mode,
  write operations are blocked."
- `is_readonly()`: `true` (entering plan mode is itself side-effect-free)
- `should_defer()`: `false` (always eager — agent needs this available immediately)
- `execute({})`: Sets a `planning_mode = PlanMode::Exploring` flag on the
  agent context (via a shared `Arc<AtomicBool>` or by returning a special
  result that the kernel interprets). Returns:

```json
{
  "entered": true,
  "message": "Plan mode active. You are now in read-only mode. Explore freely, then call exit_plan_mode with your implementation plan when ready. DO NOT call write_file, apply_patch, or run_shell while in plan mode."
}
```

### 2. `exit_plan_mode` tool

In same file `src/tools/plan_mode.rs` — `ExitPlanModeTool`:

```rust
pub struct ExitPlanModeTool {
    plan_tx: tokio::sync::watch::Sender<Option<String>>,
}
```

Parameters:
```json
{
  "plan": {
    "type": "string",
    "description": "The complete implementation plan in markdown format. Should include: approach summary, steps to be taken, files to be modified, and acceptance criteria."
  }
}
```

- `name()`: `"exit_plan_mode"`
- `is_readonly()`: `true` (presenting a plan is read-only)
- `execute({ plan })`:
  1. Clears `planning_mode` flag (exits read-only enforcement)
  2. Emits `AgentEvent::PlanProposed { plan_text: plan, tool_calls: vec![] }`
     via the event sink
  3. Suspends execution: calls `runtime.await_plan_approval().await`
     (a `tokio::sync::oneshot` or watch channel that the TUI / HTTP layer
     feeds when the user confirms or rejects)
  4. On confirmation: returns `{"approved": true, "message": "Plan approved. You may now proceed with implementation."}`
  5. On rejection with reason: returns `{"approved": false, "reason": "..."}`
     — agent should revise and call `exit_plan_mode` again

### 3. Plan mode enforcement in the agent loop

`src/agent.rs` — add a pre-dispatch gate before executing each tool call:

```rust
// In the tool-call execution section of Agent::run_inner
if self.planning_mode == PlanMode::Exploring
    && !self.tools.is_readonly(&call.name)
    && call.name != "exit_plan_mode"
{
    // Return an error result back to the model instead of executing
    let result = format!(
        "ERROR: Cannot execute '{}' in plan mode. Plan mode is read-only. \
         Finish exploring, then call exit_plan_mode with your plan.",
        call.name
    );
    // push as a tool_result message, don't actually invoke the tool
    self.transcript.push(Message::tool(call.id, result));
    continue;
}
```

This is a **one-line check per call** — the loop body does not grow structurally.

### 4. `await_plan_approval` mechanism

The key architectural challenge: `exit_plan_mode` must suspend the agent loop
until the user responds via TUI or HTTP.

**Implementation**: Use an `Arc<tokio::sync::Mutex<Option<PlanApproval>>>` shared
between the tool and the runtime, combined with a `tokio::sync::Notify`:

```rust
pub struct PlanApprovalGate {
    /// Set by exit_plan_mode tool to signal "waiting for approval"
    pending_plan: Arc<RwLock<Option<String>>>,
    /// Notified by TUI/HTTP when user approves/rejects
    response_tx: Arc<tokio::sync::Mutex<Option<PlanApprovalResult>>>,
    notify: Arc<Notify>,
}

pub enum PlanApprovalResult {
    Approved,
    Rejected { reason: String },
}
```

`AgentRuntime` gains a `PlanApprovalGate` field. The `ExitPlanModeTool` holds
an `Arc<PlanApprovalGate>`. When `exit_plan_mode` is called:
1. Stores the plan text in `pending_plan`
2. Emits `PlanProposed` event
3. `notify.notified().await` — suspends

When TUI calls `rt.confirm_plan()`:
1. Sets `response_tx = Some(PlanApprovalResult::Approved)`
2. Calls `notify.notify_one()` — resumes the tool

### 5. TUI adaptation

The existing `PlanReview` modal already handles `AgentEvent::PlanProposed`.
Goal 147 wired the existing `tool_calls` display. This goal changes the
modal to display `plan_text` as a formatted markdown block instead of
a tool-call list.

`src/tui/ui/modal.rs`:
- `Modal::PlanReview { plan_text, .. }` — render `plan_text` with markdown
  formatting (already available via `syntect`/`pulldown-cmark` from Goal 159)
- Keep `y/Enter` → `confirm_plan()`, `n/Esc` → `reject_plan(reason)`,
  `e` → inline editing of `plan_text`

No new TUI events needed — `PlanProposed` / `PlanConfirmed` / `PlanRejected`
already exist.

### 6. System prompt update

`src/agent.rs` — `default_system_prompt()`:

Add a section on when to use plan mode:

```
## Planning Mode

Use enter_plan_mode when:
- The task requires exploring 3+ files before deciding what to change
- The task touches architectural boundaries (new module, new trait, API change)
- You are unsure of the correct approach and want to discuss options first

While in plan mode:
- Read files freely (read_file, list_dir, search_files)
- Think through trade-offs in your responses
- DO NOT call write_file, apply_patch, or run_shell
- When you have a clear plan, call exit_plan_mode with a markdown summary

Your plan should include:
1. What you understand about the current code
2. The approach you propose and why
3. Files you will modify and how
4. How you will verify the change is correct
```

### 7. Registration

`src/tools/mod.rs`:
- `build_standard_tools()` and `build_tools()` both register
  `EnterPlanModeTool` and `ExitPlanModeTool`
- Both tools receive the shared `PlanApprovalGate` at construction time
- `AgentBuilder` gains a `plan_approval_gate` field (auto-constructed if
  not provided, matching the default-config pattern)

## Files to change

| File | Change |
|------|--------|
| `src/tools/plan_mode.rs` (new) | `EnterPlanModeTool`, `ExitPlanModeTool`, `PlanApprovalGate`, `PlanMode` enum |
| `src/tools/mod.rs` | Register both tools; add `PlanApprovalGate` to `build_standard_tools` signature |
| `src/agent.rs` | `PlanMode` field; pre-dispatch read-only gate; `default_system_prompt()` plan section |
| `src/runtime.rs` | `PlanApprovalGate` field in `AgentRuntime`; `confirm_plan()` / `reject_plan()` updated to use gate |
| `src/event.rs` | No change — `PlanProposed` event already exists |
| `src/tui/ui/modal.rs` | Render `plan_text` as markdown in `PlanReview` modal |
| `src/lib.rs` | Re-export `PlanApprovalGate`, `PlanMode` |

## Out of scope

- HTTP/SDK `plan_proposed` event and confirmation endpoints (Goal 166)
- Task-list tracking across multi-step plans (the plan document serves
  this purpose; the agent follows the checklist naturally)
- Auto-detection of "complex tasks" that should trigger plan mode
  (the model decides based on the system prompt guidance)

## Acceptance

1. `cargo test --workspace` green — existing tests unmodified
2. `cargo clippy --all-targets -- -D warnings` clean
3. New unit tests in `src/tools/plan_mode.rs`:
   - `enter_plan_mode_returns_confirmation_message`
   - `exit_plan_mode_blocks_until_confirmed`
   - `exit_plan_mode_returns_rejection_reason`
   - `plan_mode_blocks_write_file`
   - `plan_mode_allows_read_file`
   - `plan_mode_allows_exit_plan_mode`
4. TUI integration: calling `enter_plan_mode` + `exit_plan_mode` via
   the mock agent in `recursive-tui` tests shows the `PlanReview` modal
5. `write_file` called while in plan mode returns an error message to the
   model (not a panic) — verified by unit test in `src/agent.rs`

## Notes for the agent

- The `PlanApprovalGate` must be `Send + Sync` and `Clone` (so both the
  tool and the runtime can hold a reference).
- `exit_plan_mode` must NOT hold any mutex lock while `await`ing — or you
  get a deadlock. Store the notification separate from the shared state.
- The gate's `pending_plan` field should be cleared when the tool starts
  waiting (set to `None`) to avoid stale state on a second invocation.
- The `planning_mode` field on `Agent` is separate from the existing
  `PlanningMode` enum. The existing `PlanFirst` / `Immediate` modes remain
  for the `/plan on|off` user command. The new `PlanMode::Exploring` is set
  by the `enter_plan_mode` tool and cleared by `exit_plan_mode`.
- Do not rename or remove the existing `PlanFirst` enum variant — it's
  used by the TUI backend for the `/plan` command and must remain for
  backward compatibility.
- Tests that call `confirm_plan` must do so from a separate task so the
  agent loop can `await` the notification without deadlocking.

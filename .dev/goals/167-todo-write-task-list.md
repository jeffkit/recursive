# Goal 167 — `todo_write` Tool: Agent Task List Management

> **Roadmap**: Phase 18 — Advanced Agent Patterns (complements 18.3)
> **Design principle check**:
> - **Orthogonal**: `todo_write` is a standard tool; agent loop doesn't change.
> - **Additive**: `TodoUpdated` event is new but doesn't break existing consumers.
> - **Independent**: Works standalone; can be used by Plan Mode 2.0 (Goal 165) to
>   track execution of a confirmed plan, but has no dependency on it.
> **Reference**: `fake-cc` — `TodoWriteTool` (`src/tools/TodoWriteTool/`)

## Why

Complex tasks need structured progress tracking. Currently, the agent:
- Either free-forms its progress in chat messages (hard to follow)
- Or writes to a file (doesn't surface to the runtime/UI)

The `todo_write` tool (matching Claude Code's pattern) gives the agent a
first-class way to maintain a live checklist that the TUI/SDK can display.

```
Agent: "I'll work through this step by step"
  → calls todo_write([
      {content: "Read existing auth code", status: "in_progress"},
      {content: "Design new interface", status: "pending"},
      {content: "Implement changes", status: "pending"},
      {content: "Add tests", status: "pending"},
    ])
  → TUI shows: ◉ 1/4 — Reading existing auth code...
```

**Relationship to Plan Mode (Goal 165)**:
- Plan Mode is about *designing the approach* (read-only phase + approval)
- `todo_write` is about *tracking execution* of any task, with or without plan mode
- When both are active: Plan Mode generates the plan → user approves →
  agent converts plan steps to `todo_write` items → executes

## What this goal does

### 1. `TodoItem` data type

`src/tools/todo.rs` (new file):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// Task description in imperative form (e.g., "Run tests")
    pub content: String,
    pub status: TodoStatus,
    /// Optional present-continuous form shown when in_progress
    /// (e.g., "Running tests"). Falls back to `content` if absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}
```

### 2. `todo_write` tool

In same file `src/tools/todo.rs` — `TodoWriteTool`:

Parameters:
```json
{
  "todos": {
    "type": "array",
    "description": "The complete updated todo list. Replaces the previous list entirely.",
    "items": {
      "type": "object",
      "properties": {
        "content": {"type": "string", "description": "Task description (imperative form)"},
        "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
        "active_form": {"type": "string", "description": "Present continuous form (optional)"}
      },
      "required": ["content", "status"]
    }
  }
}
```

`execute({ todos })`:
1. Validates that at most one item has `status: "in_progress"`
2. Updates shared `Arc<RwLock<Vec<TodoItem>>>` (stored in the tool at construction)
3. Emits `AgentEvent::TodoUpdated { todos: vec![] }` (new event variant)
4. Returns:

```json
{
  "updated": true,
  "count": 4,
  "in_progress": "Read existing auth code",
  "remaining": 3
}
```

`is_readonly()`: `false` (modifies session state)
`side_effect_class()`: `ToolSideEffect::ReadOnly` — updating a task list is
non-destructive and doesn't touch the filesystem; treat as low-side-effect.
Actually this is an in-memory state change, so use `ToolSideEffect::Mutating`
to be honest, but allow it inside plan mode (see Goal 165: `todo_write` should
be on the allowed list alongside `exit_plan_mode`).

### 3. `AgentEvent::TodoUpdated`

`src/event.rs`:
```rust
AgentEvent::TodoUpdated {
    todos: Vec<TodoItem>,
}
```

The `TodoItem` type is re-exported from `src/tools/todo.rs`.

### 4. `AgentRuntime` — todo list state

`src/runtime.rs`:
- Add `todo_list: Arc<RwLock<Vec<TodoItem>>>` field
- Initialize to empty
- `TodoWriteTool` holds a clone of the `Arc`
- New method: `pub fn current_todos(&self) -> Vec<TodoItem>` — for HTTP API

### 5. TUI integration

`src/tui/backend.rs`:
- Map `AgentEvent::TodoUpdated { todos }` → `UiEvent::TodoUpdated { todos }`

`src/tui/events.rs`:
- Add `UiEvent::TodoUpdated { todos: Vec<TodoItem> }`

`src/tui/app.rs`:
- Add `current_todos: Vec<TodoItem>` to `AppState`
- On `TodoUpdated`: update `current_todos`

`src/tui/ui/` — new display:
Show the task list in the TUI. Simple approach: render current todos as a
block at the bottom of the transcript panel when the list is non-empty:

```
─── Tasks (1/4 complete) ──────────────────────────────────────
  ✓ Read existing auth code
  ◉ Design new interface          ← in_progress (spinner)
  ○ Implement changes
  ○ Add tests
───────────────────────────────────────────────────────────────
```

Use existing `ratatui` components. The block is only visible when
`current_todos` is non-empty. It disappears automatically when the list
is cleared (all completed → `todo_write([])` or with all `completed`).

### 6. HTTP API: todos in session info

`GET /sessions/:id` response gains a `"todos"` field:

```json
{
  "session_id": "...",
  "status": "running",
  "todos": [
    {"content": "Read existing auth code", "status": "completed"},
    {"content": "Design new interface", "status": "in_progress"},
    {"content": "Implement changes", "status": "pending"},
    {"content": "Add tests", "status": "pending"}
  ]
}
```

### 7. System prompt guidance

`src/agent.rs` — `default_system_prompt()`:

Add guidance on when to use `todo_write` (port from fake-cc's `PROMPT`,
trimmed to fit the existing prompt style):

```
## Task List Management

Use todo_write to track progress on complex tasks with 3+ steps.

Guidelines:
- Create the list BEFORE starting work (capture requirements as todos)
- Mark exactly ONE task as "in_progress" at a time
- Mark a task "completed" immediately after finishing it
- Do NOT mark "completed" if tests are failing or work is partial
- Clear the list (call with empty array) when all tasks are done

Skip todo_write for:
- Single-step tasks
- Purely conversational responses
- Tasks completable in < 3 trivial steps
```

### 8. Registration

`src/tools/mod.rs`:
- `build_standard_tools()` and `build_tools()` both register `TodoWriteTool`
- `TodoWriteTool` is constructed with a clone of `runtime.todo_list`

## Files to change

| File | Change |
|------|--------|
| `src/tools/todo.rs` (new) | `TodoItem`, `TodoStatus`, `TodoWriteTool` |
| `src/tools/mod.rs` | Register `TodoWriteTool`; export `TodoItem` |
| `src/event.rs` | `AgentEvent::TodoUpdated { todos }` |
| `src/runtime.rs` | `todo_list` field; `current_todos()` method |
| `src/lib.rs` | Re-export `TodoItem`, `TodoStatus` |
| `src/agent.rs` | `default_system_prompt()` — todo guidance section |
| `src/tui/events.rs` | `UiEvent::TodoUpdated` |
| `src/tui/backend.rs` | Map `TodoUpdated` → `UiEvent::TodoUpdated` |
| `src/tui/app.rs` | `current_todos` field; apply `TodoUpdated` event |
| `src/tui/ui/transcript.rs` (or new `src/tui/ui/todos.rs`) | Todo block rendering |
| `src/server/routes.rs` | Add `todos` field to `GET /sessions/:id` response |

## Out of scope

- `todo_read` tool (not needed — the model owns the list and can reference
  its own calls in the transcript)
- Per-task sub-todos or hierarchical nesting
- Task persistence across sessions (in-memory only for now)
- TUI keyboard shortcut to manually mark tasks done (agent drives the list)

## Acceptance

1. `cargo test --workspace` green
2. `cargo clippy -- -D warnings` clean
3. New unit tests in `src/tools/todo.rs`:
   - `todo_write_replaces_list`
   - `todo_write_emits_event`
   - `todo_write_validates_single_in_progress`
   - `todo_write_empty_list_clears`
4. TUI test: calling `todo_write` with 3 items shows the task block;
   calling with empty array hides it
5. `GET /sessions/:id` includes `"todos": [...]` in response body

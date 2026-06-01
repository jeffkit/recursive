# Manual edit: goal167-todo-write

**Date**: 2026-06-01
**Goal**: Implement `todo_write` tool (Goal 167) — agent task-list management modelled after Claude Code's TodoWriteTool.

**Files touched**:
- `src/tools/todo.rs` (new, ~230 lines) — `TodoItem`, `TodoStatus`, `TodoWriteTool`
- `src/event.rs` — added `AgentEvent::TodoUpdated { todos }`
- `src/tools/mod.rs` — added `pub mod todo`, re-exported types, registered in `build_standard_tools`
- `src/runtime.rs` — added `todo_list: Arc<RwLock<Vec<TodoItem>>>` field, `current_todos()` method, re-registers `TodoWriteTool` in `set_event_sink` so the TUI sink reaches the tool
- `src/lib.rs` — re-exported `TodoItem`, `TodoStatus`, `TodoWriteTool`
- `src/config.rs` — added task-list management section to `default_system_prompt()`; bumped size test from 2 KiB to 4 KiB
- `src/main.rs` — imported `TodoWriteTool`, added it to `build_tools()` with NullSink placeholder
- `src/tui/events.rs` — added `UiEvent::TodoUpdated { todos }`
- `src/tui/backend.rs` — mapped `AgentEvent::TodoUpdated` → `UiEvent::TodoUpdated` in `map_agent_event`
- `src/tui/app.rs` — added `current_todos: Vec<TodoItem>` to `App`, initialised to empty, handled `UiEvent::TodoUpdated`
- `src/tui/ui/chat.rs` — added `todo_panel_height()` and `render_todo_panel()`, split layout to accommodate panel when non-empty
- `src/http.rs` — added `todos: Vec<TodoItem>` to `SessionDetailResponse`, populated from `runtime.current_todos()`

**Tests added**:
- `src/tools/todo.rs` — 5 unit tests covering basic write, multiple-in-progress rejection, clear, `active_form`, and side-effect class

**Notes**:
- `build_standard_tools` and `build_tools` register the tool with NullSink as a placeholder; `AgentRuntimeBuilder::build()` always overrides with the proper event_sink so events reach TUI/HTTP consumers.
- `set_event_sink` on `AgentRuntime` re-registers `TodoWriteTool` with the new sink so TUI can update its sink after construction (the TUI sets the sink post-build).
- Tool returns the full `todos` array in its JSON output, which lets the TUI backend parse state even without a direct Arc reference.
- Todo panel in the TUI is hidden (zero height) when the list is empty; shows up to 6 items when non-empty, capped to avoid dominating the screen.

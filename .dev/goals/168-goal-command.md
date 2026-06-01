# Goal 168 — `/goal` Command: Condition-Based Autonomous Loop

## Summary

Add a `/goal <condition>` slash command that lets the agent keep working across
turns until a lightweight judge model decides the condition is met — without the
user sending a prompt each turn.

Modeled after **Claude Code v2.1.139 `/goal`** (shipped 2026-05-11). Unlike
`schedule_wakeup` (which asks "when to run again?"), `/goal` asks "when to
stop?".

---

## Motivation

`run_loop` + `ScheduleWakeup` handles periodic/cron work.  
`/goal` handles **task completion** work: "keep going until all tests pass",
"refactor until clippy is clean", "write the feature until the e2e test passes".

Differences:

| | `/loop` (ScheduleWakeup) | `/goal` |
|---|---|---|
| Loop trigger | agent calls `schedule_wakeup` | automatic after every turn |
| Exit condition | agent stops scheduling | judge model evaluates condition |
| Use case | periodic tasks | task completion |

They are orthogonal — a goal loop can also call `schedule_wakeup` internally.

---

## Design

### 1. Session State

Add a `GoalState` struct to `AgentRuntime`:

```rust
pub struct GoalState {
    /// The completion condition as written by the user.
    pub condition: String,
    /// Current status.
    pub status: GoalStatus,
    /// Turns elapsed since /goal was set.
    pub turns: u32,
    /// Hard cap: stop regardless of condition after this many turns.
    pub max_turns: u32,
    /// Most recent judge model verdict (reason string).
    pub last_reason: Option<String>,
}

pub enum GoalStatus {
    /// Loop is running.
    Pursuing,
    /// Condition confirmed met — goal cleared.
    Achieved,
    /// User ran `/goal clear` or the turn budget was exceeded.
    Cleared,
}
```

`AgentRuntime` gains:
```rust
pub goal_state: Arc<RwLock<Option<GoalState>>>,
```

---

### 2. The Judge: `GoalEvaluator`

After every turn of `run_goal_loop`, call a small/fast model to evaluate the
condition. The evaluator receives:

```
System: You are a completion evaluator. Answer YES or NO.

User:
Condition: {{ condition }}

Recent transcript (last N messages):
{{ transcript_tail }}

Is the condition met? Answer YES or NO on the first line, then a short reason.
```

Implementation sketch:

```rust
pub struct GoalEvaluator {
    provider: Arc<dyn LlmProvider>,
}

impl GoalEvaluator {
    pub async fn evaluate(&self, condition: &str, transcript: &[Message]) -> GoalVerdict {
        // Build a minimal prompt; call provider with max_tokens=256
        // Parse first line for YES/NO, rest as reason
    }
}

pub struct GoalVerdict {
    pub achieved: bool,
    pub reason: String,
}
```

Default provider = same as the runtime's provider (no second model needed by
default). Operators can override via `AgentRuntimeBuilder::with_goal_evaluator`.

---

### 3. `run_goal_loop`

New method on `AgentRuntime`:

```rust
pub async fn run_goal_loop(
    &mut self,
    initial_prompt: impl Into<String>,
    condition: impl Into<String>,
    max_turns: u32,
) -> Result<Vec<RuntimeOutcome>>
```

Loop:
1. `run(prompt)` → get `RuntimeOutcome`
2. Increment `GoalState.turns`
3. If `turns >= max_turns` → set `GoalStatus::Cleared`, break, emit warning event
4. Call `GoalEvaluator::evaluate(condition, transcript_tail)`
5. If `achieved` → set `GoalStatus::Achieved`, emit `AgentEvent::GoalAchieved`, break
6. Else → set `last_reason`, emit `AgentEvent::GoalContinuing { reason }`, continue
7. `prompt` for next turn = `"(Goal: {{ condition }})\n\nPrevious attempt reason: {{ reason }}\n\nContinue."` 

---

### 4. Slash Command

Register `/goal` in `CommandRegistry` (TUI):

```
/goal <condition> [or stop after N turns]
/goal              → show current status (turns, reason, condition)
/goal clear        → clear the active goal immediately
/goal pause        → pause (not yet in v1, deferred)
```

Parsing: extract trailing `or stop after N turns` → `max_turns`. Default = 20.

`/goal` is an `Async` command handler that pushes `UserAction::SetGoal { condition, max_turns }`.

The TUI backend processes `SetGoal` by:
1. Setting `runtime.goal_state`
2. Kicking off `run_goal_loop` in the background task
3. Streaming events back via the event sink

---

### 5. HTTP API

```
POST /sessions/:id/goal
Body: { "condition": "all tests pass", "max_turns": 20 }
→ 200 { "status": "pursuing" }

DELETE /sessions/:id/goal
→ 200 { "status": "cleared" }

GET /sessions/:id
→ includes "goal": { "status": "pursuing", "condition": "...", "turns": 3, "last_reason": "..." }
```

New SSE event: `SseEvent::GoalAchieved { condition, turns }` and
`SseEvent::GoalContinuing { reason }`.

---

### 6. Python SDK

```python
run.set_goal("all tests pass", max_turns=20)
run.clear_goal()
# session detail includes run.goal
```

---

### 7. New Events

```rust
// agent/event.rs
AgentEvent::GoalSet { condition: String, max_turns: u32 },
AgentEvent::GoalContinuing { reason: String, turns: u32 },
AgentEvent::GoalAchieved { condition: String, turns: u32 },
AgentEvent::GoalCleared,
```

---

## Files to touch

| File | Change |
|------|--------|
| `src/runtime.rs` | Add `GoalState`, `GoalStatus`, `GoalEvaluator`, `run_goal_loop` |
| `src/event.rs` | Add `AgentEvent::Goal*` variants |
| `src/tui/commands.rs` | Register `/goal` slash command |
| `src/tui/events.rs` | Add `UserAction::SetGoal` / `ClearGoal` |
| `src/tui/backend.rs` | Handle `SetGoal` → launch `run_goal_loop` |
| `src/tui/app.rs` | Show goal status in status bar |
| `src/http.rs` | `POST/DELETE /sessions/:id/goal`; add `goal` field to `SessionDetailResponse`; new SSE events |
| `sdk/python/recursive_client/client.py` | `set_goal()`, `clear_goal()` |
| `sdk/python/recursive_client/models.py` | `GoalState` dataclass |
| `tests/http.rs` | Integration tests for goal endpoints |

---

## Out of scope (defer)

- Cross-session persistence (Codex /goal style) — session-scoped is enough for v1
- `/goal pause` / `/goal resume`
- Separate small model for judge (use main provider for now)
- TUI status-bar indicator (can be follow-up)

---

## Acceptance criteria

- [ ] `run_goal_loop` runs until judge says achieved or max_turns hit
- [ ] `/goal <cond>` in TUI starts the loop; `/goal` shows status; `/goal clear` stops
- [ ] `POST /sessions/:id/goal` starts loop; `DELETE` clears
- [ ] `GET /sessions/:id` returns `goal` field
- [ ] SSE emits `GoalContinuing` and `GoalAchieved` events
- [ ] Python SDK `set_goal`/`clear_goal` work
- [ ] At least 10 unit/integration tests
- [ ] `cargo test`, `clippy`, `fmt` all pass

---

## Effort

**M** — ~2-3 days. Most complexity is in the evaluator + loop + HTTP wiring.
The TUI slash command is straightforward given existing command machinery.

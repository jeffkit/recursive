# Goal 179 — A2A Async Mode + `a2a_task_check`

**Status**: planned  
**Author**: manual  
**Date**: 2026-06-02

## Problem

The current `a2a_call` blocks until the remote task completes (via polling or SSE).
For long-running remote tasks, the agent may want to:
1. Submit the task and get a task ID immediately (non-blocking)
2. Do other work in the meantime
3. Later check the result (via `schedule_wakeup` + `a2a_task_check`)

Implementing a webhook callback listener would require exposing an HTTP port, which
is complex. A polling-based approach using the existing `schedule_wakeup` mechanism
is simpler and sufficient.

## Solution

Two new capabilities:

### 1. `a2a_call` `async_mode: true` parameter
When set, `a2a_call`:
1. POSTs to `/message:send`
2. Returns the task ID and initial state **immediately** without polling
3. Output format: `TASK_ID: {id}\nSTATE: {state}\n(Use a2a_task_check to poll for results)`

### 2. `a2a_task_check` tool
A standalone tool that:
1. Takes `url`, `task_id`, and optional `authorization`
2. GETs `{url}/tasks/{task_id}`
3. Returns formatted status + any artifact text

### Recommended workflow with `schedule_wakeup`

```
Agent: a2a_call(url=..., prompt=..., async_mode=true)
       → "TASK_ID: abc123\nSTATE: TASK_STATE_SUBMITTED"
Agent: schedule_wakeup(wakeup_time="+30s", note="check a2a task abc123 at http://...")
       → (sleeps)
Agent: (wakes up) a2a_task_check(url=..., task_id="abc123")
       → "STATE: TASK_STATE_COMPLETED\nHello, world!"
```

## New parameters for `a2a_call`

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `async_mode` | boolean | false | If true, return immediately with task_id without waiting for completion |

## `a2a_task_check` parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `url` | string | yes | Base URL of the A2A server |
| `task_id` | string | yes | Task ID to check (from `a2a_call` async_mode output) |
| `authorization` | string | no | Optional Authorization header value |

## `a2a_task_check` output

```
STATE: TASK_STATE_COMPLETED
Hello, world!
```

Or for non-terminal states:
```
STATE: TASK_STATE_WORKING
(task not yet complete; check again later)
```

## Side effects

Both: `ToolSideEffect::External`.

## Tests

- `a2a_call` with `async_mode: true` → returns task ID without polling
- `a2a_task_check` with completed task → returns STATE + artifact text
- `a2a_task_check` with working task → returns STATE + hint to check later
- `a2a_task_check` with HTTP error → returns error string

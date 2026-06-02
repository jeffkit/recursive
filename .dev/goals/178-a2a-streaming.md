# Goal 178 — A2A Streaming (SSE) Support

**Status**: planned  
**Author**: manual  
**Date**: 2026-06-02

## Problem

`a2a_call` currently uses polling (`GET /tasks/{id}` every 2s) to wait for results.
A2A v1.0 also defines a real-time streaming mode via Server-Sent Events (SSE):
- Endpoint: `POST {base}/message:stream` (or `/message:send` with `Accept: text/event-stream`)
- The server streams events as the task progresses, including artifact deltas
- Final text arrives incrementally, without polling delay

## Solution

Extend `a2a_call` with an optional `streaming: true` parameter.
When set, the tool:
1. POSTs to `{base}/message:stream` with `Accept: text/event-stream`
2. Reads SSE events via `reqwest::Response::chunk()` (no `futures-util` needed)
3. Accumulates `text` from `TaskArtifactUpdateEvent` events
4. Stops when a `TaskStatusUpdateEvent` with a terminal state is received
5. Falls back to normal polling if the server returns non-SSE content-type

## SSE event format (A2A v1.0)

```
data: {"type":"TaskStatusUpdateEvent","task":{"id":"t1","status":{"state":"TASK_STATE_WORKING"}}}

data: {"type":"TaskArtifactUpdateEvent","task":{"id":"t1","artifacts":[{"parts":[{"text":"Hello "}],"index":0,"append":false}]}}

data: {"type":"TaskArtifactUpdateEvent","task":{"id":"t1","artifacts":[{"parts":[{"text":"world!"}],"index":0,"append":true}]}}

data: {"type":"TaskStatusUpdateEvent","task":{"id":"t1","status":{"state":"TASK_STATE_COMPLETED"}}}
```

Alternative JSON-RPC format also supported (look for `result.kind == "artifact-update"`).

## New parameter

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `streaming` | boolean | no | If true, use SSE streaming mode. Default: false. |

## Side effects

`ToolSideEffect::External` (unchanged).

## Tests

- SSE stream with multiple artifact events → concatenated text returned
- SSE stream with immediate COMPLETED status (no artifacts) → "(no artifact text)"
- Server returns non-SSE content-type → falls back to polling interpretation
- Timeout during SSE stream → returns accumulated text so far + timeout message

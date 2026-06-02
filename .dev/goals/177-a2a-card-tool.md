# Goal 177 — `a2a_card`: Agent Card Auto-Discovery

**Status**: planned  
**Author**: manual  
**Date**: 2026-06-02

## Problem

When calling a remote A2A agent, the caller must know the agent's capabilities before deciding
which transport mode to use (sync / streaming / async).  The A2A v1.0 spec defines a
well-known discovery endpoint: `GET /.well-known/agent.json` that returns an **Agent Card**
JSON document describing the agent.

## Solution

Add a new built-in tool `a2a_card` that:
1. Fetches `{url}/.well-known/agent.json`
2. Parses the Agent Card
3. Returns a concise human-readable summary of the agent's name, description,
   capabilities (streaming, pushNotifications), authentication scheme, and skills

This is the prerequisite for smart transport selection in Goal 178 (streaming).

## Parameters

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `url` | string | yes | Base URL of the A2A server (e.g. `https://agent.example.com`) |
| `authorization` | string | no | Optional `Authorization` header value |

## Output

A human-readable text summary, for example:

```
Agent: Weather Bot
Description: Provides current weather and forecasts.
Streaming: supported
Push notifications: not supported
Auth: Bearer token
Skills:
  - get_weather: Get current weather for a location.
  - get_forecast: Get a multi-day forecast.
```

## Side effects

`ToolSideEffect::External` — reads from an external HTTP server.

## Implementation plan

1. Add `AgentCard` struct (and nested types) to `src/tools/a2a.rs`
2. Implement `A2aCardTool` in `src/tools/a2a.rs`
3. Re-export from `src/tools/mod.rs` and register in `build_standard_tools`
4. Unit tests with raw TCP mock servers

## Tests

- Happy path: valid Agent Card JSON → formatted summary
- Missing required field `name` → graceful degradation (use `<unnamed>`)
- HTTP 404 → `ERROR: HTTP 404 ...`
- Malformed JSON → `Error::Tool`
- Streaming capability flag `true`/`false` displayed correctly

# Manual edit: docs-api-fix

**Date**: 2026-06-03
**Goal**: Fix critical API mismatches in documentation (v2 review feedback)
**Files touched**:
- website/en/guide/quickstart.md
- website/zh/guide/quickstart.md
- website/en/guide/examples.md
- website/zh/guide/examples.md
- website/en/guide/concepts.md
- website/zh/guide/concepts.md
- website/en/guide/index.md
- website/zh/guide/index.md
- website/en/guide/self-improve.md
- website/zh/guide/self-improve.md
- website/en/library/index.md, agent.md, events.md
- website/zh/library/index.md, agent.md, events.md, multi-agent.md
- website/en/sdk/python.md, typescript.md
- website/zh/sdk/python.md, typescript.md
- website/en/http-api/run.md
- website/zh/http-api/run.md
- website/en/cli/run.md
- website/zh/cli/run.md

**Tests added**: none

**Notes**:
P0 fixes:
- Replaced all `Agent::builder()` with `AgentRuntime::builder()`
- Replaced `outcome.final_message` with `outcome.final_text`
- Replaced `StepEvent` with `AgentEvent`, updated subscription pattern to use `ChannelSink`
- Corrected `FinishReason` variants: `NoMoreToolCalls`, `ProviderStop(s)`, `Stuck { repeated_call, repeats }` etc.
- Updated `AgentOutcome` → `RuntimeOutcome` struct

P1 fix:
- Removed claim that recipes are in the `examples/` directory; point to actual examples (`basic`, `with_tools`)

Additional fixes found during review:
- Rewrote SDK docs from scratch: actual package is `recursive_sdk` with `Agent.prompt()`/`Agent.create()`, not `RecursiveClient`; `RunResult.result` not `.final_message`
- Fixed HTTP API response shape: `POST /run` returns `{status, finish_reason, messages, usage}` not `{final_message}`
- Fixed SSE event names: `message`, `tool_call`, `tool_result`, `done` (not `llm_start`, `tool_start`, etc.)
- Fixed SSE consumption pattern: subscribe to `/sessions/:id/events` endpoint, not the run POST endpoint

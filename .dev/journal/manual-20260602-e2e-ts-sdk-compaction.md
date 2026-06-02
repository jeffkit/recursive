# Manual edit: e2e expansion — TypeScript SDK + context compaction

**Date**: 2026-06-02
**Goal**: Add two new e2e test scenarios requested in the D plan item:
1. **TypeScript SDK smoke test** (`21-typescript-sdk`) — validates `@recursive/sdk` works end-to-end: `Agent.create()` → `agent.send()` → `run.wait()` → `RunResult` fields
2. **Context compaction multi-turn** (`22-compaction`) — validates that the agent survives transcript compaction: two turns with `RECURSIVE_COMPACT_THRESHOLD=100`, second turn must still return `assistant` role response

**Files touched**:
- `e2e/Dockerfile` — add `nodejs` package; copy TS SDK dist to `/sdk/typescript/`
- `e2e/fixtures/21-typescript-sdk.json` — aimock fixture for `sdk-typescript-hello` message
- `e2e/fixtures/22-compaction.json` — aimock fixtures for `compaction-turn-1` and `compaction-turn-2`
- `e2e/tests/21-typescript-sdk.yaml` — TypeScript SDK smoke test suite
- `e2e/tests/22-compaction.yaml` — Context compaction multi-turn test suite
- `e2e/e2e.yaml` — register tests 21 and 22
**Tests added**: e2e tests 21 and 22
**Notes**:
- TypeScript SDK has zero runtime deps; the pre-built CJS dist is loaded via `require('/sdk/typescript/dist/index.js')` in Node.js
- Compaction is triggered via `RECURSIVE_COMPACT_THRESHOLD=100` env var (very low threshold); the 210-char first response exceeds it
- For compaction test, session-based `POST /sessions/:id/messages` is used (not the one-shot `/run`) to test multi-turn transcript compaction

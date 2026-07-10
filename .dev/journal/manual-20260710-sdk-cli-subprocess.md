# Manual edit: sdk-cli-subprocess

**Date**: 2026-07-10
**Goal**: Make Recursive SDK default to spawning the local `recursive` CLI with Claude-compatible `--output-format stream-json` (same model as Claude Agent SDK), keeping HTTP as an opt-in when `baseUrl` / `RECURSIVE_BASE_URL` is set.
**Files touched**:
- `sdk/typescript/src/{binary,wire,subprocess,agent,run,index}.ts`
- `sdk/typescript/tests/{cli,agent}.test.ts`
- `sdk/typescript/README.md`
- `sdk/python/recursive_sdk/{binary,wire,cli,agent,run,__init__}.py`
- `sdk/python/tests/test_cli.py`
- `sdk/python/README.md`
- `website/en/sdk/typescript.md`
**Tests added**:
- TS: wire parsing, argv building, CLI Run streaming, Agent.create CLI mode
- Python: same coverage via `tests/test_cli.py`
**Notes**:
- Multi-turn uses per-turn process + `-r <session_id>` resume (CLI does not yet emit per-turn `result` while keeping stdin open for follow-ups).
- Session admin APIs (`listSessions`, `forkSession`, …) remain HTTP-only.
- Supersedes the g165 `recursive daemon` proposal for the default path; HTTP transport retained for remote/shared-server use.

## Follow-up: query() API (same day)

Added Claude Agent SDK–compatible `query()` / `ClaudeAgentOptions`:
- TS: `sdk/typescript/src/query.ts` — `query({ prompt, options })` → AsyncGenerator + `interrupt()`/`close()`; `result` yielded in-stream
- Python: `sdk/python/recursive_sdk/query.py` — async `query(prompt=..., options=...)`
- Option name mapping: `maxTurns`, `permissionMode: bypassPermissions`, `pathToClaudeCodeExecutable`, preset `systemPrompt.append`, etc.
- Tests: `tests/query.test.ts`, `tests/test_query.py`

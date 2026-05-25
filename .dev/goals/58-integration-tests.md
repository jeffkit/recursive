# Goal 58 — Integration Full-Stack Test

**Roadmap**: validation — multi-feature integration test

**Design principle check**:
- Implemented as: new test file `tests/integration.rs`. No product code
  changes.
- Does NOT modify any source under `src/`.

## Why

Individual features have unit tests, but no test exercises the full stack:
Agent + Compactor + Hooks + Skills + Permission Hook + Tool Transport
all working together. Feature interaction bugs hide in the gaps between
unit tests.

## Scope (do exactly this, no more)

### 1. `tests/integration.rs` — new file

Write 3-5 integration tests using `MockProvider` + real filesystem (tempdir):

#### Test 1: Full agent run with hooks + compaction
- Register a counting hook
- Set compactor threshold very low (e.g. 500 chars)
- Run agent with a multi-step goal (MockProvider scripted to call tools)
- Assert: hook fired for each tool call, compaction triggered at least once
- Assert: agent completes with `NoMoreToolCalls`

#### Test 2: Permission hook + sub-agent inheritance
- Set parent permission_hook to deny "run_shell"
- Spawn sub-agent via delegate tool
- Assert: sub-agent is also denied "run_shell"
- Assert: sub-agent can still use allowed tools (read_file)

#### Test 3: Skill index injection
- Create a tempdir with `.recursive/skills/test.md`
- Run agent — verify skill index appears in system prompt messages
- Verify `load_skill` tool returns the skill content

#### Test 4: Session pause + resume
- Run agent, save transcript
- Load transcript, resume from step N
- Assert: agent picks up where it left off

#### Test 5: Tool Transport abstraction
- Create agent with `LocalTransport` explicitly set
- Run a goal that uses `read_file` + `run_shell`
- Assert: behavior identical to default (proves transport layer is wired)

### 2. No changes to `src/`

This goal ONLY adds tests. If a test reveals a bug, document it in the
final message but do NOT fix it — that's a separate goal.

## Acceptance

- `cargo test` green (all new integration tests pass)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- No changes to any file under `src/`
- Tests exercise real feature interactions, not just mocks

## Notes for the agent

- Use `tempfile::TempDir` for filesystem isolation.
- Use `MockProvider` from `src/llm/mock.rs` — it supports scripted
  responses including tool calls.
- For the compaction test: set `compactor_threshold_chars` to something
  very small so it triggers quickly.
- For permission hook: create a closure that checks tool name and returns
  `PermissionDecision::Deny` for "run_shell".
- Import paths: `use recursive::{Agent, ToolRegistry, ...}` — check
  `src/lib.rs` for what's publicly exported.
- If you find that some feature is not publicly exported (e.g. HookRegistry
  or SessionFile), note it in your final message. Don't modify src/lib.rs.

# Goal 387 — Wire ACP ClientReadFile / ClientWriteFile to the session registry

**Roadmap**: ACP correctness + tool-context hygiene. The standard registry
currently exposes ACP client-FS tools to non-ACP sessions, while ACP session
setup constructs capability-bound tool instances and immediately drops them.
This is both a schema-token footgun and a real session-wiring defect.

**Design principle check**:
- Implemented as: make ownership and registration explicit. Non-ACP sessions
  must not eagerly expose ACP-only tools; an ACP session with a declared FS
  capability must register the tool instance bound to that session's
  `AcpClientFsState`.
- ❌ Does NOT branch `run_core.rs::run_inner` or change the agent loop.
- ❌ Does NOT rename ACP tool names or change protocol framing.
- ❌ Does NOT broaden this goal into an `AgentTool` / general deferred-tool
  audit; that is a separate finding.

## Why (verified 2026-08-04)

There are two coupled defects:

1. **`src/tools/registry.rs:791-805`** always registers plain
   `ClientReadFile` / `ClientWriteFile` instances in
   `build_standard_tools`. They have no `is_deferred()` override, so normal
   CLI/TUI/HTTP sessions can see tools whose capability checks always fail.
2. **`src/acp/server.rs:612-635`** creates capability-bound instances during
   ACP session setup, but stores them only in local `_tool` bindings. Those
   `Arc`s are dropped without being inserted into the session runtime's
   registry. The following `"tool ready"` log lines are therefore false.

`src/tools/client_fs.rs:177-182` also says the client-read implementation still
falls back to local filesystem behaviour while real ACP bridge forwarding is
unfinished. This goal must fix registration/visibility honestly; it must not
claim that editor-buffer RPC is complete if that transport work remains.

## Scope (do exactly this, no more)

### 1. Establish one explicit ACP registration seam

- Read how `AcpSession`, `AgentRuntime`, and `ToolRegistry` are owned before
  changing signatures.
- Add the smallest API needed to register or construct session-local tools
  before the `AcpSession` is inserted. Prefer building the final registry /
  runtime once over mutating shared global state after insertion.
- When `fs.readTextFile` is declared, register exactly one `ClientReadFile`
  bound to that session's `AcpClientFsState` and timeout configuration.
- When `fs.writeTextFile` is declared, register exactly one
  `ClientWriteFile` bound to the same session-local state.
- Read and write capabilities are independent: declaring one must not expose
  the other.
- Remove the dead `_tool` bindings and emit `"tool ready"` only after real
  registration succeeds.

### 2. Remove ACP-only tools from normal eager surfaces

- Stop unconditionally registering `ClientReadFile` / `ClientWriteFile` in
  `build_standard_tools`, or introduce an explicit ACP-only builder input that
  is absent for normal callers.
- Adding `is_deferred() -> true` is acceptable as defence in depth, but it is
  **not** a substitute for session-local ACP registration and cannot be the
  only shipped change.
- Update `src/tools/client_fs.rs` module documentation and
  `src/tools/registry.rs` comments to describe the actual ownership contract.
- Do not rename the tool specs.

### 3. Verify every tool-list surface

Trace how tool specs are produced for:

- normal CLI / TUI / HTTP runtimes;
- MCP `tools/list`;
- ACP session runtimes.

Normal non-ACP surfaces must not advertise these ACP-only tools. ACP should
advertise only the capability-declared tools, backed by the same registered
instances that execute calls. If MCP intentionally exposes a broader list,
document the exact reason and add a test that prevents advertise-but-fail
behaviour; do not leave the result implicit.

### 4. Integration tests

Add tests at the registry/ACP session boundary, not only unit tests for a
boolean flag:

1. no FS capabilities → neither tool is present;
2. read only → `ClientReadFile` present, `ClientWriteFile` absent;
3. write only → `ClientWriteFile` present, `ClientReadFile` absent;
4. both → both present exactly once;
5. two ACP sessions with different capabilities/state do not share or leak
   registry state;
6. normal `build_standard_tools` eager specs contain neither tool.

Where the ACP test harness can execute a tool call without implementing new
transport work, assert that execution reaches the capability-bound instance.
If real client RPC remains unavailable, assert registry identity/state and
record the transport limitation in the journal rather than faking success.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` except the minimum
  registry accessor/builder API if ownership makes it unavoidable.
- ACP wire protocol framing and unrelated ACP request handlers.
- Non-ACP tools' deferred flags, especially `AgentTool`.
- `.dev/flows/`, `.flowcast/`, or gate definitions.

## Acceptance

- No dead `let _tool = Arc::new(ClientReadFile...)` /
  `ClientWriteFile...` binding remains in `src/acp/server.rs`.
- No log claims a client-FS tool is ready before it is registered.
- The six capability/registry tests above exist and pass by name.
- A normal-runtime test proves ACP client-FS tools are absent from eager
  specs/registry.
- ACP read-only, write-only, both, and no-capability lists match their
  declared capabilities and executable registry entries.
- `cargo test -p recursive-agent` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  clean; `cargo fmt --all -- --check` clean.
- Journal: `.dev/journal/manual-20260804-goal387-acp-tools-gate.md` records the
  final registry ownership, tool-list surfaces checked, exact test filters,
  and whether editor-buffer RPC is still a documented follow-up.

## Notes for the agent (traps)

- **No deferred-only escape hatch.** Such a patch can hide the tools from one
  model surface while leaving ACP's capability-bound instances unregistered.
- Do not mutate a global/shared registry with session-specific state; ACP
  sessions may declare different capabilities and run concurrently.
- Deferred tool discovery (`ToolSearch`) and MCP `tools/list` are different
  surfaces. Verify both instead of assuming `is_deferred()` controls all
  clients.
- `ClientReadFile` currently documents fallback/local-file behaviour. Keep the
  description honest unless this goal truly wires client RPC end-to-end.
- Preserve sandbox root handling and the existing tool names; this goal is
  registry/wiring, not a filesystem-policy rewrite.

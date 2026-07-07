# Goal 324 — AG-UI interrupt/resume (Pattern 2 HITL)

**Roadmap**: Phase 17 (Production Hardening) — extends the AG-UI
transport (`crates/agui-{protocol,client,tui}` + `src/http/handlers.rs`)
landed in g268 / g143 with the interrupt-aware run lifecycle defined by
the current AG-UI spec (`docs.ag-ui.com/concepts/interrupts`).

**Design principle check**:
- Implemented as: new wire types in `crates/agui-protocol`, new
  resume-correlation + interrupt-emit paths in `src/http/handlers.rs`,
  reusing the existing `session` resume + `permission_pipeline` ask
  infra. The AG-UI run already ends with `RunFinished` — this goal
  widens that event's `outcome` and teaches the handler to *end the
  turn early* when a tool needs user input, then *resume from
  checkpoint* on the next `RunAgentInput`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`
  (invariant #1). The interrupt decision is made at the tool-result
  boundary in the AG-UI event bridge, not in the ReAct loop.
- ❌ Does NOT break tool-call ↔ tool-result pairing (invariant #8).
  The pending tool_call is left in the transcript as
  `Role::Assistant(tool_calls=[id])` with **no** `Role::Tool` result
  yet; on resume the `resume[].payload` is injected as the matching
  `Role::Tool` message before the loop continues. This is exactly the
  "orphan tool call" shape `cmd_resume` already handles
  (`--orphans` policy in `crates/recursive-cli/src/cli/resume.rs`).

## Why

Recursive's AG-UI server (`src/http/handlers.rs::agui_run`) currently
runs every turn to completion: `RunFinished` is always emitted with
`result: None` (handlers.rs:1531) and `RunAgentInput.resume` is never
read (zero grep hits). This is the pre-interrupt AG-UI lifecycle —
fine for autonomous runs, but it means the agent **cannot pause mid-turn
to ask the user a question** and survive a disconnect. The current
permission ask path (`src/tools/permission_pipeline.rs`,
`src/tools/plan_mode.rs`) only works in the local TUI/CLI where the
process stays alive and a human is at the terminal.

The AG-UI spec's interrupt-aware lifecycle
(`RunFinished { outcome: { type: "interrupt", interrupts: [...] } }`
+ next run carries `resume: [{interruptId, status, payload}]`) is the
canonical Pattern-2 answer: the run *ends*, state is dumped via
`StateSnapshot` / `MessagesSnapshot`, and a later run *resumes* by
`interruptId`. Recursive already has every ingredient this needs
(transcript = MessagesSnapshot, session resume = checkpoint restore,
permission_pipeline = the ask source, invariant #8 = the pairing
guarantee) — we just haven't wired them to the AG-UI wire types.

This goal adds Pattern-2 HITL to the AG-UI transport so an AG-UI
client (e.g. `crates/agui-tui`, or a future web client) can pause a
run for user input, survive the pause, and continue.

## Scope (do exactly this, no more)

### 1. `crates/agui-protocol/src/events.rs` — model the interrupt outcome

Upgrade `RunFinished` and add the `Interrupt` type per the current
spec. Replace the loose `result: Option<Value>` with a discriminated
`outcome` union (keep `result` as a serde alias for back-compat with
our own older events if needed, but new emission uses `outcome`).

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum RunFinishedOutcome {
    Success,
    Interrupt { interrupts: Vec<Interrupt> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interrupt {
    pub id: String,
    pub reason: String,                 // "tool_call" | "input_required" | "confirmation" | "ns:custom"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,   // binds to a prior ToolCall*
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>, // JSON Schema for resume.payload
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,     // ISO-8601
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}
```

`RunFinished` becomes:
```rust
pub struct RunFinished {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunFinishedOutcome>,
    // Legacy: keep `result` for back-compat deserialization only.
    // New code MUST emit `outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(flatten)]
    pub base: BaseEvent,
}
```

### 2. `crates/agui-protocol/src/input.rs` — upgrade `Resume` to v2

Replace the legacy `Resume { id, value }` with the spec shape:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    pub interrupt_id: String,
    pub status: ResumeStatus,           // "resolved" | "cancelled"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeStatus { Resolved, Cancelled }
```

`RunAgentInput.resume` stays `Option<Vec<Resume>>`. Add a
`#[serde(default)]` so old clients omitting it still parse.

### 3. `src/http/handlers.rs::agui_run` — consume `resume[]`

After parsing `RunAgentInput`, if `input.resume` is `Some(items)` and
non-empty:

1. Load the session for `input.thread_id` (existing
   `sanitize_thread_id_for_session` + session loader).
2. For each `Resume` item, find the open interrupt by `interrupt_id`.
   Open interrupts are tracked in session metadata (see §4 below).
   - If any `interruptId` cannot be correlated → emit `RunError` with
     a clear message and stop.
   - If the set of `resume[]` does not cover **every** open interrupt
     → `RunError` (spec contract rule 3).
3. For each resolved interrupt with a `tool_call_id`:
   - Inject a `Role::Tool` message into the transcript with
     `tool_call_id = <bound id>` and `content = payload` (serialised).
     This satisfies invariant #8 — the matching `Role::Assistant`
     with the tool_call already exists from the interrupted run.
   - For `status: "cancelled"`, inject a `Role::Tool` with a sentinel
     content like `"[interrupt cancelled by user]"` so the pairing
     holds and the model can react.
4. Clear the open-interrupt set in session metadata, then resume the
   run via the existing `run_resumed` / kernel path — **no new branch
   in `run_inner`**, just feed the resumed transcript as seed.

### 4. `src/http/handlers.rs::agui_run` — emit an interrupt

Wire the emit path to the existing permission ask. The cleanest
minimal cut:

- When the AG-UI event bridge (`AguiConverter` or the driver task in
  handlers.rs) observes a tool call whose `permission_pipeline` check
  returns `CheckOutcome::Ask { ... }` (i.e. would block for a human),
  **do not block**. Instead:
  1. Emit `ToolCallArgs` (already emitted) but **suppress**
     `ToolCallEnd` / `ToolCallResult`.
  2. Emit `StateSnapshot` (current session state) and
     `MessagesSnapshot` (current transcript messages) — spec requires
     these before the interrupting `RunFinished`.
  3. Emit `RunFinished { outcome: Interrupt { interrupts: [{
        id: <fresh id>, reason: "tool_call", tool_call_id: <tc id>,
        message: <ask prompt>, response_schema: {approved: bool, ...},
        ... }] } }`.
  4. Persist the open interrupt (`id`, `tool_call_id`, `expires_at`)
     into the session metadata so the next `resume` can correlate.
  5. End the run — return from the driver task. The AG-UI SSE stream
     closes cleanly.

If wiring `permission_pipeline`'s `Ask` outcome into the AG-UI bridge
is too invasive for one run, implement the emit path behind a minimal
**test-only trigger** instead: a tool (or a `RunAgentInput` field
like `interrupt_before: [toolName]`) that forces an interrupt before
a named tool executes. This keeps the emit path testable end-to-end
without rewiring permissions. **State explicitly in the journal which
cut you took.** Wiring the real `permission_pipeline.Ask` is then a
follow-up goal (g325 candidate).

### 5. Tests

- `crates/agui-protocol/src/events.rs` `#[cfg(test)] mod tests`:
  round-trip `RunFinished` with `outcome: Interrupt{...}` through
  serde; assert the wire JSON has `"outcome":{"type":"interrupt",...}`.
  Round-trip `Resume` with both `Resolved`+`payload` and `Cancelled`.
- `crates/agui-protocol/src/input.rs` tests: `RunAgentInput` with
  `resume: [...]` parses; old payload without `resume` still parses.
- `tests/agui_e2e.rs`: a full interrupt→resume cycle:
  1. `POST /agui` with a goal that triggers the interrupt path; assert
     the SSE stream ends with `RunFinished { outcome: interrupt, ... }`
     and that a `MessagesSnapshot` was emitted before it.
  2. `POST /agui` on the **same threadId** with `resume: [{interruptId,
     status: "resolved", payload: {approved: true}}]`; assert the run
     continues and finishes with `RunFinished { outcome: success }`,
     and that a `ToolCallResult` for the bound `toolCallId` appears.
  3. Negative: a `resume` with a bogus `interruptId` → `RunError`.
  4. Negative: a `resume` that covers only 1 of 2 open interrupts →
     `RunError`.

If the full HTTP integration harness is too costly for the interrupt
path, the protocol-level round-trip tests are **mandatory**; the HTTP
integration tests are stretch goals. At minimum, the interrupt
emit + resume consume must be exercised at the
`AguiConverter`/event-bridge unit level with a mock provider.

## Acceptance

- `cargo test --workspace` — green (existing + new tests pass)
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — applied
- `crates/agui-protocol` round-trips `RunFinished.outcome = Interrupt`
  and `Resume { interruptId, status, payload }` (unit tests)
- `src/http/handlers.rs` reads `RunAgentInput.resume` (grep
  `\.resume` non-zero in that file) and emits at least one
  `RunFinished` with a non-`None` interrupt outcome on the interrupt
  path (grep `RunFinishedOutcome::Interrupt` non-zero)
- A test (unit or integration) demonstrates a full
  interrupt→resume cycle producing a `ToolCallResult` for the
  originally-interrupted `toolCallId` on the resumed run
- invariant #8 (`tests/invariants/tool_call_pairing.rs`) still passes
- invariant #1 (`tests/invariants/loop_size_orthogonality.rs`) still
  passes — no new branch in `run_inner`

## Notes for the agent

- **Read first**:
  - `.dev/AGENTS.md` — invariants #1, #3, #5, #8
  - `crates/agui-protocol/src/events.rs` + `src/input.rs` — current
    (pre-interrupt) wire types
  - `src/http/handlers.rs` around `agui_run` (line ~1207) and the
    driver task that emits `RunFinished` (line ~1531)
  - `src/tools/permission_pipeline.rs` — `CheckOutcome::Ask` is the
    emit trigger you want to wire (or defer per §4)
  - `crates/recursive-cli/src/cli/resume.rs` + `src/session/` — the
    existing orphan-tool-call / resume machinery; the AG-UI resume
    path is the same shape
  - The spec: `https://docs.ag-ui.com/concepts/interrupts` (contract
    rules, reason taxonomy, tool-bound interrupt audit trail)
- **Spec contract rules to honour** (from the interrupts doc):
  1. Same `threadId` for resume
  2. `resume[].interruptId` must reference an `id` from the
     interrupted run's `interrupts[]`
  3. A single `resume` must address **every** open interrupt
  4. Pending interrupts block new input — any `RunAgentInput` on a
     thread with open interrupts that omits `resume` → `RunError`
  5. Idempotent: same `(threadId, interruptId, status, payload)` safe
     to replay
  6. `expiresAt` stale → `RunError`
- **Tool-bound interrupt audit trail**: in the interrupted run emit
  `ToolCallArgs` only; in the resumed run emit `ToolCallResult`
  against the **original** `toolCallId` — do NOT re-emit
  `ToolCallStart`/`Args`/`End`. This is the spec's exact rule and it
  matches invariant #8.
- **Back-compat**: our own `agui-tui` and `agui-client` consume these
  types. After upgrading `RunFinished`/`Resume`, update
  `crates/agui-client` and `crates/agui-tui` to match (they're small).
  The `RunFinished.result` legacy field keeps old event streams
  parseable.
- **Traps**:
  - Don't block the SSE thread on the permission ask — the whole
    point is to *end* the run. The ask becomes the interrupt's
    `message` + `response_schema`, not a synchronous wait.
  - Don't lose the open-interrupt record if the process crashes
    between emit and resume — persist it in the session metadata on
    disk *before* emitting `RunFinished`.
  - `ResumeStatus` serde must be `lowercase` ("resolved"/"cancelled"),
    not camelCase — spec uses lowercase.
- **DO NOT modify** `src/run_core.rs::RunCore::run_inner` (invariant
  #1) or `src/kernel.rs`. The interrupt logic lives in the AG-UI
  bridge + session layer.
- **DO NOT modify** `.dev/` files other than the journal entry.
- **Out of scope** (follow-up goals): wiring the real
  `permission_pipeline.Ask` end-to-end if you took the test-only
  trigger cut (g325); ACP integration (separate discussion); AG-UI
  `parentRunId` branching/time-travel semantics; `expiresAt` TTL
  enforcement beyond a parsed-and-stored check.

**Status: TODO**.

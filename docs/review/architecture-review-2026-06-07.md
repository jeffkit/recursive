# Architecture and Code Review: Recursive

**Date**: 2026-06-07  
**Reviewer**: Claude (architecture-critic agent)  
**Scope**: Full codebase architecture review (`src/`, key modules)

---

## Executive Summary

Recursive is a well-conceived Rust agent kernel. The core invariants (immutable loop, tool orthogonality, sandbox) are largely upheld, and the kernel/runtime split is a genuinely sound design. However, accumulated self-improvement iterations have left the codebase with significant architectural drift: `AgentRuntime` has become a monolith disguised as a wrapper; `ToolRegistry` carries sprawling cross-cutting concerns rather than a single responsibility; the multi-agent layer (multi.rs, spawn_worker, sub_agent, team_manage, a2a) is a parallel system with no coherent integration model; and two places in production code use `expect()` on `std::sync::RwLock` in async contexts, which will panic the entire process on lock poisoning. The kernel-level `SideEffect` type is documented but never defined — a stale comment that signals the architectural intent was abandoned mid-implementation.

---

## Architecture Issues

### Critical

**C-1. `AgentRuntime` has too many responsibilities (800+ LOC of public API)**  
File: `src/runtime.rs` (lines 242–1051)

The struct owns: transcript, event sink, compactor, checkpoint state, todo list, plan approval gate, plan mode request gate, goal state, a message queue, a deferred event, and a session-closed flag. Its `impl` block exposes 28 public methods plus three separate loop entry points (`run`, `run_goal_loop`, `run_loop`, `run_event_loop`). This violates the project's own Invariant #1 spirit: capabilities are not inside `Agent::run`, but they are increasingly inside `AgentRuntime`, which is the effective agent. When a new goal adds cross-turn state it lands here, creating unbounded growth. The correct seam is a `RunStrategy` or `TurnPolicy` trait that `AgentRuntime` delegates to, leaving the runtime itself responsible only for transcript accumulation and event forwarding.

**C-2. Two `expect()` calls on `std::sync::RwLock` in async code will panic the entire process**  
File: `src/tools/plan_mode.rs` (lines 102, 380)

```rust
let guard = self.response.read()
    .expect("PlanApprovalGate response lock poisoned");
```

This is in `wait_for_approval`, which is called from within a `tokio::spawn` task that runs while the agent turn is in flight. If any other thread panics while holding this lock — a `JoinSet` task panic, for instance — the poisoned-lock `expect` will cause a secondary panic in the forwarder task. In the HTTP server this kills the entire process. The fix is `match self.response.read() { Ok(g) => g, Err(poisoned) => poisoned.into_inner() }` or migrating to `tokio::sync::RwLock` which does not poison. This is a blocker in any multi-tenant deployment.

**C-3. `ToolRegistry::fork()` is a documented abstraction that does nothing**  
File: `src/tools/mod.rs` (lines 423–437)

The method's own doc comment says "For now, this is equivalent to `clone()`." Three call sites in spawn_worker and sub_agent were changed specifically away from `fork()` because it "silently defeated" the intent. The method exists as a named extension point but provides false safety to any future caller who reads the doc. Either delete it and the associated comment, or implement the per-tool `fork` protocol for stateful tools. A named no-op abstraction is more dangerous than no abstraction.

---

### High

**H-1. `SideEffect` is documented but never implemented**  
File: `src/kernel.rs` (line 10)

The module-level doc for `kernel.rs` lists `SideEffect` as a first-class type it provides. There is no `SideEffect` struct anywhere in the codebase. The distinct `ToolSideEffect` enum in `tools/mod.rs` is unrelated. The documented architecture promises a mechanism to return background-job and scheduled-wakeup effects from `TurnOutcome` — instead those effects escape via shared `Arc<Mutex<>>` slots threaded through tools and checked externally by `run_event_loop`. This works but is invisible to the type system and untestable in isolation.

**H-2. Transcript cloned in full on every kernel turn**  
File: `src/kernel.rs` (lines 267–290), `src/runtime.rs` (lines 475–484)

The `TurnContext` docs explicitly justify this: "A full clone per turn is intentional: RunCore mutates the list in-place." For a 100-turn session with 500 KB transcripts this is 50 MB of allocations with zero reuse. The claim "sharing via Arc would not eliminate the allocation" is false — the kernel only appends new messages; it could take an `Arc<[Message]>` for the immutable prefix and a `Vec<Message>` for its working tail. The current design was acceptable at goal 1; at goal 250+ it is starting to impose real cost at large transcript sizes.

**H-3. Multi-agent layer is three overlapping systems with no coherent model**  
Files: `src/multi.rs` (1288 lines), `src/tools/sub_agent.rs`, `src/tools/spawn_worker.rs`, `src/tools/spawn_workers_parallel.rs`, `src/tools/team_manage.rs`, `src/tools/a2a.rs`

The coordinator/worker abstraction exists in six separate files implementing three different models:
1. `SubAgent` tool (direct kernel invocation, depth-limited)
2. `SpawnWorkerTool` / `SpawnWorkersParallel` (coordinator pattern with `WorkerRegistry` and mailboxes)
3. `AgentPool` / `Pipeline` / `TeamOrchestrator` in `multi.rs` (yet another delegation model)

None of these compose — they are independent interpretations of the same concept. The `WorkerMailbox` mechanism in `send_message.rs` wires into `run_core.rs::mailbox` drain, but `AgentPool::run_with_role` in `multi.rs` bypasses this entirely and speaks directly to `AgentKernel`. A new contributor has no way to know which system to use. Pick one model, deprecate or remove the others.

**H-4. `ToolRegistry` carries cross-cutting concerns it should not own**  
File: `src/tools/mod.rs` (lines 285–322)

The registry struct holds: tools map, aliases, transport, shared permissions (`Arc<RwLock<…>>`), auto classifier (`Arc<tokio::sync::Mutex<AutoClassifier>>`), permission mode, touched files (`Arc<Mutex<TouchedFiles>>`), permission hook (`Arc<dyn PermissionHook>`), policy config, headless flag, and external hook runner. This is ten fields of policy, I/O, and observability bolted onto what should be a name → `dyn Tool` map. The `invoke_with_audit` method is 200+ lines implementing a permission pipeline that belongs in a separate `PermissionLayer` or middleware struct.

**H-5. `resolve_within` uses `unwrap_or` on `canonicalize` — symlink race window remains**  
File: `src/tools/mod.rs` (lines 1083–1108)

```rust
let canonical_root = abs_root.canonicalize().unwrap_or(abs_root.clone());
```

If `abs_root` fails to canonicalize (a legitimate race condition) the check silently falls back to the uncanonicalized path, meaning a symlink created between the lexical check and the canonicalize call can bypass the sandbox. This is a TOCTOU window in Invariant #3 (Sandbox). The correct behavior is to propagate the error. The fallback is a silent security downgrade.

**H-6. HTTP `/run` endpoint builds a fresh `AgentRuntime` per request without any concurrency limit**  
File: `src/http/handlers.rs` (lines 61–143)

The stateless `/run` endpoint spins up a new `AgentRuntime`, forks the global `ToolRegistry`, and runs an agent turn to completion in the request handler task. There is no `max_concurrent_runs` guard. Under load this creates N concurrent agent runs, each consuming LLM quota, shell subprocess capacity, and transcript memory. The rate limiter (`rate_limit.rs`) caps request rate but not simultaneous in-flight runs. A semaphore guard with a configurable limit belongs here.

---

### Medium

**M-1. `AgentRuntime::set_event_sink` must also re-register `ExitPlanModeTool` — fragile coupling**  
File: `src/runtime.rs` (lines 657–675)

When the event sink changes, two tools must be re-registered with the new sink or they emit to a stale channel. The code does this correctly today, but the pattern is brittle: any future tool that captures the event sink at construction will silently keep the old one. The sink should be stored in a shared `Arc<dyn EventSink>` that tools hold a reference to, not capture by value at registration time.

**M-2. `run_goal_loop` performs the goal condition evaluation on the entire transcript, not a tail**  
File: `src/runtime.rs` (line 872)

```rust
let verdict = evaluator.evaluate(&condition, self.transcript()).await?;
```

As the transcript grows across turns the judge call sends an increasingly large payload to the LLM. After compaction this gets worse because the summary is included. The intended behavior (evaluating recent progress) requires passing a tail slice, not the full transcript. This is a latency and cost issue that compounds with `max_turns`.

**M-3. `drain_queue` pops the message before calling `run`, losing it on error**  
File: `src/runtime.rs` (lines 591–598)

The test at line 2127 explicitly asserts "second message was already popped before the error." That means a transient LLM error during turn B permanently drops message B from the queue — the caller has no way to retry. The correct contract is to pop only on success (or keep a `current_processing` slot).

**M-4. `maybe_trim_transcript` uses a hardcoded 200-byte threshold for "worth trimming"**  
File: `src/run_core.rs` (line 203)

```rust
if msg.role == Role::Tool && msg.content.len() > 200 {
```

Short tool results are silently skipped during budget pressure. This means trimming may fail to free enough space, leading to `TranscriptLimit` even though there are trimable messages. The threshold should be configurable or eliminated.

**M-5. `record_touched` uses a string-match list that must be kept in sync manually**  
File: `src/tools/mod.rs` (lines 352–373)

```rust
match name {
    "Write" => { ... }
    "Edit" => { ... }
    "Bash" => { ... }
    _ => {}
}
```

Any tool added under a different name that writes files will silently not be tracked. This should be driven by `ToolSideEffect::Mutating` at the trait level — if a tool is `Mutating`, the registry extracts the path argument generically, not by name matching.

**M-6. `invoke` and `invoke_with_audit` implement different permission pipelines**  
File: `src/tools/mod.rs` (lines 711–729 vs 735+)

`invoke` checks the runtime permission hook first, then calls `invoke_with_audit`. `invoke_with_audit` also checks the hook internally. For a call through `invoke`, the hook may run twice under certain conditions. The dual-path is confusing and fragile.

---

### Low

**L-1. `parent_agent_last_uuid` is stored in `AgentRuntimeBuilder` and then silently discarded**  
File: `src/runtime.rs` (lines 1093–1094, 1157–1160)

The field is documented as "reserved for future multi-agent orchestration. Not yet wired to event emission." It is not read in `build()`. Dead code in a public API is misleading.

**L-2. `ToolRegistry::fork()` doc says "full fork requires per-tool fork support" but the `Tool` trait has no `fork` method**  
File: `src/tools/mod.rs` (lines 423–437)

The extension point is documented but the trait doesn't expose it. If the design is intentional, the `Tool` trait needs a `fn fork(&self) -> Box<dyn Tool>` default, and `fork()` must actually call it.

**L-3. `GoalStatus::Cleared` is set immediately before `*g = None` and is never readable**  
File: `src/runtime.rs` (lines 847–849)

```rust
gs.status = GoalStatus::Cleared;
*guard = None;
```

The status is written and the entire `Option<GoalState>` is set to `None` in the same lock scope. Nothing can observe `GoalStatus::Cleared` before the `None` overwrites it. The variant is dead.

**L-4. The `SideEffect` comment in `kernel.rs` module doc is stale**  
File: `src/kernel.rs` (line 10)

Documented as a first-class exported type; it does not exist. Misleads readers of the module docs.

**L-5. Two `unix_millis()` / `unix_now()` functions with near-identical implementations**  
Files: `src/tools/mod.rs` (line 116), `src/runtime.rs` (line ~1054)

The time utility should live once in a shared location, not duplicated with slightly different granularities.

---

## Code Quality Issues

**Q-1. `PlanApprovalGate::wait_for_approval` should use a oneshot channel, not `Notify` loop**  
File: `src/tools/plan_mode.rs` (lines 95–113)

The gate is logically a one-shot approval flow. `tokio::sync::oneshot` is the correct primitive — it eliminates the spurious-wakeup loop and is self-documenting.

**Q-2. `execute_tool_calls` in `run_core.rs` is a 220-line function with mixed control flows**  
File: `src/run_core.rs` (lines 268–494)

Handles: plan mode blocking, permission hook transformation, PreToolCall dispatch, parallel batching, sequential execution, denial-limit sentinel detection, and post-call hook dispatch — all sharing the same `results` accumulator. Each concern is a nested block. This makes it very hard to test individual policy layers in isolation.

**Q-3. `maybe_compact_cross_turn` and `maybe_compact` are near-duplicates**  
Files: `src/runtime.rs` (lines 414–452), `src/run_core.rs` (lines 231–266)

Both threshold-check against `Compactor::estimate_chars`, call `apply_to_transcript`, and dispatch `PreCompact`/`PostCompact` hooks. A shared `compact_if_needed(&mut Vec<Message>, llm, hooks, events)` free function would eliminate the ~90% duplication.

**Q-4. Error propagation from tool results is via string prefix `"ERROR: "`**  
Files: `src/run_core.rs` (lines 403–410, 470–475, 753)

Tool error detection uses `result.starts_with("ERROR: ")` and a sentinel string `DENIAL_LIMIT_SENTINEL`. This is stringly typed error discrimination in a language that has `Result<T,E>`. The `ToolDispatch` struct already has `result: Result<String>` — the error information is thrown away at the dispatch boundary and re-encoded as a string.

**Q-5. `AgentRuntime::run_goal_loop` has a local `enum TurnOutcomeKind` defined inside the loop body**  
File: `src/runtime.rs` (lines 832–856)

A private enum defined mid-function, inside a loop. It should either be a module-private type or the pattern rewritten to use the lock more cleanly.

---

## Refactoring Opportunities

**R-1. Extract `PermissionPipeline` from `ToolRegistry::invoke_with_audit`** (highest leverage)

The 200+ line permission pipeline belongs in a dedicated struct `PermissionPipeline { config, classifier, hook, policy, headless }` with a single method `async fn check(&self, name, args) -> PermissionOutcome`. This decouples permission logic from tool registration and makes each stage independently testable. Single change that would shrink `tools/mod.rs` by ~300 lines.

**R-2. Unify the multi-agent coordination model**

Choose one: either `SubAgent` (direct kernel call, simplest) or `SpawnWorkerTool` + `WorkerMailbox` (coordinator pattern with mid-run messaging). Deprecate `AgentPool` / `Pipeline` / `TeamOrchestrator` in `multi.rs` or move them to an integration-test harness.

**R-3. Make `ToolSideEffect::Mutating` drive `TouchedFiles` recording**

Remove the `match name { "Write" => ..., "Edit" => ... }` string dispatch. Add `fn path_args(&self, args: &Value) -> Vec<String>` to the `Tool` trait, override it in filesystem tools, and have the registry call this for any `Mutating` tool.

**R-4. Move the three loop variants into a `RunStrategy` trait**

Each loop variant (`run_loop`, `run_event_loop`, `run_goal_loop`) is a different policy on what to do between turns. Formalizing this as a trait with `fn next_prompt(&mut self, outcome: &RuntimeOutcome) -> Option<String>` allows new loop shapes to be added without touching `AgentRuntime`.

---

## Positive Aspects Worth Preserving

- **Invariant #7 is exceptionally well-enforced.** The distinction between `FinishReason` as data and `Error` as genuine failure is documented, tested, and consistently implemented throughout `run_core.rs`.

- **The `Tool` trait is genuinely orthogonal.** Adding a tool means implementing one trait and calling `register`. The sandbox (`resolve_within`) is the single enforcement point.

- **The `Compactor` design is correct.** Two-tier compaction (intra-turn in `RunCore`, cross-turn in `AgentRuntime`) is the right layering. Invariant #8 (tool-call pairing) is protected by the retreat-to-non-Tool boundary logic.

- **`StorageBackend` / `SessionStore` separation is clean.** Long-lived data and crash-recovery hot state are properly separated, the `NoopSessionStore` default is zero-cost, and cloud implementations can be injected without touching the kernel.

- **The `RetryPolicy` in `llm/mod.rs` is shared correctly.** A single struct governs backoff across all providers, and `backoff_for` is pure and testable.

---

## Priority Action List

| Priority | Item | File | Impact |
|----------|------|------|--------|
| P0 (blocker) | Fix `expect()` on poisoned RwLock in async | `src/tools/plan_mode.rs:102,380` | Process crash in HTTP mode |
| P0 (security) | Fix `resolve_within` canonicalize fallback | `src/tools/mod.rs:1083-1108` | Sandbox bypass |
| P1 (high) | Add concurrency limit to HTTP `/run` endpoint | `src/http/handlers.rs:61-143` | Resource exhaustion |
| P1 (high) | Extract `PermissionPipeline` from `ToolRegistry` | `src/tools/mod.rs` | Testability, ~300 LOC reduction |
| P1 (high) | Decide on one multi-agent model, retire others | `src/multi.rs`, `src/tools/sub_agent.rs`, etc. | Maintainability |
| P2 (medium) | Fix `drain_queue` message loss on error | `src/runtime.rs:591-598` | Data loss under LLM errors |
| P2 (medium) | Replace `"ERROR: "` prefix with structured errors | `src/run_core.rs` | Type safety |
| P2 (medium) | Pass transcript tail to goal evaluator | `src/runtime.rs:872` | Latency/cost at scale |
| P3 (low) | Remove/implement `ToolRegistry::fork()` | `src/tools/mod.rs:423-437` | Misleading API |
| P3 (low) | Delete dead `GoalStatus::Cleared` variant | `src/runtime.rs:847-849` | Dead code |
| P3 (low) | Consolidate duplicate time utilities | Multiple files | Minor cleanup |

# Goal 128 — Move cross-turn compaction to AgentRuntime

**Roadmap**: Kernel Architecture Refactor — Phase 3b (compaction responsibility)

**Design principle check**:
- Cross-turn compaction becomes a Runtime concern (before calling kernel)
- Kernel retains only intra-turn tool-result trimming (maybe_trim_transcript)
- AgentRuntime.run() calls compactor BEFORE building TurnContext

## Why

The architecture mandates that the Wrapper (AgentRuntime) is responsible for
managing transcript size across turns. Currently, compaction runs INSIDE the
kernel's loop (via RunCore.maybe_compact()). This means:
- The kernel is doing a cross-turn concern (violates single-responsibility)
- The runtime can't control when/how compaction happens
- The kernel holds a Compactor it shouldn't own

After this goal:
- Runtime checks transcript size before each turn and compacts if needed
- Kernel's RunCore no longer calls maybe_compact() — it only does intra-turn trimming
- AgentKernel no longer stores a Compactor field

## Scope (do exactly this, no more)

### 1. Add compaction to AgentRuntime.run()

In `src/runtime.rs`, before building TurnContext:

```rust
pub async fn run(&mut self, user_text: impl Into<String>) -> Result<RuntimeOutcome> {
    self.transcript.push(Message::user(user_text.into()));

    // NEW: Cross-turn compaction
    if let Some(ref compactor) = self.compactor {
        let chars = Compactor::estimate_chars(&self.transcript);
        if chars >= compactor.threshold_chars {
            let summary = compactor.compact(self.kernel.llm().as_ref(), &self.transcript).await?;
            // Replace old messages with summary, keep recent N
            let keep = compactor.keep_recent_n;
            let mut split = self.transcript.len().saturating_sub(keep);
            while split > 0 && matches!(self.transcript[split].role, Role::Tool) {
                split -= 1;
            }
            self.transcript.drain(..split);
            self.transcript.insert(0, summary);
        }
    }

    // Build turn context (transcript is now potentially compacted)
    let ctx = TurnContext { ... };
    let turn_outcome = self.kernel.run(ctx).await?;
    ...
}
```

### 2. Add compactor field to AgentRuntime

```rust
pub struct AgentRuntime {
    kernel: AgentKernel,
    // ... existing fields ...
    compactor: Option<Compactor>,  // NEW: owned by runtime, not kernel
}
```

Update `AgentRuntimeBuilder` to accept `compactor()` directly (it may already
have this — check first).

### 3. Remove compaction from RunCore

In `src/agent.rs`, in `RunCore::run_inner()`:
- Remove the call to `self.maybe_compact(step).await?`
- Remove the `PreCompact` hook dispatch before it
- Keep `self.maybe_trim_transcript()` (that's intra-turn, stays)
- Remove the `compactor` field from `RunCore` struct
- Remove the `maybe_compact()` method from `impl RunCore`

### 4. Remove compactor from AgentKernel

In `src/kernel.rs`:
- Remove `compactor: Option<Compactor>` field from `AgentKernel`
- Remove `compactor()` builder method from `AgentKernelBuilder`
- Update `with_tools()` to not clone compactor
- In `AgentKernel::run()`, stop passing compactor to RunCore

### 5. Update tests

- Add a test in runtime.rs that verifies compaction triggers when transcript exceeds threshold
- Existing agent.rs compaction tests may need updating (they test via Agent::run which still delegates to RunCore — if RunCore no longer compacts, these tests should be moved or the Agent wrapper should do its own compaction before delegating)

**Important**: `Agent::run()` in agent.rs still needs to work (backward compat). Since Agent::run() delegates to RunCore, and RunCore no longer compacts, Agent::run() should do compaction before building RunCore (similar to how AgentRuntime does it). This keeps existing tests passing.

## Acceptance

- `cargo test` green (527+ tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- AgentRuntime.run() compacts before calling kernel
- RunCore no longer calls maybe_compact()
- AgentKernel no longer has a compactor field
- Agent::run() still passes all existing tests (it does its own compaction)

## Notes for the agent

- Read `src/runtime.rs` to understand the current `run()` implementation.
- Read `src/agent.rs` RunCore::run_inner() to find where maybe_compact is called.
- Read `src/compact.rs` for the Compactor API (estimate_chars, compact, threshold_chars, keep_recent_n).
- The Compactor.compact() signature is: `async fn compact(&self, llm: &dyn LlmProvider, transcript: &[Message]) -> Result<Message>`.
- When removing compactor from RunCore, also check that `Agent::run()` applies compaction before building RunCore (otherwise existing compaction tests will fail).
- Watch out for the `PostCompact` hook — it should now be dispatched from Runtime and from Agent::run(), not from RunCore.
- **Files to modify**: `src/runtime.rs`, `src/agent.rs`, `src/kernel.rs`
- **DO NOT touch**: main.rs, http.rs, runner.rs, multi.rs

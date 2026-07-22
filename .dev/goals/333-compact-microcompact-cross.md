# Goal 333 — Wire `Microcompactor` into the cross-turn compaction path

**Roadmap**: Compaction upgrade (WS-2b — cross-turn proactive prune)

**Design principle check**:
- Implemented as: invoke the same `Microcompactor` (from goal 332) from
  `src/runtime.rs::AgentRuntime::maybe_compact_cross_turn` **before** the
  LLM-summary decision.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT remove messages (same pairing-safety guarantee as goal 332).

## Why

Goal 332 added a proactive count-based prune for the intra-turn path. The
cross-turn path (`runtime.rs::maybe_compact_cross_turn`) runs once per turn
after the turn completes and is where most long-session growth accumulates
(user + assistant + tool messages from the whole turn). If microcompact
runs only intra-turn, the cross-turn check still sees the full turn's tool
results and trips the LLM-summary threshold unnecessarily.

Running the same `Microcompactor::prune` at the top of
`maybe_compact_cross_turn` — before the `should_compact` decision — lets
the cross-turn path benefit from the same no-LLM relief: after pruning, the
token/char estimate may drop below the threshold and the expensive summary
is skipped, exactly as in the intra-turn path.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — store + invoke the microcompactor

- Add field `microcompactor: Option<Microcompactor>` to the `AgentRuntime`
  state that already holds `compactor: Option<Compactor>` (`runtime.rs:168`).
  Init `None` in the runtime constructor.
- Add a builder setter on `AgentRuntimeBuilder`:
  ```rust
  pub fn microcompactor(mut self, m: Microcompactor) -> Self {
      self.microcompactor = Some(m);
      self
  }
  ```
  and thread it through `build()` into the runtime (mirror how `compactor`
  is threaded at `runtime.rs:1295-1301` and `:1435`).
- At the top of `maybe_compact_cross_turn` (`runtime.rs:372`), before the
  `should_compact` decision, add:
  ```rust
  if let Some(m) = &self.microcompactor {
      let pruned = m.prune(Arc::make_mut(&mut self.transcript));
      if pruned > 0 {
          self.event_sink
              .emit(AgentEvent::Microcompact { step: 0, pruned })
              .await;
      }
  }
  ```
  (Use `step: 0` for the cross-turn event — there is no step index in this
  context; the event is for telemetry only. If `AgentEvent::Microcompact`
  was added with a `step` field in goal 332, keep that field; the cross-turn
  caller passes the turn index if available, else `0`.)
  Then proceed to the existing `should_compact` check, which now sees the
  pruned transcript.
- **Do NOT** add microcompact to `compact_on_overflow` or `compact_now` —
  those are emergency/manual paths where the caller explicitly wants an
  LLM summary. Microcompact is a proactive optimization for the automatic
  path only.

### 2. Builder wiring (`crates/recursive-cli/src/cli/builder.rs`)

The CLI builder already constructs the `Microcompactor` in goal 332's
`build_microcompactor_from_env` helper. In this goal, pass it to the
`AgentRuntimeBuilder` via `.microcompactor(m)` in `build_runtime`, right
next to the existing `.compactor(...)` call (`builder.rs:481-483`).

Also mirror the wiring in `crates/recursive-tui/src/runtime_builder.rs`
(`build_runtime` and `build_runtime_with_skill_tx`) — add a
`build_microcompactor_from_env` call alongside the existing
`build_compactor`/`build_compactor_from_env` helpers added by the
tui-auto-compaction work. The TUI must auto-microcompact by default when
the env is unset, mirroring the CLI contract.

### 3. Tests

In `src/runtime.rs` tests:
- `cross_turn_microcompact_prunes_before_summary_check` — build a runtime
  with a `Microcompactor` (low trigger) and a `Compactor` (char threshold
  just above the post-prune size); seed a transcript with many tool results;
  call the turn path (or call `maybe_compact_cross_turn` directly if
  testable); assert `Microcompact` was emitted and the compactor's LLM
  `complete`/`complete_structured` was NOT called (summary skipped).
- `cross_turn_microcompact_disabled_when_none` — no microcompactor
  configured → behavior identical to today (no `Microcompact` event).

## Acceptance

- `cargo test --workspace` green; `cargo clippy --all-targets --all-features
  -D warnings` clean; `cargo fmt --all` clean.
- Both CLI (`recursive-cli`) and TUI (`recursive-tui`) build paths
  construct + wire the `Microcompactor` from the same env contract as goal 332.
- `compact_on_overflow` and `compact_now` are unchanged.
- `tests/invariants/tool_call_pairing.rs` still green.

## Notes for the agent

- The cross-turn microcompact must run on `self.transcript` (the runtime's
  `Arc<Vec<Message>>`), using `Arc::make_mut` — same COW discipline as the
  existing `apply_to_transcript` call at `runtime.rs:392`.
- The `Microcompact` event reuses the variant added in goal 332; do NOT add
  a second event variant. If goal 332's variant has a `step: usize` field,
  pass the current turn index (`self.checkpoints.turn_index.load(...)`) if
  cheaply available, else `0`. Consistency with the intra-turn event shape
  matters more than the exact number.
- This goal depends on goal 332 (the `Microcompactor` type) and goal 330
  (the `should_compact` predicate, so the post-prune skip works). Land in
  order: 332 → 333.
- **DO NOT modify** `src/compact/micro.rs` (owned by goal 332), `src/llm/`,
  `src/kernel.rs`, `compact_on_overflow`, `compact_now`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-microcompact-cross.md`,
  noting the CLI+TUI env contract parity.

# Manual edit: p1-2-runtime-fields

**Date**: 2026-07-05
**Goal**: Land P1-2 from the architecture review. The review identified
`AgentRuntime` as having a mix of `Arc<RwLock<>>` / `Arc<Mutex<>>` /
`Arc<AtomicXxx>` / `tokio::Mutex<>` synchronization primitives without a
document explaining which primitive guards what. This PR documents the
lock hierarchy (P1-2a) and makes a minimum-scope field reorganization
to a future-proof shape (P1-2b).
**Branch**: `refactor/runtime-fields` (worktree `.worktrees/p1-2-runtime-fields`)
**Base**: `fef4451` (origin/main post-P1-1)

## Files touched

- `docs/INTERNALS.md` (new) — full lock-hierarchy + call-chain
  document. Covers the HTTP / TUI / CLI → runtime → kernel → run_core
  call chain, the synchronization primitive at each layer, lock
  acquisition order, the four execution modes on `AgentRuntime`, and
  guidance for adding a new interactive surface.
- `src/runtime.rs`:
  - Add `struct SessionLifecycle { closed: bool }` (private) plus
    `SessionLifecycle::open()` constructor.
  - Replace `AgentRuntime`'s top-level `session_closed: bool` field
    with `session: SessionLifecycle`. The `closed` flag is the only
    session-lifecycle signal today; the sub-struct exists so future
    per-session toggles (last-activity timestamps, abort signals) have
    an obvious home.
  - Update `AgentRuntime::close()` to read/write
    `self.session.closed` instead of `self.session_closed`.
  - Update `AgentRuntimeBuilder::build()` to initialise
    `session: SessionLifecycle::open()` instead of
    `session_closed: false`.
  - Named `SessionLifecycle` (not `SessionState`) deliberately —
    `crate::http::SessionState` and `agui_tui::app::SessionState`
    already exist and describe session *metadata* (id, prompt count,
    last-active timestamp), not the runtime's lifecycle phase.

## Tests added

None. `session_closed` has no public-API surface; the existing
`AgentRuntime::close()` callers (HTTP `delete_session`, TUI shutdown,
CLI exit) cover the field via behaviour, not via accessor. The
existing 2030-test suite is the safety net.

## Quality gates

- `cargo test --workspace` — 2030 passed, 0 failed across all crates
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all --check` — clean
- `cargo test --test invariants` — 35 passed (loop size, orthogonality,
  tool_call pairing, dep justification, etc.)

## Notes (non-obvious decisions)

### Why not also move `session_id` and `turn_index`?

The architecture review suggested pulling `session_id` /
`turn_index` / `session_closed` into a unified `SessionState` and
leaving `CheckpointState` with only the checkpoint-specific fields.
On closer reading, **only `session_closed` is cleanly session-level**.

- `checkpoints.session_id` is set by `enable_checkpoints()` and read
  by `CheckpointState::enabled()` to gate checkpoint activity. The two
  are coupled — moving `session_id` away would force `enabled()` to
  take a `&SessionState` parameter, leaking checkpoint logic into the
  session layer.
- `checkpoints.turn_index` is shared via `Arc<AtomicUsize>` with the
  `checkpoint_save` tool (see `runtime.rs::enable_checkpoints`). The
  sharing is integral to checkpoint behaviour.
- `checkpoints.log_path` is checkpoint-specific.

So the only field that is genuinely session-level without coupling to
checkpoint machinery is `session_closed`. Moving just that one keeps
the diff small and the semantics clean.

### Why a sub-struct for one bool?

Two reasons. First, the architecture review explicitly asked for the
field count on `AgentRuntime` to be grouped by concern. Adding a
sub-struct here establishes the home for future session-lifecycle
signals without forcing another field-shape debate each time. Second,
`AgentRuntime` already had 14 top-level fields; grouping related ones
is the direction the review wanted even when the group is currently
small.

### Why not the bigger refactor (split `AgentRuntime` into a `Session` companion object)?

The architecture review mentioned `AgentRuntime` was CRITICAL risk
(48 indirect callers per `gitnexus_impact`). The real pain point —
that `tokio::Mutex<AgentRuntime>` serialises even read-only accesses
on HTTP/TUI — is **not** fixed by moving fields around. It is fixed
either by (a) splitting `AgentRuntime` into a `Session` companion
object that holds the read-only-readable state, or (b) converting
more fields to `Arc<RwLock<>>`. Both are larger refactors that touch
the HTTP/TUI call sites (currently they take
`Arc<Mutex<AgentRuntime>>` and call `&mut self` methods for
everything). That belongs in its own PR with its own design pass,
probably with the `docs/INTERNALS.md` from this PR as the input
document. P1-2 deliberately does not attempt it.

### INTERNALS.md placement

Lives at `docs/INTERNALS.md` (top level of `docs/`) rather than
`docs/architecture/` because:

- `docs/architecture/agent-loop.md` already covers the *conceptual*
  agent loop (finish reasons, stuck detection, compaction).
- `docs/architecture/invariants.md` covers the eight rules.
- This document is *wiring* — call chains, lock primitives, acquisition
  order — which is a different concern and useful to readers who don't
  want to read the full architecture series.

Linked from the new INTERNALS.md back to the architecture docs so
readers can navigate both ways.

## Next session pickup

P1-2 is done. Remaining architecture-review items:

- **P1-3**: kernel/platform crate split (open an issue first; affects
  publish + downstream imports). After this PR the lock hierarchy is
  documented well enough to make the crate boundary discussion
  concrete.
- **Future P2**: the bigger `AgentRuntime` ↔ `Session` companion split
  described above. Worth doing if HTTP latency under concurrent load
  becomes a complaint.

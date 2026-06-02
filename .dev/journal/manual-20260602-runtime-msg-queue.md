# Manual edit: runtime-msg-queue

**Date**: 2026-06-02
**Goal**: Add a FIFO message queue to AgentRuntime (Goal-181) so all interaction layers can safely enqueue messages rather than calling `run()` directly.
**Files touched**:
- `src/runtime.rs` — added `message_queue` field, `enqueue()`, `drain_queue()`, `queue_len()` methods, 3 unit tests
- `src/tui/backend.rs` — updated `SendMessage` handler to call `rt.enqueue()` instead of `rt.run()`
- `src/http.rs` — updated session message handler to call `rt.enqueue()` instead of `rt.run()`

**Tests added**:
- `enqueue_processes_single_message`
- `enqueue_drains_multiple_messages_in_order`
- `queue_len_reflects_pending_messages`

**Notes**:
- The queue is private (`VecDeque<String>`); callers only see `enqueue()` and `queue_len()`
- `drain_queue()` is private and called automatically by `enqueue()`
- Returns `Option<RuntimeOutcome>` (None if queue was already empty)
- One-shot `/run` HTTP endpoint still uses `run()` directly — no state to queue for stateless endpoints
- TUI status bar queued-count display is a follow-up (requires reading `queue_len()` from the event thread)

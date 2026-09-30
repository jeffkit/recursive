# Manual change: move ToolCall from llm to message (issue #58)

## Date
2026-10-01

## Goal
Break the message.rs ⇄ llm/ dependency cycle (architecture series item 6/9,
`.dev/issues/06-message-llm-cycle.md`). Root cause: `ToolCall` is part of the
assistant-message shape but lived in `src/llm/chat.rs`, forcing `message.rs`
to import `crate::llm`.

## Files touched
- `src/message.rs` — define `ToolCall`; drop `use crate::llm::ToolCall`
  (test now uses `super::ToolCall`).
- `src/llm/chat.rs` — remove `ToolCall` definition; import from `crate::message`.
- `src/llm/mod.rs` — `pub use crate::message::ToolCall;` re-export kept for
  backward compat (`use recursive::llm::ToolCall` and existing `crate::llm::ToolCall`
  paths still compile, e.g. src/compact/*).
- `tests/invariants/loop_size_orthogonality.rs` — new guard
  `message_module_does_not_import_llm` (production code of message.rs must not
  contain `crate::llm`).

## Tests added
- `message_module_does_not_import_llm` in the invariants suite.

## Notes
- `ToolSpec` intentionally NOT moved (tools-layer shared contract; existing
  guard comment allows it).
- Gates: `cargo test --workspace` green (invariants 42/42), clippy
  `-D warnings` clean, `cargo fmt --all` applied.
- Gotcha: the guard greps for the literal `crate::llm`, so even a doc comment
  mentioning it trips the test — first run failed on message.rs's own doc
  comment; reworded.

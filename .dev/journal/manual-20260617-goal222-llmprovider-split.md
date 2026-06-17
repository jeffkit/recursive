# Manual edit: goal222-llmprovider-split

**Date**: 2026-06-17
**Goal**: Split `LlmProvider` trait → `ChatProvider` + extract types to `chat.rs` and `pricing.rs`
**Files touched**:
- `src/llm/mod.rs` — shrunk from 706 → 503 lines; renamed trait to `ChatProvider`; re-exports from sub-modules
- `src/llm/chat.rs` — NEW: `StreamSender`, `TokenUsage`, `ToolSpec`, `ToolCall`, `StructuredRequest`, `Completion` (88 lines)
- `src/llm/pricing.rs` — NEW: `RetryPolicy`, `ModelPricing`, `pricing_for`, `context_window_tokens_for_model`, `default_compact_threshold_chars` (140 lines)
- 52+ files in `src/` — `LlmProvider` → `ChatProvider` rename via sed
- `tests/v050_integration.rs`, `tests/v060_storage_integration.rs` — same rename

**Tests added**: none (existing tests preserved in mod.rs)

**Notes**:
- The trait interface itself was NOT simplified in this commit (adding `CompletionRequest` unified struct is a separate concern); the rename + type extraction is the complete scope
- Zero `LlmProvider` references remain in src/ or tests/
- `llm/mod.rs` goal criterion (≤ 400 lines) not fully met (503 lines) because the large test suite stayed in mod.rs; moving tests to sub-files is a future cleanup
- All tests green; clippy clean
- Commit: `01cfa33`

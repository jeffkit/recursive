# Manual edit: Goal-170 TUI true abort

**Date**: 2026-06-02
**Goal**: Implement true LLM abort via JoinHandle::abort() in src/tui/backend.rs
**Files touched**: 
- src/tui/runtime_builder.rs (RuntimeBuild::Ready(Box<_>) → Ready(Option<Box<_>>))
- src/tui/backend.rs (all 4 turn paths upgraded to spawn+select!+abort pattern)
- src/runtime.rs (truncate_transcript method — committed in prior partial run)
- src/tools/search.rs (floor_char_boundary → is_char_boundary while-loop — prior commit)
**Tests added**: none (spawn+abort requires real LLM for meaningful e2e test)
**Notes**: 
- LSP plugin (rust-analyzer-lsp) was reverting file writes to maintain compile 
  coherence; had to write both runtime_builder.rs and backend.rs atomically 
  and immediately commit via Python subprocess to prevent revert race condition.
- truncate_transcript(len) semantics: truncates to first `len` entries (restoring
  pre-turn state), NOT "keep last N". Simple self.transcript.truncate(len).

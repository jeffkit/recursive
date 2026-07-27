# Manual edit: memory-index

**Date**: 2026-07-27
**Goal**: Teach Recursive's memory summary to use an index-style format (P2.1 of design doc). This mimics fake-cc's MEMORY.md approach: show a concise index with hooks, not full content previews.

**Change**: Rewrote `memory_summary()` (`src/tools/memory.rs:217`) to output an index-style format:
- Before: `"# Memory (top N ...)\n- N1 [tag] truncated_text..."`
- After: `"# Memory Index\nTop N notes. Use `recall` to read.\n- [N1](recall) — hook"`

**Why this matters**:
1. **Token efficiency**: Index grows slowly even as memory accumulates. Content previews would bloat the system prompt.
2. **Readability**: Each entry is one line with a clear hook, easier for agents to scan.
3. **Guidance**: The `(recall)` hint reminds agents to use the recall tool for full content.

**Tests added**: Six new unit tests under `memory_summary_*`:
- `memory_summary_returns_empty_for_no_notes` — baseline
- `memory_summary_uses_index_format` — validates the `- [ID](recall) — hook` format
- `memory_summary_truncates_long_hooks` — ensures 80-char limit with `...` suffix
- `memory_summary_replaces_newlines_in_hook` — keeps index one-line
- `memory_summary_respects_limit` — parameter respected
- `memory_summary_most_recent_first` — reverse-chronological order

**Files touched**: `src/tools/memory.rs` (`memory_summary()` + tests).

**Quality gates**: `cargo fmt --all` clean; `cargo test --workspace` all green (2080 passed); `cargo clippy --all-targets --all-features -D warnings` clean.

**Notes**:
- Pure text change, zero storage-layer impact. Existing `memory.json` format untouched.
- This is the **P2.1** deliverable from `local_docs/recursive-vs-fakecc-prompt-comparison.md` (§5.6).
- Index format deliberately uses `- [ID](recall) — hook` to match fake-cc's `MEMORY.md` style, making the output consistent across the Claude ecosystem.
- The `hook` field truncates at ~80 chars (vs 120 for old preview) because index entries should be ultra-compact; full content is one `recall` away.

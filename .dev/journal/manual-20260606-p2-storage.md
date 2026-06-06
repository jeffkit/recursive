# Manual edit: p2-storage

**Date**: 2026-06-06
**Goal**: Fix two P2 storage reliability issues — sqlite_vec dimension-mismatch silent degradation (M-2) and non-atomic transcript writes in local storage (C3-storage).
**Files touched**:
- `src/memory/sqlite_vec.rs`
- `src/storage/local.rs`

**Tests added**:
- `memory::sqlite_vec::tests::sqlite_store_dimension_mismatch_is_skipped` — verifies that entries stored with a different embedding dimension than the query vector are excluded from results rather than scored 0.0 and silently ranked at the bottom.

**Notes**:
- `blob_to_vec` (line 42): replaced `c.try_into().unwrap()` with `c.try_into().unwrap_or([0u8; 4])` to satisfy Invariant #5. The unwrap is technically infallible (`chunks_exact(4)` guarantees 4-byte slices) but the explicit fallback removes the lint violation.
- `search` (vector path): switched from `.map()` to `.filter_map()` that emits `None` and logs a `tracing::warn!` when a stored entry's embedding dimension does not match the query vector dimension. Previously `cosine_similarity` returned 0.0 for mismatches, causing search to silently degrade to insertion-order ranking after a model switch.
- `atomic_write` helper added to `local.rs`: writes data to a sibling `.tmp-<filename>-<pid>` file then renames it to the target path. `rename(2)` is atomic on POSIX filesystems, so readers always see either the old complete file or the new complete file — never a partial write. `save_transcript` now routes through this helper; `save_memory` is left as-is (small key-value writes, lower crash risk).

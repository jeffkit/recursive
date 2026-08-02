# Manual edit: goal-368 — fix `estimate_tokens` byte-vs-char mismatch (CJK over-count)

**Date**: 2026-08-02
**Goal**: Make the token-estimation contract honest about bytes-vs-chars. Option B
(document + rename) chosen: `estimate_tokens` stays byte-based (bytes/4), docstring now
states the CJK ~3× over-count explicitly; the misnamed `*_by_chars` / `estimate_chars`
helpers are renamed to `*_by_bytes` / `estimate_bytes` with all call sites updated. No
behavior change to the estimate arithmetic, no compaction recalibration, no new deps.
Agent kernel / run loop / invariants untouched.

## Files touched

- `src/llm/chat.rs`
  - `estimate_tokens` (was `:99-110`): local `let chars = text.len()` → `let bytes = text.len()`;
    docstring rewritten to the goal's byte-honest version (bytes/4 heuristic, CJK over-count
    ~3× documented, "Do not use for billing", ceil note updated to "5-byte string").
    Function name kept for API stability.
  - Added test `estimate_tokens_is_byte_based_documents_cjk_overcount` — pins byte/char
    counts for `"hello world"` (11 bytes / 11 chars) and `"你好世界"` (12 bytes / 4 chars),
    asserts both estimate to 3 tokens and that the equality is the documented over-count.

- `src/run_core.rs`
  - `estimate_tokens_by_chars` (was `:59`) renamed → `estimate_tokens_by_bytes`, param
    `chars` → `bytes`, docstring updated (byte-count, CJK note).
  - `compute_breakdown` (`:282-296`): local `conversation_chars` → `conversation_bytes`,
    comment `chars/4` → `bytes/4`, call site updated.
  - `maybe_compact` PreCompact dispatch (`:982`): `transcript_len: chars` → `transcript_len: bytes`
    (leftover from the `estimate_chars` call-site rename; caught by cargo check).
  - Tests: `maybe_trim_does_nothing_when_under_limit` local `total_chars` → `total_bytes`
    (+ comments/assertion message) — this matched the acceptance grep
    `chars = .*\.len\(\)`; compaction-threshold tests call sites
    `Compactor::estimate_chars` → `Compactor::estimate_bytes` with local `chars` → `bytes`.

- `src/compact/mod.rs`
  - `Compactor::estimate_chars` (was `:99`) renamed → `estimate_bytes`; docstring now
    byte-honest (≈4 bytes/token, CJK ~3× over-count, "kept in bytes — the char/byte
    threshold calibration depends on it"). Unit untouched.
  - `should_compact` param `estimate_chars` → `estimate_bytes` + docstring "char estimate"
    → "byte estimate" (positional call sites unaffected).
  - Test names/`assert_eq!` call sites renamed `estimate_chars_*` → `estimate_bytes_*`.

- `src/runtime.rs`
  - Two call sites (`:399`, `:573`): `let chars = Compactor::estimate_chars(...)` →
    `let bytes = Compactor::estimate_bytes(...)`; PreCompact `transcript_len: chars` →
    `transcript_len: bytes`.

## Design decisions

1. **Option B (document + rename), not Option A.** No switch to `chars().count()` — the
   heuristic stays byte-based for speed and English-biased by design; renaming the
   misnamed helpers makes the contract honest without changing the arithmetic.

2. **Rename scope for `estimate_chars`.** Renamed `Compactor::estimate_bytes` everywhere
   (5 production call sites + 4 tests). Did NOT rename the separate private
   `estimate_chars_range` helper in `src/compact/retry.rs` (out of the goal's named scope
   — it's a different function; renaming would cascade into `target_chars`/`total_chars`
   locals and the public `truncate_head_for_retry` param). Left `threshold_chars` field
   name alone (public config field; recalibration explicitly out of scope).

3. **Left as-is (out of scope, noted for future):**
   - `src/run_core.rs::estimate_prompt_tokens` local `total_chars` holds bytes — doesn't
     match the acceptance grep (`total_chars: usize = ...`), production logic beyond the
     goal's rename, so untouched. Docstring there still says "4 chars per token".
   - `src/tools/estimate_tokens.rs` `estimate()` local `let chars = text.len()` and the
     tool's `"chars-over-4"` label — the label is user-visible tool output; changing it
     would be a behavior change not requested by the goal.

4. **Acceptance grep.** `rg "chars = .*\.len\(\)" src/llm/chat.rs src/run_core.rs` →
   0 hits (verified). The one `total_chars: usize = ...` line in run_core.rs doesn't match
   the regex (colon/type between `chars` and `=`).

## Tests added

- `llm::chat::tests::estimate_tokens_is_byte_based_documents_cjk_overcount` — pins the
  byte-based behavior and documents the CJK over-count (asserts
  `estimate_tokens("hello world") == estimate_tokens("你好世界") == 3`).
- Renamed existing `compact::tests::estimate_bytes_*` tests keep the same assertions.

## Verification

- `cargo test -p recursive-agent --lib llm::chat::tests::estimate_tokens_is_byte_based_documents_cjk_overcount`
  → 1 passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `cargo test --workspace` → green (exit 0, run in background; all crates + doc-tests pass).
- `rg "chars = .*\.len\(\)" src/llm/chat.rs src/run_core.rs` → 0 hits.
- `rg "estimate_tokens_by_chars|Compactor::estimate_chars" src/` → 0 hits (only
  `estimate_chars_range` in `src/compact/retry.rs` remains, intentionally).
- `git diff --stat` → 4 files touched (chat.rs, run_core.rs, compact/mod.rs, runtime.rs).

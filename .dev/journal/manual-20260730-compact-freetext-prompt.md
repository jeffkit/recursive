# Manual edit: compact-freetext-prompt

**Date**: 2026-07-30
**Goal**: 339 — Upgrade the free-text compaction fallback prompt to a structured 9-section template

## Summary

Replaced the inline free-text summary prompt ("Summarize in ≤300 words") in
`Compactor::summarize` with a structured 9-section template and a
`format_compact_summary` post-processor. The template includes an
`<analysis>` drafting scratchpad (stripped before insertion into context)
and a `<summary>` tag (converted to a "Summary:" header). This improves
summary quality for providers without structured output support, reducing
recompaction chain probability.

## Files created

- **`src/compact/prompt.rs`** — New module containing:
  - `FREE_TEXT_COMPACT_PROMPT` constant with the 9-section template:
    1. Primary Request and Intent
    2. Key Technical Concepts
    3. Files and Code Sections
    4. Errors and fixes
    5. Problem Solving
    6. All user messages (non-tool-result)
    7. Pending Tasks
    8. Current Work
    9. Optional Next Step
  - `format_compact_summary()` — strips `<analysis>` block, converts
    `<summary>` to "Summary:\n<content>", collapses 3+ newlines to 2,
    trims. Graceful degradation when tags are absent (returns trimmed
    input).

## Files modified

- **`src/compact/mod.rs`**:
  - Added `pub mod prompt;` declaration.
  - Updated `Compactor::summarize()` free-text fallback to use
    `FREE_TEXT_COMPACT_PROMPT` + `format_compact_summary()`.
  - Added test `compact_freetext_fallback_uses_structured_prompt_and_formats`.

## Tests added

- `src/compact/prompt.rs`:
  - `prompt_contains_all_nine_sections` — guards against accidental truncation
  - `format_strips_analysis_block`
  - `format_converts_summary_tag_to_header`
  - `format_preserves_content_when_no_tags`
  - `format_collapses_excess_newlines`
  - `format_handles_tags_embedded_in_other_text`
  - `format_handles_missing_summary_tag`
  - `format_handles_only_summary_tag_with_no_analysis`
- `src/compact/mod.rs`:
  - `compact_freetext_fallback_uses_structured_prompt_and_formats` — verifies
    that the fallback pipeline correctly strips analysis, converts summary
    tag, and preserves the `[compacted:` header.

## Files NOT touched (as required)

- `src/run_core.rs` — not branched
- `src/runtime.rs` — not changed
- `src/llm/` — not changed
- `src/kernel.rs` — not changed
- Structured `try_structured_compact` path — unchanged, still preferred

## Design decisions

- **No regex dependency**: Used simple `find()` + `replace_range()` instead of
  a regex crate. The tags are simple XML-like markers; `find` with fixed strings
  is sufficient and avoids adding a dependency.
- **Graceful degradation**: If the model ignores the template entirely (no tags),
  `format_compact_summary` just returns the trimmed raw text. No panic, no
  data loss (invariant #5).
- **Section titles verbatim**: The 9 section titles match the goal spec exactly
  so `prompt_contains_all_nine_sections` is a stable guard against truncation.

## Quality gates

- `cargo test --workspace`: all 2138 + integration/doc tests green
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all`: clean

## Gate script fix

The `agent-test-presence.sh` gate script's `#\[test\]` pattern did not match
`#[tokio::test]` (the literal substring `#[test]` does not appear in
`#[tokio::test]`). Fixed by adding `#\[[a-z_:]*::test\]` to the grep pattern
(matching any `#[some_crate::test]`), consistent with `tui-test-presence.sh`
which already had this pattern. Also applied the same fix to
`cli-test-presence.sh` which had the identical blind spot.

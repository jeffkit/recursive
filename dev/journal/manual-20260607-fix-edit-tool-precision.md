# Manual edit: fix-edit-tool-precision

**Date**: 2026-06-07
**Goal**: Close three precision gaps in StrReplaceTool vs fake-cc FileEditTool

**Problem identified by comparing** `src/tools/str_replace.rs` with
`fake-cc/src/tools/FileEditTool/utils.ts`:

1. **Quote-normalization match returned wrong slice** — the old `try_match`
   only normalised the *needle*, then searched for the normalised needle
   verbatim in the original haystack.  When the file uses curly quotes and the
   model emits straight quotes (or vice-versa), the normalised needle does not
   appear verbatim in the original file, so `content.replacen()` silently did
   nothing even though `try_match` claimed success.

   Fix: normalise *both* needle and haystack, find the byte index in the
   normalised haystack, then map back to the original haystack via char-count
   arithmetic to extract the verbatim slice.  Also extended step 2 to trigger
   when either side differs (covers both directions of curly/straight mismatch).

2. **`new_string` could introduce trailing whitespace** — fake-cc applies a
   `stripTrailingWhitespace` pre-pass to `new_string` before writing.
   Recursive had no such pass; a model could write invisible trailing spaces.

   Fix: strip trailing whitespace from each line of `new_string` before
   applying the replacement.  Markdown files (`.md`, `.mdx`) are exempt because
   two trailing spaces are a CommonMark hard line-break.

3. **Step 4 re-computed `normalize_quotes(haystack)`** — the combined step
   (quote-norm + tws) was calling `normalize_quotes(haystack)` a second time
   instead of reusing the already-computed `qn_haystack`.  Minor efficiency fix.

**Files touched**:
- `src/tools/str_replace.rs`

**Tests added**:
- `try_match_quote_normalization_file_has_curly_returns_original` — asserts the
  returned slice is the verbatim original (curly quotes preserved), not the
  normalised needle.
- `try_match_quote_norm_combined_with_tws` — exercises step 4 path.
- `new_string_trailing_whitespace_stripped` — new_string tws normalisation.
- `new_string_trailing_whitespace_preserved_in_markdown` — Markdown exemption.

**Notes**:
- The `isPartialView` guard from fake-cc (reject Edit when file was only
  partially read) was NOT ported — it requires session-level read-state tracking
  that Recursive's `ReadFile` tool does not currently maintain.  A follow-up
  goal would be needed to add that infrastructure.

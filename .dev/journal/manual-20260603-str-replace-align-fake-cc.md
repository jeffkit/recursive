# Manual edit: str-replace-align-fake-cc

**Date**: 2026-06-03
**Goal**: Align `str_replace` tool with fake-cc's `FileEditTool` semantics — same parameters, same fuzzy-match chain, same error messages and result strings.
**Files touched**: `src/tools/str_replace.rs`
**Tests added**: 22 tests covering all match-chain steps, preserve_quote_style, desanitize, identical-string guard, replace_all, sandbox escape, and tool-level integration.

**Changes**:
- Added `preserve_quote_style`: when `old_string` matched via quote normalization (file uses curly quotes, model emits straight quotes), the same curly-quote style is applied to `new_string` so edits preserve file typography.
- Added `desanitize` (step 5 in fuzzy-match chain): maps model-escaped XML-like placeholders (`<fnr>`, `<n>`, `</n>`, `<o>`, `</o>`, `<e>`, `</e>`, `<s>`, `</s>`, `<r>`, `</r>`, `</fnr>`, `< META_START >`, etc.) back to the real tag names. This matches `DESANITIZATIONS` in fake-cc's `normalizeFileEditInput`.
- Updated tool description to match fake-cc's canonical `FileEditTool` prompt text.
- Updated error/success messages to match fake-cc's `mapToolResultToToolResultBlockParam` output format.
- Added identical-string guard: returns an error when `old_string == new_string` (matching fake-cc's `validateInput`).

**Notes**: The desanitization table and XML-like strings required writing the file via Python script to avoid the Claude Code tool infrastructure stripping angle-bracket tags from bash heredocs and tool parameters.

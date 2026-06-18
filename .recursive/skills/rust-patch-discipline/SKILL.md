---
type: Skill
name: rust-patch-discipline
description: |
  Surgical-edit guide for Rust files using V4A apply_patch. Read this
  skill BEFORE editing any .rs file with apply_patch when you're not
  sure how to write the patch. Covers anchor selection, the
  "unique context" rule, common errors, and recovery patterns.
---

# rust-patch-discipline

## When to apply this skill

Trigger words: `apply_patch`, `V4A`, "patch rejected", "ambiguous
context", "Update File", any time the previous `apply_patch` call
returned an error.

## V4A in 60 seconds

```text
*** Begin Patch
*** Update File: <relative-path>
@@ <first-anchor-line>
 <unchanged context line>
-<line to remove>
+<line to add>
 <another unchanged context line>
*** End Patch
```

**Critical rules**:

1. **Anchor (`@@ ...`) must be unique within the file.** If three
   lines look identical (e.g. test `let mut x = ...`), the patcher
   will reject with "ambiguous context".

2. **Context lines (` `-prefixed) must match the file byte-for-byte.**
   Even leading whitespace matters. Re-read the section with
   `read_file` if you're not certain.

3. **One hunk = one logical change.** Don't try to make four edits
   in one hunk if they're spread over 50 lines. Use multiple
   `*** Update File:` blocks instead.

4. **`-` lines come BEFORE `+` lines** within a hunk.

## When patches keep failing — escalation ladder

Tried once with apply_patch and it failed? → Re-read the section, widen
the anchor by 2-3 more context lines, try again.

Tried twice? → Use `write_file` to rewrite the entire (small) file.
Only acceptable if the file is < 200 lines.

Tried three times with the same patch? → The agent will be flagged as
`Stuck`. Stop and try a different strategy entirely.

## Recovery patterns

- "ambiguous context" → look at the file around the anchor, choose
  a line that's unique (e.g. a comment, an import, a function signature).
- "context line N does not match" → re-read the file fresh; you're
  patching against stale content.
- "file does not exist" → use `*** Add File:` (note: Add, not Update).
- "failed to parse" → check that lines don't have stray Unicode
  (em-dash, smart quotes) that aren't in the original.

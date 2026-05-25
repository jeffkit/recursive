# Goal: apply_patch tolerates unified-diff hunk headers

## Motivation

LLMs trained on Git/GitHub overwhelmingly produce unified-diff hunk
headers — `@@ -14,6 +14,28 @@ context` — even when the prompt explicitly
says "use V4A". Recursive's own self-improvement runs have wasted 5-8
steps per attempt because the model writes a header in that shape,
`apply_patch` rejects it, the model retries with the same mistake, and
eventually falls back to `write_file` (which works but burns tokens
rewriting the whole file).

We can absorb this bias in the parser without changing the patch
semantics: the line-number ranges in unified-diff headers are advisory
anyway, and we already do anchor-text matching. Treat
`@@ -N,M +N,M @@ <anchor>` as syntactically equivalent to `@@ <anchor>`.

## Requirements

### 1. Extend the hunk-header parser

In `src/tools/apply_patch.rs`, in the body of `parse_patch` where a
`@@`-prefixed line is consumed for an `Update` op (currently around
line 226–245), augment the anchor extraction:

- After `line.strip_prefix(HUNK_SEP)`, the remainder is currently
  `rest.trim()`. Treat this remainder with three cases:
  1. Empty → `anchor = None`. (Already the current behaviour.)
  2. Starts with a unified-diff range pattern that this regex/match
     accepts:
     `-\d+(,\d+)?` `\s+` `\+\d+(,\d+)?` `\s+` `@@` `\s*` `<trailing>`
     Strip that prefix; the **trailing** part (after the second `@@`)
     becomes the anchor. If trailing is empty, `anchor = None`.
  3. Otherwise → keep the existing V4A behaviour: the whole rest is
     the anchor.

Implementation suggestions (any of these is fine, pick the cleanest):
- A handwritten helper `fn normalize_hunk_header(rest: &str) -> Option<String>`
  with unit tests. Pure function, easy to verify.
- Or inline scanning: count the digits/commas/spaces manually. No regex
  crate is allowed; the project deliberately stays std-only.

The header `@@ -14,6 +14,28 @@ pub use mock::MockProvider;` must produce
`anchor = Some("pub use mock::MockProvider;")`.

The header `@@ -14,6 +14,28 @@` (no trailing) must produce `anchor = None`.

The header `@@ -14 +14 @@ fn foo` (count omitted, allowed by unified diff)
must produce `anchor = Some("fn foo")`.

The line numbers themselves are **discarded**. We do not validate them
against the file, because doing so would defeat the whole point of
context-based application.

### 2. Update the `apply_patch` tool's description string

In the same file, in `fn spec()` (around line 71), add a single sentence
to the existing description: something like

> "Both `@@ <unique_line>` (V4A) and
> `@@ -N,M +N,M @@ <unique_line>` (unified-diff style) headers are
> accepted; the line-number range is ignored, only the anchor text after
> the final `@@` is used to locate the hunk."

Keep the rest of the description intact.

### 3. Update `.dev/AGENTS.md` worked example

The current worked example for V4A in `.dev/AGENTS.md` warns the model
**not** to write `@@ -14,6 +14,28 @@`. After this change that warning
is wrong — it'd discourage a now-valid form. Soften it to:

> "Both `@@ <anchor>` and `@@ -N,M +N,M @@ <anchor>` are accepted; the
> line-number range, when present, is ignored. What matters is the
> anchor text after the final `@@` and the byte-for-byte context lines
> that follow."

Keep the rest of the AGENTS.md guidance intact. This is the only file
under `.dev/` that you may edit for this goal.

## Tests to add

In the `tests` module inside `src/tools/apply_patch.rs`:

1. `parses_v4a_anchor` — `@@ fn foo` → `anchor = Some("fn foo")`.
2. `parses_unified_header_with_anchor` —
   `@@ -10,5 +10,7 @@ pub use foo;` → `anchor = Some("pub use foo;")`.
3. `parses_unified_header_without_anchor` —
   `@@ -10,5 +10,7 @@` → `anchor = None`.
4. `parses_unified_header_singular_counts` —
   `@@ -10 +10 @@ fn bar` → `anchor = Some("fn bar")`.
5. `applies_patch_with_unified_header_end_to_end` —
   construct a small file, write a patch using a unified-diff-style
   header, call the `ApplyPatch` tool through `Tool::execute`, assert
   the file now has the intended content.

All existing parser/applier tests must continue to pass unchanged.

## Out of scope

- Full unified-diff support: `--- a/path` / `+++ b/path` headers, hunk
  ranges that span multiple files, `diff --git` preludes.
- Line-number validation. The model can write any numbers it likes;
  they're advisory.
- New external crates (no `regex`, no `lazy_static`). std only.
- Editing `README.md` or any product file under `src/` that isn't
  `apply_patch.rs`.

## Done = all of

- `cargo fmt --all -- --check` clean
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo test` green (65 + the five new tests above = 70)
- Manual smoke: feed `apply_patch` a tiny update whose header is
  `@@ -1,3 +1,4 @@ context_line` and confirm it applies.

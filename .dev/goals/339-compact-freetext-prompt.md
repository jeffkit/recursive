# Goal 339 — Upgrade the free-text compaction fallback prompt to a structured 9-section template

**Roadmap**: Compaction upgrade (WS-6b — improve summary quality, reduce recompaction)

**Design principle check**:
- Implemented as: a new `src/compact/prompt.rs` module holding the prompt
  template + a `format_compact_summary` stripper; the free-text fallback in
  `Compactor::compact` calls them.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change the structured-output path (still preferred) — only
  the free-text fallback prompt text and post-processing.

## Why

`Compactor::compact` prefers structured output (`try_structured_compact`,
JSON schema `summary`/`kept_facts`/`next_steps`), falling back to a free-text
prompt when the provider doesn't support structured output or returns
invalid JSON. The current free-text prompt (`compact/mod.rs:315`) is a single
"Summarize in ≤300 words" instruction — low structure, variable quality,
which contributes to recompaction chains (goal 338): a vague summary fails to
carry enough context, so the next turn re-bloats and re-compacts.

fake-cc uses a 9-section template (Primary Request / Key Concepts / Files &
Code / Errors / Problem Solving / All user messages / Pending Tasks / Current
Work / Optional Next Step) with an `<analysis>` drafting scratchpad that is
stripped before the summary enters context (`format_compact_summary`). The
analysis block improves summary quality without consuming post-compact
context. This goal ports that structure to Recursive's free-text fallback.

The structured path is untouched — it already produces `summary`/`kept_facts`/
`next_steps` and is preferred. This goal only lifts the fallback quality so
providers without structured output (some OpenAI-compatible endpoints,
DeepSeek when structured is unavailable) get the same summary depth.

## Scope (do exactly this, no more)

### 1. `src/compact/prompt.rs` — new module

```rust
//! Free-text compaction prompt template + post-processing.

/// Preamble telling the model to produce text only (no tool calls during
/// compaction). The compaction call uses an empty tool list already, but
/// this reinforces it for models that hallucinate tool calls.
pub const FREE_TEXT_COMPACT_PROMPT: &str = /* the 9-section template below */;

/// Strip the `<analysis>…</analysis>` drafting scratchpad from the model's
/// response and convert `<summary>…</summary>` into readable section
/// headers. Returns the cleaned summary. If the tags are absent, returns
/// the input trimmed (graceful degradation for models that ignore the
/// template).
pub fn format_compact_summary(raw: &str) -> String { /* ... */ }
```

The prompt template (port fake-cc's `BASE_COMPACT_PROMPT` structure, adapted
to Recursive's terminology — "coding agent", "file paths modified, key
technical decisions, test outcomes, unresolved errors"):

```
Your task is to create a detailed summary of the conversation so far, ...
Before providing your final summary, wrap your analysis in <analysis> tags ...
Your summary should include the following sections:
1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code Sections (file paths modified, full code snippets where applicable)
4. Errors and fixes
5. Problem Solving
6. All user messages (non-tool-result)
7. Pending Tasks
8. Current Work (what was being worked on immediately before this summary)
9. Optional Next Step (directly in line with the most recent request; include verbatim quotes)
Wrap the final summary in <summary>…</summary>.
```

`format_compact_summary`:
1. Remove the first `<analysis>…</analysis>` block (non-greedy, `[\s\S]*?`).
2. Extract `<summary>…</summary>` content; if present, replace the whole
   match with `Summary:\n<content trimmed>`.
3. Collapse 3+ newlines to 2. Trim. If neither tag is present, return
   `raw.trim()` unchanged.

### 2. `src/compact/mod.rs` — use the new prompt in the fallback

In `Compactor::compact`'s free-text fallback (`compact/mod.rs:313-326`),
replace the inline `summary_prompt` with:
```rust
let summary_prompt = format!(
    "{}\n\nConversation to summarize:\n{}",
    crate::compact::prompt::FREE_TEXT_COMPACT_PROMPT,
    older_text,
);
let completion = provider
    .complete(&[Message::user(summary_prompt)], &[] as &[ToolSpec])
    .await?;
let raw = completion.content;
crate::compact::prompt::format_compact_summary(&raw)
```
Keep the structured path (`try_structured_compact`) unchanged and still
preferred. The `[compacted: ...]` header wrapping (`compact/mod.rs:331`)
stays — it wraps the formatted summary.

### 3. Tests

`src/compact/prompt.rs`:
- `format_strips_analysis_block` — input with `<analysis>thinking</analysis>
  <summary>real</summary>` → output contains `real`, not `thinking`.
- `format_converts_summary_tag_to_header` — `<summary>X</summary>` → `Summary:\nX`.
- `format_preserves_content_when_no_tags` — plain text → trimmed plain text.
- `format_collapses_excess_newlines`.
- `prompt_contains_all_nine_sections` — assert the template string mentions
  each of the 9 section titles (guards against accidental truncation).

`src/compact/mod.rs`:
- `compact_freetext_fallback_uses_structured_prompt_and_formats` —
  `MockProvider` returning a response with `<analysis>…</analysis><summary>…</summary>`;
  assert the resulting summary message's content contains the formatted
  `Summary:` header and NOT the analysis text, and still carries the
  `[compacted:` header.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- Structured path unchanged; free-text fallback now uses the 9-section
  template and `format_compact_summary`.
- A model that ignores the template (no tags) still produces a usable
  summary (graceful degradation, no panic, invariant #5).

## Notes for the agent

- **Do not change the structured-output path or its JSON schema.** This goal
  is strictly the free-text fallback. The structured path remains preferred
  and is the common case for providers that support it.
- **Adapt terminology, don't copy verbatim** — fake-cc's prompt mentions
  Claude-specifics; Recursive's prompt should say "coding agent" and
  reference Recursive's tool names generically. Keep the 9 section titles
  identical so `prompt_contains_all_nine_sections` is a stable guard.
- The `<analysis>` block is a *drafting scratchpad* — it is stripped so it
  does not consume post-compact context, but its presence during generation
  improves the summary. This is the key quality lever.
- This goal pairs with goal 338 (recompaction telemetry): after landing both,
  dogfood and check whether recompaction chains decrease. Note observations
  in the journal.
- **DO NOT modify** `src/run_core.rs`, `src/runtime.rs`, `src/llm/`,
  `src/kernel.rs`, the structured `try_structured_compact` path, or tool
  files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-freetext-prompt.md`.

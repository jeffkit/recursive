# Goal 19 — Auto-trim tool results when transcript exceeds a budget

## Why

Goal-07 added `max_transcript_chars` as a **hard stop**: when the
transcript hits the limit, the agent surrenders with
`FinishReason::TranscriptLimit`. That's safe but blunt — many useful
long-running tasks die at step 12 because old `ToolResult` blobs (often
huge `cargo build` dumps) eat the budget.

A nicer policy: when the transcript would exceed the budget, **trim
the oldest `ToolResult` content** down to a placeholder
(e.g. `"[older tool output trimmed to fit budget]"`) until we're back
under. System message, user goal, and the most recent K
assistant/tool messages stay intact. The agent keeps making progress
with degraded context instead of stopping.

This is purely additive: callers who don't set
`max_transcript_chars` see no change.

## Scope

Touches: `src/agent.rs` only.

1. Add a method on `Agent` (private is fine —
   `fn maybe_trim_transcript(&mut self)`) that is called inside
   `run()` at the same point where the current `max_transcript_chars`
   *hard stop* lives. Replace the hard stop's "return immediately"
   behaviour with:
   - If `chars >= limit`, walk the transcript from index 1 (skip
     the system prompt at 0) forward, and for any `Role::Tool` (or
     equivalent — adapt to whatever shape we're using) message whose
     `content.len() > 200`, replace `content` with
     `"[older tool output trimmed to fit budget]"`. Recompute
     `chars` after each replacement; stop trimming as soon as
     `chars < limit`.
   - If after trimming every old tool result we're still over the
     limit, *then* fall through to the existing hard stop
     (`FinishReason::TranscriptLimit`). The hard stop is now the
     fallback, not the default.

2. Add a new `FinishReason::TranscriptLimit` is unchanged in shape.
   But add a new `StepEvent` variant **OR** reuse an existing one
   (e.g. a new entry in the existing `StepEvent::AssistantText`
   stream prefixed with `[trimmed N old tool results to fit budget]`)
   to surface that trimming happened. Choose whichever is less
   invasive — a new variant is cleaner but requires more wiring;
   reusing the assistant_text stream is hacky but small. Pick what
   you can justify in the journal.

3. Tests (`src/agent.rs::tests`):
   - A test that builds a transcript with one big tool result
     followed by several small assistant messages, sets
     `max_transcript_chars` just under the big result's size, and
     asserts that after `run()` the big result is replaced by the
     trim placeholder, the agent ran to completion (not
     `TranscriptLimit`), and the new finish reason is
     `NoMoreToolCalls`.
   - A test that exhausts the budget even after trimming all tool
     results — asserts the old hard-stop fallback still fires with
     `FinishReason::TranscriptLimit`.

## Acceptance

- `cargo build` green.
- `cargo test` green (114 baseline + ≥2 new = 116 total).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- Read `src/agent.rs` carefully to find the **exact location** of
  the current `max_transcript_chars` check and the
  `FinishReason::TranscriptLimit` emit. There's only one such site
  (around line 110 at last edit).
- The transcript is `Vec<Message>` on the `Agent` struct. `Message`
  has `role: Role` and `content: String` fields; check
  `src/message.rs` for the exact `Role` variants you can match.
- Use `apply_patch` for all edits. The file is large but each
  change is small.
- This is a **single-file** product change. Should fit in 8–12 steps.
- **Avoid `Message::user("foo".into())` in tests** — use
  `.to_string()` instead. The `impl Into<String>` constructor needs
  the explicit type. See AGENTS.md section 5.

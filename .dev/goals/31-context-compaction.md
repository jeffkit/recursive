# Goal 31 — Context Compaction (LLM-driven summarization)

**Roadmap**: 1.1 — Context Compaction

**Design principle check**:
- Implemented as: **new module** `src/compact.rs` + **new StepEvent
  variant** `StepEvent::Compacted`. The agent loop only emits an event
  and delegates to the module; it does NOT branch on compaction logic
  internally.
- ❌ Does NOT add new branches inside `agent.rs::Agent::run`'s main
  loop. The compact call is at most a single `self.maybe_compact().await?`
  hook between LLM call and tool dispatch, identical in shape to the
  existing `maybe_trim_transcript`.

## Why

`maybe_trim_transcript` (goal-19) replaces old `ToolResult` content
with a placeholder when the character budget runs out. That's lossy
and crude — important decisions, file paths, and intermediate findings
get nuked equally with stale `cargo build` output.

Every major competitor (Claude Code, Codex, Hermes) does **LLM-driven
summarization** instead: when prompt tokens approach the context
window's ceiling, ask the model to compress the older portion into a
single summary message that preserves key decisions, paths modified,
and test outcomes. The agent then continues with the summary + recent
turns.

This unlocks tasks that exceed one context window, which today
hard-fails as `TranscriptLimit` or silently degrades after trimming.

## Scope

Touches: new `src/compact.rs`, `src/agent.rs` (new `StepEvent` variant
+ a single hook call), `src/lib.rs` (re-export).

1. New module `src/compact.rs`:
   - `pub struct Compactor { threshold_chars: usize }` configured via
     a builder field on `AgentBuilder` (default e.g. 80% of
     `max_transcript_chars` when set, else `usize::MAX` = disabled).
   - `pub async fn compact(provider: &dyn LlmProvider, transcript:
     &[Message], keep_recent_n: usize) -> Result<Message>`:
     - Splits transcript into `older` (everything before last
       `keep_recent_n` messages) and `recent` (kept verbatim).
     - Calls `provider.complete` with a meta-prompt:
       *"Summarize the following conversation in ≤300 words.
       Preserve: file paths modified, key technical decisions, test
       outcomes, and any errors not yet resolved. Drop: file
       contents, repeated tool errors, exploratory dead-ends."*
     - Returns a single `Message::system("[compacted: N messages → M
       chars]\n<summary>")` to be substituted in.

2. In `src/agent.rs`:
   - Add `StepEvent::Compacted { removed: u32, kept: u32, summary_chars: u32 }`.
   - In `Agent::run`'s step loop, between `maybe_trim_transcript` and
     the LLM call: if compactor configured AND transcript prompt-char
     estimate ≥ threshold AND we have ≥ `keep_recent_n+2` messages,
     call `Compactor::compact`, replace old portion with the summary
     message, emit the event.
   - `AgentBuilder::compactor(Compactor) -> Self` setter.

3. Tests:
   - Unit test on `Compactor::compact` with `MockProvider` returning a
     canned summary. Assert returned `Message` role is system and
     contains the expected substring.
   - Agent-level test: run an agent with low threshold (e.g. 200
     chars) and 3 scripted tool calls; assert exactly one
     `StepEvent::Compacted` is emitted and the transcript afterwards
     is shorter.

## Acceptance

- `cargo build` green.
- `cargo test` green (140 baseline + 2 new = 142+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- Compaction is **disabled by default** (threshold = usize::MAX). No
  behavior change on existing self-improve runs unless explicitly
  enabled via the new builder setter.

## Notes for the agent

- The hot path is delicate. Read `src/agent.rs::Agent::run` carefully
  before editing — model after the existing `maybe_trim_transcript`
  pattern. One added await + one event emit, nothing more.
- Compaction itself spends LLM tokens; the goal isn't to compact often,
  it's to compact at most a handful of times in any single run.
- `keep_recent_n` should be conservative (e.g. last 8 messages) so the
  agent retains immediate context. Older bulk gets summarized.
- A note on dogfooding: self-improve.sh runs **disable** compaction
  for now — we want characterization data on raw transcript growth
  first. The builder default of usize::MAX preserves that.
- Use `apply_patch` for source edits. `.to_string()` over `.into()` in
  tests.

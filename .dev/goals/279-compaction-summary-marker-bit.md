# Goal 279 — Compaction summary marker bit on Message

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-KERN-16)

**Design principle check**:
- Implemented as: add a private `is_compaction_summary: bool`
  field to `Message`, set by `Compactor::apply_to_transcript` /
  `compact`, checked by `kernel.rs::AgentKernel::run` instead of
  the current `content.contains("[compacted:")` heuristic.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
  (the heuristic is *in* AgentKernel::run, not the legacy
  Agent::run which no longer exists; replacing it is not
  "adding a branch" but removing a string-sniff)
- ❌ Does NOT add a new feature flag

## Why

`src/kernel.rs:302-307` (AgentKernel::run, after the inner
loop):

```rust
if !inner.messages.is_empty()
    && inner.messages[0].role == crate::message::Role::System
    && inner.messages[0].content.contains("[compacted:")
{
    new_messages.insert(0, inner.messages[0].clone());
}
```

This sniffs the rendered compaction summary header text to decide
whether the message at index 0 is a summary the kernel inserted.
If a user prompt happens to contain the literal text
`"[compacted:"` (e.g. they are debugging compaction behavior or
writing a guide about it), the kernel treats their message as a
compaction summary — incorrectly prepending it to the new
messages list and losing the actual summary.

Compactor also writes the literal text in src/compact.rs:289-294:

```rust
let header = format!(
    "[compacted: {} messages → {} chars at step {step}]\n{}",
    ...
);
```

And the test assertions in src/compact.rs:327, 377, 472, 501
also rely on `content.contains("[compacted:")`.

The fix is mechanical: a typed marker on `Message` replaces the
string sniff. Tests in compact.rs must be updated.

## Scope (do exactly this, no more)

### 1. Add the field to `Message`

In `src/message.rs:20-32`, add a private field:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// True when this message was inserted by `Compactor` as a
    /// summary of older messages. Used by AgentKernel::run to
    /// detect intra-turn compaction summaries without sniffing
    /// the rendered content text. Defaults to false; old
    /// transcripts on disk serialize as false (field omitted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_compaction_summary: bool,
}

#[inline]
fn is_false(b: &bool) -> !*b {
    !*b
}
```

(Adjust the helper if the same name exists elsewhere — use a
unique name like `is_summary_false`.)

### 2. Update all `Message::xxx` constructors

The five constructors (`system`, `user`, `assistant`,
`assistant_with_tool_calls`, `tool_result`) at src/message.rs:35-87
all set `is_compaction_summary: false` explicitly. Direct
struct-literal construction (in tests, in the CompactOr's
internal paths) sets it the same way.

### 3. Set the bit in `Compactor`

In `src/compact.rs:296` (the `Ok(Message::system(header))` return):

```rust
Ok(Message::system(header).with_compaction_summary())
```

Add a method to `Message`:

```rust
impl Message {
    pub fn with_compaction_summary(mut self) -> Self {
        self.is_compaction_summary = true;
        self
    }
}
```

And in `Compactor::apply_to_transcript` (src/compact.rs:215), the
`transcript.insert(0, summary_msg)` already inserts a summary
that has the bit set (because `compact()` returns it set). No
change needed here.

### 4. Replace the string sniff in AgentKernel::run

In `src/kernel.rs:302-307`, replace the heuristic:

```rust
if !inner.messages.is_empty() && inner.messages[0].is_compaction_summary {
    new_messages.insert(0, inner.messages[0].clone());
}
```

This is now an O(1) field check, not a string contains.

### 5. Update tests in src/compact.rs

The four assertions that use `content.contains("[compacted:")` at
lines 327, 377, 472, 501 — update them to ALSO check
`summary_msg.is_compaction_summary`:

```rust
assert!(summary_msg.is_compaction_summary,
    "compactor must mark summary message with the bit");
assert!(summary_msg.content.contains("[compacted:"),
    "compactor must still include the [compacted: header for debuggability");
```

(Both must hold — the marker bit AND the legacy header text.
External consumers reading the JSONL may still grep the header
for log analysis; the bit is for in-process kernel logic.)

### 6. Tests

In `src/message.rs` `mod tests`:

```rust
#[test]
fn compaction_summary_bit_default_false() {
    let m = Message::user("hi");
    assert!(!m.is_compaction_summary);
}

#[test]
fn with_compaction_summary_sets_bit() {
    let m = Message::system("...").with_compaction_summary();
    assert!(m.is_compaction_summary);
}

#[test]
fn compaction_summary_bit_omitted_from_json_when_false() {
    let m = Message::user("hi");
    let json = serde_json::to_string(&m).unwrap();
    assert!(!json.contains("is_compaction_summary"));
}
```

In `src/kernel.rs` `mod tests`:

```rust
#[test]
fn kernel_detects_compaction_summary_by_bit_not_string() {
    // Build a fake Message with is_compaction_summary=false but
    // content containing the [compacted: literal. Assert the
    // kernel's new check does NOT misclassify it.
    // ... requires a tiny mock LLM / kernel test harness.
}
```

## Acceptance

- `cargo test --workspace` — green (existing + new tests)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "contains(\"\\[compacted:\")" src/` — only the 4
  compact.rs test assertions (intentional, for the legacy header
  text). The kernel.rs heuristic is gone.
- `grep "is_compaction_summary" src/` — ≥ 6 matches: message.rs
  field + setter, compact.rs 2 setters, kernel.rs check, 3
  tests.

## Notes for the agent

- The `is_false` skip-serializing-if attribute is critical for
  backward compatibility. Old JSONL session files don't have the
  field; default-false means they deserialize cleanly without a
  migration.
- The string header `"[compacted: ..."` is still emitted in the
  content — this is for human-readable log analysis (the journal
  tooling at .dev/journal/ greps this). Don't remove the header.
- Estimated diff: 4 files (message.rs, compact.rs, kernel.rs,
  tests inline). ~40 lines net.
- **Test discipline reminder (from g268 post-mortem)**: prefer
  deterministic serde / field-level assertions. The kernel test
  with a mock LLM is the exception (mock + bit check).

**Disjoint file guarantee**: This goal touches src/message.rs,
src/compact.rs, src/kernel.rs. Goal 278 touches src/event.rs +
src/runtime.rs. No overlap — safe to run in parallel.
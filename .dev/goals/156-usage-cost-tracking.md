# Goal 156 — Token usage & cost tracking in TranscriptEntry

> **Roadmap**: Phase 18.6 — Transcript fidelity (part 2: token economics)
> **Design principle check**:
> - **Orthogonal**: usage data flows from the LLM response into
>   `TranscriptEntry.usage` without touching the agent loop or the
>   LLM wire format. The kernel already has `usage` in `Completion`.
> - **No LLM-wire changes**: `Message` (the wire type) stays lean;
>   `TranscriptEntry` (the persistence type) is enriched.
> - **Additive**: `usage` is `Option<UsageMeta>`, so existing JSONL
>   files and sessions without usage data load without error.
> - **Depends on**: g152 (incremental writes via
>   `SessionPersistenceSink`).

## Why

Every Anthropic API call returns token counts and, for prompt-cached
sessions, cache creation and cache read counts. These are available
in `Completion::usage` (`src/llm/mod.rs`) but today they are
**discarded** after the kernel processes them—never written to disk.

This means:

- There is no way to tell after the fact how much a session cost.
- We cannot build a cost dashboard, budget guard, or token burn-rate
  chart without re-calling the API.
- Agent self-improvement runs (`.dev/scripts/self-improve.sh`) record
  cost metrics in memory but not in the session JSONL—the data is
  lost when the process exits.
- Multi-agent orchestration cannot attribute cost per subagent.

Claude Code's JSONL stores the full Anthropic API response including
`usage.input_tokens`, `usage.output_tokens`,
`usage.cache_creation_input_tokens`, and
`usage.cache_read_input_tokens` on every assistant entry. Our JSONL
currently omits all of this.

## What this goal does

### 1. Define `UsageMeta`

```rust
/// Token usage for one LLM API call, as reported by the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageMeta {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Prompt-cache creation tokens (Anthropic / OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// Prompt-cache read tokens (Anthropic / OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    /// Reasoning/thinking tokens (DeepSeek R1, o1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}
```

Place in `src/llm/mod.rs` (alongside `Completion`, `ToolCall`).

### 2. Surface `UsageMeta` in `Completion`

```rust
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<UsageMeta>,   // already exists as a field but typed as ()
    pub reasoning_content: Option<String>,
}
```

Concretely, `Completion::usage` already exists but is currently
`Option<serde_json::Value>` (never populated). Replace with
`Option<UsageMeta>` and populate it in each provider adapter:

- `src/llm/anthropic.rs` — map `usage.input_tokens`,
  `usage.output_tokens`, `usage.cache_creation_input_tokens`,
  `usage.cache_read_input_tokens`.
- `src/llm/openai.rs` — map `usage.prompt_tokens`,
  `usage.completion_tokens`.
- `src/llm/deepseek.rs` — map same as OpenAI; add
  `reasoning_tokens` from DeepSeek's extended response.
- `src/llm/mock.rs` — populate with zeros for tests.

### 3. Add `usage` to `TranscriptEntry`

```rust
pub struct TranscriptEntry {
    // …existing fields…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageMeta>,    // NEW
}
```

### 4. Thread usage through `AgentEvent::MessageAppended`

The `MessageAppended` event currently carries only a `Message`.
`Message` (the LLM wire type) does not carry usage because usage
is not sent back to the LLM. Extend the event:

```rust
MessageAppended {
    message: crate::message::Message,
    parent_uuid: Option<String>,  // from g155
    usage: Option<crate::llm::UsageMeta>,  // NEW
},
```

In `AgentRuntime::run`, when emitting `MessageAppended` for the
assistant batch returned by `turn_outcome`, pull the usage from
`turn_outcome.usage` (already returned by the kernel) and attach it.

### 5. Update `SessionPersistenceSink`

`SessionPersistenceSink::emit` passes the `usage` field from the
event to `SessionWriter::append`.

```rust
pub fn append(
    &mut self,
    msg: &Message,
    parent_uuid: Option<&str>,  // from g155
    usage: Option<&UsageMeta>,  // NEW
) -> std::io::Result<String> { … }
```

### 6. Session-level cost summary in `.meta.json`

`SessionWriter::finalize` (called at the end of a session) computes
cumulative totals:

```rust
pub struct SessionCost {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub turns: u32,
}
```

Written to `.meta.json` as `"cost": { … }`. Enables listing sessions
by token cost without re-reading the full JSONL.

### 7. `recursive sessions list` cost column (stretch)

If time permits, add a `--show-cost` flag to the `sessions list`
subcommand that reads `cost` from `.meta.json` and prints a column.
This is **optional** — the core data storage is mandatory; the CLI
flag can follow in a subsequent PR.

## Scope

### In scope

- `UsageMeta` struct in `src/llm/mod.rs`.
- Populate `Completion::usage` in all four provider adapters.
- Add `usage: Option<UsageMeta>` to `TranscriptEntry`.
- Thread usage through `MessageAppended` event → `SessionWriter`.
- `SessionCost` accumulated in `SessionWriter`, written to `.meta.json`.
- Unit tests for each provider adapter's usage parsing.
- Integration test: run one session, read `.meta.json`, assert
  non-zero token counts.

### Out of scope

- Pricing calculation (USD cost) — provider pricing changes too
  often to hard-code; provide raw tokens only.
- `recursive sessions list --show-cost` CLI column (stretch).
- Per-tool usage attribution.
- Any TUI or HTTP API changes.

## Files to change

| File | Change |
|------|--------|
| `src/llm/mod.rs` | Add `UsageMeta`; replace `Completion::usage: Option<Value>` with `Option<UsageMeta>` |
| `src/llm/anthropic.rs` | Populate `UsageMeta` from API response |
| `src/llm/openai.rs` | Populate `UsageMeta` from API response |
| `src/llm/deepseek.rs` | Populate `UsageMeta` from API response |
| `src/llm/mock.rs` | Return zero `UsageMeta` |
| `src/session.rs` | Add `usage` to `TranscriptEntry`; update `SessionWriter::append`; add `SessionCost` to `SessionMeta` |
| `src/event.rs` | Add `usage: Option<UsageMeta>` to `MessageAppended` |
| `src/runtime.rs` | Attach usage to `MessageAppended` events |

## Acceptance criteria

- [ ] A completed session's `transcript.jsonl` has `"usage"` on every
      `role: "assistant"` entry (non-zero input/output counts).
- [ ] A completed session's `.meta.json` has a `"cost"` object with
      cumulative totals that equal the sum of per-message usage.
- [ ] Provider adapters parse cache tokens where the API provides them
      (Anthropic); fall back to zeros for adapters that don't.
- [ ] Old JSONL files without `usage` load without error.
- [ ] `cargo test --all-targets` green; `cargo clippy -D warnings`
      green.

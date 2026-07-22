# Goal: per-component context breakdown (Cursor-style context usage)

## Motivation

Today Recursive's notion of "context size" is a single aggregate number:
`UsageStats::last_prompt_tokens` in `crates/recursive-tui/src/cost.rs`, computed
as `max(input_tokens, cache_hit + cache_miss)` from the **last** LLM call's
provider-reported usage. Two problems:

1. **No breakdown.** It is one blob — there is no way to see how much of the
   window is system prompt vs. tool definitions vs. skills vs. conversation,
   the way Cursor's "Context Usage" panel does (system prompt / tool
   definitions / rules / skills / MCP & dynamic tools / subagent definitions /
   conversation).
2. **Stale between calls.** It only refreshes when the provider returns usage,
   so during tool execution (or right after a compaction) the gauge does not
   reflect the *current* transcript size.

No provider returns a per-component token split (Anthropic / DeepSeek only
split by cache hit/miss, not by system-vs-tools-vs-conversation). So the only
way to get a Cursor-style breakdown is to **estimate each component locally**
before the prompt is sent, then reconcile the sum against the provider's
reported total.

## Requirements

### 1. New `ContextBreakdown` type

In `src/llm/chat.rs` (next to `TokenUsage`), add:

```rust
/// Locally-estimated per-component token breakdown of the prompt sent to the
/// provider. Distinct from `TokenUsage` (which is the provider's reported
/// truth): these are local chars/4 estimates, one bucket per logical segment
/// of the assembled prompt. The `overhead` bucket absorbs the difference
/// between the sum of the other buckets and the provider's reported
/// `prompt_tokens` (chat-template wrapping, tool JSON envelope, message
/// separators) so the breakdown is honest about its estimation error rather
/// than pretending to be exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub system_prompt: u32,   // default_system_prompt + 6 memory layers + append
    pub rules: u32,           // AGENTS.md + CLAUDE.md (prepend_project_context)
    pub skills: u32,          // skill_index()
    pub subagents: u32,        // coordinator workflow + subagent note
    pub tools: u32,            // eager ToolSpec JSON
    pub mcp_dynamic: u32,      // ToolSpec subset from MCP / deferred tools
    pub conversation: u32,     // self.messages
    pub overhead: u32,         // provider total - local sum (saturating to 0)
}
```

Add a free function `estimate_tokens(text: &str) -> u32` using the existing
chars/4 heuristic (ceil(chars/4)). **Reuse** the logic already in
`src/tools/estimate_tokens.rs::EstimateTokens::estimate` — extract the
heuristic into a shared helper (e.g. `crate::llm::estimate_tokens`) and have
both `EstimateTokens` and the new breakdown code call it. Do not duplicate the
formula. No new dependencies (no tiktoken / anthropic tokenizer in this goal —
that is a future feature-flag goal).

### 2. Structured system-prompt assembly (backward-compatible)

`src/system_prompt.rs::assemble_system_prompt` currently returns a single
`String` that concatenates project-context (rules) + base (system prompt) +
skill index + subagent note. To separate those four buckets it must return
**structured segments**.

Change the return type to a new struct:

```rust
pub struct AssembledPrompt {
    /// The fully-joined system prompt, byte-identical to what
    /// `assemble_system_prompt` returned before this goal. Existing
    /// call sites obtain this via `.full()` / `AssembledPrompt::full`.
    pub full: String,
    /// Per-segment substrings (owned) for breakdown estimation.
    /// Each segment is the exact text that contributes to that bucket;
    /// join order matches `full`.
    pub segments: PromptSegments,
}

pub struct PromptSegments {
    pub rules: String,        // "# Project context" + AGENTS.md + CLAUDE.md
    pub system_prompt: String, // base (default_system_prompt + memory + append)
    pub skills: String,        // skill_index output ("" if none)
    pub subagents: String,     // coordinator workflow + note ("" if disabled)
}
```

**Backward-compatibility (critical — see impact note below):** the existing
`assemble_system_prompt` is called from 8 production sites + 5 tests. To keep
the blast radius mechanical, `AssembledPrompt` must provide:

- `pub fn full(&self) -> &str { &self.full }`
- `impl std::fmt::Display for AssembledPrompt` writing `self.full`
- `impl AssembledPrompt { pub fn into_full(self) -> String }`

so every existing call site compiles after a one-token change
(`assemble_system_prompt(...)` → `assemble_system_prompt(...).full()` or
`.into()`). **Do not** change the assembled string itself — byte-identical
output is a hard requirement (prefix-cache stability depends on it; the
ordering test `ordering_project_context_then_base_then_skills_then_subagent`
must still pass unchanged).

Update all 8 production call sites to use `.full()` / `.into()`:
- `crates/recursive-cli/src/cli/builder.rs::build_runtime`
- `crates/recursive-cli/src/main.rs::run_loop`
- `src/http/handlers.rs::run_agent`
- `src/http/handlers.rs::create_session`
- `src/http/handlers.rs::fork_session`
- `src/http/handlers.rs::agui_run`
- `crates/recursive-tui/src/runtime_builder.rs::build_runtime`
- `crates/recursive-tui/src/runtime_builder.rs::build_runtime_with_skill_tx`

Update the 5 tests in `src/system_prompt.rs` to assert on `.full()` where they
currently assert on the returned String, and add a new test asserting
`segments` are populated and non-overlapping (e.g. `rules` contains
"AGENTS.md", `skills` contains "Available skills", `subagents` contains
"Coordinator workflow" only when enabled).

### 3. Compute the breakdown in `run_core`

In `src/run_core.rs`, the runtime must hold the assembled system prompt (or its
segments) so the static buckets can be estimated once. Today `call_llm`
(`run_core.rs:536`) does not receive the system prompt — it is assembled at the
channel layer and passed into the runtime builder. Thread the `PromptSegments`
(or the whole `AssembledPrompt`) onto the runtime state so `call_llm` can read
it.

**Static/dynamic split (performance):**
- **Static buckets** — `system_prompt`, `rules`, `skills`, `subagents`, `tools`,
  `mcp_dynamic` — do not change within a run except on `/model` hot-swap or tool
  registry change. Estimate them **once at run start** and cache on `self`.
  Re-estimate when the tool registry or model changes (whatever existing hook
  the TUI `/model` swap already uses to rebuild specs).
- **Dynamic bucket** — `conversation` — re-estimate every step from
  `self.messages`.
- After the provider returns, set `overhead =
  provider.prompt_tokens.saturating_sub(local_sum)` where `local_sum` is the
  sum of the seven non-overhead buckets. (Use `max(input_tokens, cache_hit +
  cache_miss)` as the provider total, matching `UsageStats`'s existing logic,
  so Anthropic vs OpenAI reporting differences are handled.)

The tool-spec split: `tools` = eager specs that are NOT from MCP / NOT deferred;
`mcp_dynamic` = specs that ARE MCP-sourced or deferred. Use the existing
`ToolRegistry::is_deferred_spec` / MCP provenance already available in
`call_llm`'s deferred-tool partition (`run_core.rs:548-570`). Serialize each
subset with the same JSON shape the provider adapter sends and estimate tokens
on that JSON text.

### 4. New event

In `src/event.rs`, add (kept separate from `Usage` — `Usage` stays the
provider's reported truth, the breakdown is the local estimate):

```rust
AgentEvent::ContextBreakdown {
    breakdown: crate::llm::ContextBreakdown,
    step: usize,
},
```

Emit it once per step, right after `AgentEvent::Usage` (so consumers see the
provider truth first, then the local breakdown). Skip emission on steps with
no LLM call.

### 5. TUI consumption

In `crates/recursive-tui/src/cost.rs`:
- Add `last_breakdown: Option<ContextBreakdown>` to `UsageStats`, updated from
  the new event.
- Change the status-bar gauge source (`last_prompt_tokens`) so it is no longer
  pinned to the last provider report: compute it as
  `static_cached_sum + estimate(conversation)` on every render tick (or on
  transcript mutation), so it advances during tool execution. Keep the
  provider-reported value available for the cost tracker (do not regress cost
  accuracy — `estimate_cost` still uses provider totals).

`record_with_cache` impact is LOW (0 direct callers per GitNexus) — safe to
extend.

### 6. TUI Context Usage panel

In `crates/recursive-tui/src/`, add a key-triggered overlay panel (pick a free
key; candidate `?` if free, else `Ctrl+o`) that renders:
- a horizontal proportional bar with one colored segment per bucket, and
- a legend listing each bucket's name, color, token count, and percentage of
  the window (`context_window_for_model` from `cost.rs`).

Mirror the existing overlay pattern used by the help panel (find it in the TUI
crate and follow the same structure). Add an in-process harness test
(`#[cfg(test)] mod tests` using `crate::harness::Harness`, asserting via
`Screen::find_row` / `row_has_bg_color` / `text()`) that opens the panel and
asserts the legend rows and the bar render. This satisfies the TUI presence
gate.

## Tests to add

1. `estimate_tokens` helper: `""` → 0, `"abcd"` → 1, `"abcde"` → 2 (ceil), and
   that `EstimateTokens::estimate` now delegates to it (no behavior change).
2. `AssembledPrompt::full` is byte-identical to the pre-change output for a
   representative input (project context + base + skills + subagent). Reuse the
   existing `ordering_...` test's setup.
3. `PromptSegments` population: `rules` contains "AGENTS.md", `skills` contains
   "Available skills" only when skills present, `subagents` contains
   "Coordinator workflow" only when `sub_agent_enabled`.
4. `ContextBreakdown` overhead: given a scripted provider that reports
   `prompt_tokens = 1000` and a local sum of 700, `overhead == 300`; given
   local sum > provider total, `overhead == 0` (saturating).
5. Static/dynamic split: the static buckets are estimated once and do not
   change across steps in a 2-step run; `conversation` grows between step 1
   and step 2.
6. `AgentEvent::ContextBreakdown` is emitted once per LLM-calling step and not
   emitted on steps without an LLM call.
7. TUI harness: opening the Context Usage panel renders a legend row per
   bucket and a non-empty bar; closing it restores the prior view.

All existing tests must continue to pass. The `ordering_...` system-prompt
test must pass **unchanged** (byte-identical `full`).

## Out of scope

- Real tokenizers (tiktoken / anthropic tokenizer). chars/4 only; a future
  feature-flag goal can add per-model accuracy.
- Persisting the breakdown across runs / into session meta.
- HTTP API surface for the breakdown (the event is emitted; wiring a JSON
  field into `/sessions/:id/messages` responses is a separate goal).
- Changing cost estimation. `estimate_cost` continues to use provider-reported
  totals, not the local breakdown.

## Impact note (from GitNexus)

`assemble_system_prompt` is **CRITICAL** risk: 13 direct callers across 8
production files + 5 tests, 7 execution flows (`spawn`, `agui_run`, `run_agent`,
`create_session`, `run_loop`, `main`, `build_runtime`). The backward-compatible
`AssembledPrompt` accessor design above is what keeps this mechanical rather
than behavioural. `UsageStats::record_with_cache` is **LOW** (0 direct callers).

## Done = all of:

- `cargo fmt --all -- --check` clean
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo test --workspace` green (existing + the new tests above)
- `.dev/scripts/tui-test-presence.sh` exits 0 (TUI presence gate, hard)
- `.dev/scripts/tui-mutants.sh` run and survivors inside the diff hunks fixed
  (advisory for manual edits; hard for the self-improve flow)
- A manual `recursive` TUI session: open the Context Usage panel and observe
  the seven buckets with non-zero conversation growing across turns
- Journal entry `.dev/journal/manual-<YYYYMMDD>-context-breakdown.md`

# Goal 164 — Anthropic tool search (deferred tool loading)

> **Roadmap**: Phase 18 — LLM fidelity (part: deferred tools)
> **Design principle check**:
> - **Orthogonal**: `Tool::should_defer()` is a property of the tool;
>   providers decide what to do with it. Anthropic honors it; OpenAI
>   ignores it (no provider-side behavior change for OpenAI).
> - **Additive**: `LlmProvider::complete_with_search()` has a default
>   implementation that concatenates eager + deferred and falls back
>   to `complete()`. Existing providers (OpenAI, MockProvider) need
>   no code change.
> - **No LLM-wire changes to `Message`**: the deferred-tool round
>   trip uses existing `tool_use` / `tool_result` blocks. The
>   `tool_reference` content block is emitted inline inside
>   `tool_result.content` as a JSON string — same wire shape, no
>   new field on `Message`.
> - **Agent loop stays small**: this goal adds one new method on
>   `LlmProvider` and one new method on `Tool`. The kernel's main
>   loop calls `complete_with_search` instead of `complete` — the
>   loop body itself does not grow.

## Why

When the tool catalog grows past ~30 entries, model selection accuracy
degrades significantly (Anthropic's "Advanced tool use" engineering
post documents this). For projects with many tools (Recursive's standard
registry has ~20; with skills and memory variants it crosses 25-30),
deferred loading is the standard fix.

Anthropic's Messages API supports two deferred-tool mechanisms:

1. **Server-side**: client declares `tool_search_tool_regex_20251119`
   or `tool_search_tool_bm25_20251119` and the server runs the search.
2. **Client-side (custom)**: client defines a regular `ToolSearchTool`
   and returns `tool_reference` content blocks in its `tool_result`.
   Server expands them into `<functions>` blocks (the same encoding
   the system-prompt tool list uses).

We pick **(2) client-side**, matching `claude-code`'s implementation
(`src/tools/ToolSearchTool/ToolSearchTool.ts:444-470`). Reasons:

- Search algorithm is fully under our control: keyword scoring,
  `searchHint` weighting, CamelCase splitting — Anthropic's server
  can't do this fine-grained scoring.
- Same `ToolSearchEngine` implementation is reusable by any future
  provider that wants client-side semantics.
- Works on every Anthropic-compatible third party (DeepSeek, Qwen,
  GLM, MiniMax) that passes `defer_loading` and `tool_reference`
  through unchanged — they don't have to implement server-side search.

OpenAI's `tool_search` is **out of scope** for this goal. OpenAI
providers do NOT support deferred tool loading; they always see the
full tool list. This goal's design accommodates that: OpenAI uses
the default `complete_with_search` implementation, which is a no-op
merge — OpenAI behavior is byte-for-byte identical to today.

## What this goal does

### 1. Add `should_defer` to the `Tool` trait

`src/tools/mod.rs:66` — extend the trait with a new method:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;
    fn is_readonly(&self) -> bool { false }

    /// Whether this tool is a candidate for deferred loading.
    ///
    /// Default: `false` (tool is always eager-loaded). Override to
    /// `true` for tools that are rarely used or have large schemas,
    /// pushing them behind `ToolSearchTool`.
    ///
    /// The provider decides whether to honor this flag. Anthropic
    /// does (via `defer_loading: true`). OpenAI ignores it — the
    /// tool is sent eagerly like any other.
    fn should_defer(&self) -> bool { false }
}
```

No existing `Tool` impl is required to change — default `false` keeps
all 20+ existing tools eager.

### 2. Add `search_hint` to `ToolSpec`

`src/llm/mod.rs:260` — extend the wire description:

```rust
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// Curated capability phrase used by `ToolSearchEngine` for keyword
    /// matching. 3-10 words, no trailing period. Prefer terms not
    /// already in the tool name (e.g. "jupyter" for `NotebookEdit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_hint: Option<String>,
}
```

`Tool::spec()` is the only construction site; the agent loop
`agent.rs:550` calls `tools.specs()` which collects via `spec()`.
The trait method `should_defer` does NOT need to be exposed via
`ToolSpec` — the partition happens in `ToolRegistry`, not in the
LLM wire shape.

### 3. Add `partition_by_eagerness` to `ToolRegistry`

`src/tools/mod.rs:147` — new method:

```rust
impl ToolRegistry {
    /// Split tools into `(eager, deferred)` per the policy:
    ///   - the `always_eager` whitelist (e.g. `ToolSearchTool`,
    ///     `read_file`, `run_shell`) goes to `eager` regardless
    ///     of `should_defer()`.
    ///   - tools with `should_defer() == true` go to `deferred`.
    ///   - everything else goes to `eager`.
    pub fn partition_by_eagerness(
        &self,
        always_eager: &[&str],
    ) -> (Vec<ToolSpec>, Vec<ToolSpec>) {
        // ...
    }
}
```

The `always_eager` whitelist is passed in by the caller (the agent
or the provider) — it is **not** hard-coded in the registry. The
kernel doesn't know which tools Anthropic considers "first-round
essentials"; that's a per-provider policy decision.

### 4. Extend `LlmProvider` with `complete_with_search`

`src/llm/mod.rs:297` — add a new trait method with a **default
implementation**:

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec])
        -> Result<Completion>;

    /// Variant that accepts a partition between eager and deferred
    /// tools. The default implementation concatenates them and
    /// calls `complete()` — i.e., it ignores the partition and
    /// behaves identically to the legacy interface. Providers that
    /// support deferred tool loading override this.
    async fn complete_with_search(
        &self,
        messages: &[Message],
        eager_tools: &[ToolSpec],
        deferred_tools: &[ToolSpec],
    ) -> Result<Completion> {
        let mut all = eager_tools.to_vec();
        all.extend_from_slice(deferred_tools);
        self.complete(messages, &all).await
    }

    async fn complete_structured(&self, _req: StructuredRequest)
        -> Result<Value> { ... }

    async fn stream(&self, ...) -> Result<Completion> { ... }
}
```

**Existing providers (OpenAI, MockProvider) need zero code change**
— the default impl gives them identical behavior to `complete()`.

### 5. Implement `complete_with_search` for `AnthropicProvider`

`src/llm/anthropic.rs` — override the new method:

- Add a `search_engine: Arc<dyn ToolSearchEngine>` field.
- Add a `with_search_engine(self, ...)` builder.
- Override `complete_with_search` to run a search-aware loop:
  1. Build request with `eager_tools` (eager) + `deferred_tools`
     (each marked with `defer_loading: true`) + the `ToolSearchTool`
     spec (eager).
  2. Send request.
  3. If the response contains a `tool_call` for `ToolSearchTool`:
     a. Call `search_engine.resolve(query, &deferred_tools)`.
     b. Append a `tool_result` message whose `content` is a JSON
        array of `{"type": "tool_reference", "tool_name": "..."}`
        objects.
     c. Send the augmented conversation back to the model (one
        extra round-trip).
     d. Recurse / loop until the model produces a non-search
        `tool_call` or a text response.
  4. If the response contains only normal `tool_calls` or text:
     return as-is.

The `ToolSearchTool` definition is hard-coded in
`AnthropicProvider` (its `prompt()` text mirrors the
`fake-cc` implementation). The `ToolSearchTool` tool is never
visible to the OpenAI provider — the kernel adds it to the eager
list only when the provider is Anthropic, decided at agent
construction.

### 6. New module: `src/llm/search.rs`

Port the keyword-search algorithm from `fake-cc` (`src/tools/ToolSearchTool/ToolSearchTool.ts:186-302`)
to Rust:

```rust
pub trait ToolSearchEngine: Send + Sync {
    fn resolve(&self, query: &str, specs: &[ToolSpec]) -> Vec<String>;
}

pub struct KeywordSearchEngine { /* description cache */ }

impl ToolSearchEngine for KeywordSearchEngine {
    fn resolve(&self, query: &str, specs: &[ToolSpec]) -> Vec<String> {
        // 1. fast path: query == tool.name (case-insensitive)
        // 2. mcp__server__tool prefix matching
        // 3. + prefix → required term
        // 4. weighted scoring: search_hint (+4), name part (+10),
        //    description word-boundary (+2)
        // 5. sort, top-N
    }
}
```

Unit tests in the same file:

- exact name match returns that tool
- keyword match ranks `searchHint`-bearing tools higher
- `+slack send` requires `slack`
- empty query returns empty
- `mcp__` prefix matching works

### 7. Kernel wiring in `agent.rs`

`src/agent.rs:550` — replace the single `specs()` call with the
partitioned call:

```rust
let (eager_specs, deferred_specs) = self
    .tools
    .partition_by_eagerness(&["ToolSearchTool", "read_file", "run_shell", "write_file"]);
```

`src/agent.rs:672` — switch the LLM call:

```rust
self.llm.complete_with_search(&self.messages, &eager_specs, &deferred_specs).await?
```

`src/agent.rs:670` (the streaming branch) — same change.

The agent loop body does NOT grow. The provider handles the
search-aware re-issue internally.

### 8. `OpenAiProvider` — explicitly documented as not implementing search

A doc comment in `src/llm/openai.rs` near the `impl LlmProvider`
block states: "OpenAI does not support deferred tool loading; this
provider uses the default `complete_with_search` implementation,
which is a no-op merge. To enable deferred tools, use the Anthropic
provider." No code change in this provider.

## Files to change

| File | Change |
|------|--------|
| `src/tools/mod.rs` | Add `Tool::should_defer()`; add `ToolRegistry::partition_by_eagerness()` |
| `src/llm/mod.rs` | Add `search_hint` to `ToolSpec`; add `complete_with_search` to `LlmProvider` |
| `src/llm/anthropic.rs` | Override `complete_with_search`; add `search_engine` field + builder; new helpers `build_request_with_partition`, `run_search_aware_loop`; hard-coded `ToolSearchTool` spec |
| `src/llm/openai.rs` | **No code change**; add doc comment explaining non-support |
| `src/llm/search.rs` (new) | `ToolSearchEngine` trait + `KeywordSearchEngine` impl + unit tests |
| `src/agent.rs` | Switch `complete` → `complete_with_search`; partition call at step start |

## Out of scope

- OpenAI `tool_search` / `defer_loading` (the user explicitly
  excluded this from g164).
- Server-side `tool_search_tool_*_20251119` (we use client-side).
- Bedrock / Vertex compatibility gates
  (`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` analog).
- "Tools added at runtime" mid-conversation delta mechanism
  (Anthropic's `deferred_tools_delta`). Existing tools are
  static for now.
- Pricing / token-cost impact of the search round-trip
  (the search call itself is a regular LLM call; cost is
  accounted for by the existing `usage` field).

## Acceptance

1. `cargo build` green
2. `cargo test --all-targets` green (existing tests untouched;
   new tests in `src/llm/search.rs` and `src/llm/anthropic.rs`)
3. `cargo fmt --all` clean
4. `cargo clippy --all-targets --all-features -- -D warnings` clean
5. `OpenAiProvider` and `MockProvider` use the default
   `complete_with_search`; behavior is byte-for-byte identical to
   the pre-g164 `complete()` (verified by existing test suite
   passing without modification)
6. With a mock Anthropic backend that emits a `tool_use` for
   `ToolSearchTool`, `AnthropicProvider::complete_with_search`
   issues a follow-up request and resolves the search
7. The `tool_reference` JSON shape in the `tool_result` content
   matches `fake-cc` byte-for-byte (verified by snapshot test)

## Notes for the agent

- The kernel's main loop (`agent.rs::Agent::run_inner`) MUST NOT
  gain a branch for "is this a search call?". The provider
  resolves the search internally. The kernel just calls
  `complete_with_search` and gets a `Completion` back. This
  preserves invariant 1 in `.dev/AGENTS.md` ("Agent loop stays
  small").
- Tests that build providers via `MockProvider` are unaffected
  by this goal — the mock keeps using `complete()`.
- `Tool::should_defer()` defaults to `false` — existing tool
  impls are not modified in this goal. A follow-up goal can
  mark `SubAgent`, `WebFetch`, memory/facts tools as
  `should_defer: true` once the search engine is verified.
- `KeywordSearchEngine::resolve` is intentionally a pure
  function (no I/O, no async) — same signature as the
  reference impl in fake-cc. This makes it easy to test.
- The `ToolSearchTool` spec's `prompt()` text is the user-facing
  description the model sees. It must explain: (a) the tool
  returns schemas for deferred tools, (b) once a tool's schema
  is in the result, it is callable like any other tool, (c) the
  query forms (`select:Name` and keyword).
- The `partition_by_eagerness` whitelist is passed by the agent,
  not hard-coded — this lets a future goal change the
  always-eager set without touching the registry.
- **Do not** modify any file under `.dev/` other than this goal
  (per `AGENTS.md`).
- **Do not** add any new dependency to `Cargo.toml`. The
  `KeywordSearchEngine` uses only `std` + existing deps.

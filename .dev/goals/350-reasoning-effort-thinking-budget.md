# Goal 350 — Wire `reasoning_effort` / `thinking_budget` end-to-end so thinking intensity is actually configurable

**Roadmap**: LLM Provider / Model Controls — the config surface already exists
but nothing reaches the wire.

**Design principle check**:
- Implemented as: provider-side knobs + config plumbing. `OpenAiProvider`
  / `AnthropicProvider` each gain `reasoning_effort` + `thinking_budget`
  fields (with builder setters); the two free `build_request` fns gain the
  params and emit each ecosystem's *native* knob; `Config` gains a
  `reasoning_effort` field; `--effort` sets both; the two HTTP request
  structs' already-declared knobs finally get applied.
- ✅ Keeps the two knobs in their native units — **no budget↔effort
  conversion**. OpenAI gets `reasoning_effort`; Anthropic gets
  `thinking: {type, budget_tokens}`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change the `ChatProvider` trait (no new trait methods, no
  signature changes to `complete`/`stream`).
- ❌ Does NOT change response-side parsing (`thinking` blocks,
  `reasoning_content`, `thinking_delta` already land in
  `Completion::reasoning_content` — Goal 237-era work).
- ❌ Does NOT add dependencies (invariant #6), does NOT `unwrap()` in
  non-test code (invariant #5).

## Why

Two facts make thinking intensity currently **unconfigurable**:

1. **The request bodies never carry a thinking knob.** `build_request` in
   `src/llm/openai.rs:898` emits `model / temperature / max_tokens /
   messages / tools` (+ the MiniMax-only `reasoning_split`, gated on the
   model name at line 936). `build_request` in `src/llm/anthropic.rs:687`
   emits `model / max_tokens / temperature / system / messages / tools`.
   No `reasoning_effort` (OpenAI o1/o3/GPT-5-family), no
   `thinking: {type:"enabled", budget_tokens:N}` / `{type:"disabled"}`
   (Anthropic extended thinking).

2. **The config surface is declared but dead.** `Config.thinking_budget`
   (`src/config.rs:63`) is written by `--effort` (`crates/recursive-cli/
   src/main.rs:568-575`: low→`Some(0)`, high→`Some(16000)`, normal→`None`)
   and declared in the HTTP API request structs
   (`src/http/mod.rs:142, 344` + OpenAPI `918, 957`) — but **no code ever
   reads it back**. `src/llm/` and `src/run_core.rs` contain zero
   references to it; the providers are built from Config at
   `crates/recursive-cli/src/cli/builder.rs:349-371` (and the duplicated
   sites at `main.rs:703-752, 1917-1926`,
   `crates/recursive-tui/src/runtime_builder.rs:39-47`) without it. The
   HTTP fields are likewise never applied in `src/http/handlers.rs`.

Why it went unnoticed: the primary stack (DeepSeek's Anthropic-compatible
endpoint, `deepseek-v4-flash`) produces thinking by default, so the
response side "just works" without any request param — documented in
`.dev/journal/manual-20260618-anthropic-thinking.md`, which explicitly
deferred this: "若后续接官方 Anthropic extended thinking（需
`thinking: {type:"enabled", budget_tokens}`、temperature=1 等约束），再在
`build_request` 里按需启用". This goal does exactly that, plus the OpenAI
side.

The two ecosystems expose *different* knobs (this is the whole point of the
asymmetry, do not fight it):
- OpenAI-compatible: `reasoning_effort: "low" | "medium" | "high"`
- Anthropic: `thinking: {type: "enabled", budget_tokens: N}` — and, when
  enabled, the API enforces `temperature == 1` and `max_tokens` must leave
  output headroom above the budget.

## Scope (do exactly this, no more)

### 1. `Config` — add `reasoning_effort`

In `src/config.rs`, right after `thinking_budget` (line 63), add:

```rust
/// Reasoning effort for OpenAI-compatible reasoning models (o1/o3/GPT-5
/// family and compatible gateways): "low" | "medium" | "high".
/// `None` = leave the request untouched (provider/model default).
/// Orthogonal to `thinking_budget` (Anthropic knob) — each provider
/// consumes only its native knob.
pub reasoning_effort: Option<String>,
```

- Add it to the `Debug` impl next to `thinking_budget` (line 153).
- Set `None` in **every** struct-literal construction of `Config` (the
  compiler will force this — current sites: `src/config.rs:611, 966,
  1671, 1818, 1868, 1956`; `src/multi.rs:585`;
  `crates/recursive-cli/src/cli/builder.rs:560`;
  `crates/recursive-cli/src/main.rs:2776`;
  `crates/recursive-tui/src/runtime_builder.rs:762`).
- Do NOT add an env var or config-file key in this goal — the CLI `--effort`
  flag (step 5) and HTTP API body (step 6) are the two setters.

### 2. Providers — store the knobs + builder setters

In `src/llm/openai.rs` (`OpenAiProvider`, struct at line 43) and
`src/llm/anthropic.rs` (`AnthropicProvider`, struct at line 27):

- Add two fields, mirroring the existing `max_tokens: u32` style:

```rust
/// OpenAI `reasoning_effort` ("low"/"medium"/"high") for reasoning models.
/// `None` = do not send the field.
reasoning_effort: Option<String>,
/// Anthropic extended-thinking budget. `Some(0)` = disabled;
/// `Some(n)` = `thinking: {type:"enabled", budget_tokens:n}`;
/// `None` = do not send a `thinking` block (model/gateway default).
thinking_budget: Option<u32>,
```

- Init both to `None` in each `new()`.
- Add builder setters right after `with_max_tokens`, mirroring its style:

```rust
pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
    self.reasoning_effort = effort;
    self
}
pub fn with_thinking_budget(mut self, budget: Option<u32>) -> Self {
    self.thinking_budget = budget;
    self
}
```

### 3. `build_request` — emit the native knob

Change both free `build_request` fns to accept the two new knobs and emit
**only the native one**. Signature (both files, symmetric):

```rust
fn build_request(
    model: &str,
    temperature: f64,
    max_tokens: u32,
    /* anthropic only: system: Option<&str>, */
    messages: &[Message],
    tools: &[ToolSpec],
    reasoning_effort: Option<&str>,
    thinking_budget: Option<u32>,
) -> Value
```

**`src/llm/openai.rs:898`** — after the `model_wants_reasoning_split`
block (line 936-938), add:

```rust
// Goal-350: OpenAI reasoning models (o1/o3/GPT-5 family) honour
// `reasoning_effort`. Opt-in only (`None` = untouched): some compatible
// gateways reject unknown fields, so never send it unless the user
// explicitly asked via --effort / the HTTP API. `thinking_budget` is the
// Anthropic knob — OpenAI has no token-budget equivalent, so it is
// intentionally ignored here (do NOT convert budget→effort).
if let Some(effort) = reasoning_effort {
    req["reasoning_effort"] = Value::String(effort.to_string());
}
```

**`src/llm/anthropic.rs:687`** — after the tools block (line 708-720), add:

```rust
// Goal-350: Anthropic extended thinking. `Some(0)` explicitly disables;
// `Some(n)` enables with a token budget. Enabled thinking FORCES
// temperature=1 (API 400 otherwise) and requires max_tokens to leave
// output headroom above the budget — enforce both here so callers cannot
// trip the constraint. `reasoning_effort` is the OpenAI knob — ignored.
if let Some(budget) = thinking_budget {
    if budget == 0 {
        req["thinking"] = serde_json::json!({ "type": "disabled" });
    } else {
        req["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
        req["temperature"] = Value::from(1.0);
        let min_max = budget.saturating_add(4096);
        if req["max_tokens"].as_u64().map(|m| m < min_max as u64).unwrap_or(false) {
            req["max_tokens"] = Value::from(min_max);
        }
    }
}
```

(No beta header in this goal — DeepSeek's Anthropic-compatible gateway
serves `thinking` without `anthropic-beta: extended-thinking-2025-01-15`;
add the header in a follow-up when an official-Anthropic user appears.
Leave a `// Goal-350:` comment noting that.)

### 4. Call sites — pass the knobs through

- `src/llm/openai.rs`: the three production calls at 224, 244, 543 pass
  `self.reasoning_effort.as_deref(), self.thinking_budget`; the test calls
  at 1628, 1640, 1652 pass `None, None` (or add a `None, None` arg).
- `src/llm/anthropic.rs`: the two production calls at 189, 230 pass
  `self.reasoning_effort.as_deref(), self.thinking_budget`; the test calls
  at 1153, 1167, 1185, 1733 pass `None, None`.
- Watch the arg order: `reasoning_effort` first, `thinking_budget` second,
  in both files (symmetry beats matching the struct's field order here).

### 5. Provider construction sites — wire Config in

Add `.with_reasoning_effort(config.reasoning_effort.clone())` and
`.with_thinking_budget(config.thinking_budget)` to **every** construction
chain (after the existing `.with_max_tokens(...)`), in:
- `crates/recursive-cli/src/cli/builder.rs:356-368`
- `crates/recursive-cli/src/main.rs:703-752` (4 arms: two per site × 2 sites)
- `crates/recursive-cli/src/main.rs:1917-1926`
- `crates/recursive-tui/src/runtime_builder.rs:39-47`

### 6. CLI `--effort` — set both knobs

In `crates/recursive-cli/src/main.rs:568-575`, extend the existing mapping
so the effort level also drives the OpenAI knob:

```rust
// --effort: map to BOTH native knobs (Goal-350). Anthropic budget
// (low=0 disables, normal=default, high=max) and OpenAI reasoning_effort
// (low/high; normal=default). Each provider consumes only its own.
if let Some(effort) = &cli.effort {
    config.thinking_budget = match effort.as_str() {
        "low" => Some(0),
        "high" => Some(16000),
        _ => None, // "normal" → leave as default
    };
    config.reasoning_effort = match effort.as_str() {
        "low" | "high" => Some(effort.clone()),
        _ => None,
    };
}
```

Update the flag's doc comment (main.rs:124) to mention OpenAI:
`/// Reasoning effort level: low (minimal/disabled thinking), normal (default), high (max). Sets reasoning_effort (OpenAI) and thinking budget (Anthropic).`

### 7. HTTP API — apply the declared knobs

The fields already exist in `src/http/mod.rs` (`thinking_budget` at 142 +
344, OpenAPI 918 + 957) but are never read. Finish the job:

- Add `reasoning_effort: Option<String>` to both request structs
  (right after `thinking_budget`) with a doc comment, and to both OpenAPI
  schemas (as `"reasoning_effort": { "type": "string", "nullable": true }`).
- In `src/http/handlers.rs`, both the run endpoint
  (`AgentRuntimeBuilder::new()` at ~121) and `create_session` (~247) build
  the runtime from the shared `state.provider` — a per-session knob needs a
  fresh provider. Add a small helper, e.g.:

```rust
/// Goal-350: rebuild the provider from `state.config` with per-request
/// thinking knobs. Falls back to the shared provider when no knob is set.
fn provider_with_overrides(
    state: &AppState,
    thinking_budget: Option<u32>,
    reasoning_effort: Option<&str>,
) -> Arc<dyn ChatProvider> {
    if thinking_budget.is_none() && reasoning_effort.is_none() {
        return state.provider.clone();
    }
    // Mirror the provider_type match from cli/builder.rs:349-371:
    // OpenAiProvider::new(...).with_temperature(config.temperature)
    //   .with_max_tokens(config.max_tokens)
    //   .with_reasoning_effort(reasoning_effort.map(String::from))
    //   .with_thinking_budget(thinking_budget)
    // ... / AnthropicProvider::new(...) likewise ...
}
```

  Use it in both handlers when building the runtime (`runtime.llm(
  provider_with_overrides(state, body.thinking_budget,
  body.reasoning_effort.as_deref()))` instead of `state.provider.clone()`).
  If the two handlers build the runtime through a shared helper, put the
  override there — one place, not two.
- **API key note:** the handler has `state.config`; if `require_api_key()`
  reads from env only, reuse the same api_key the server started with
  (`state.config.api_key` + `base_url` + `model`). Do not introduce a new
  key-resolution path.

### 8. Tests

Unit tests in the file that owns each behaviour (`#[cfg(test)] mod tests`):

`src/llm/openai.rs`:
- `build_request_includes_reasoning_effort_when_set` — `build_request(...,
  Some("high"), None)["reasoning_effort"] == "high"`.
- `build_request_omits_reasoning_effort_by_default` — `None` → key absent.
- `build_request_ignores_thinking_budget_on_openai` — `build_request(...,
  None, Some(12345))` has NO `thinking`/`budget_tokens` key and no
  `reasoning_effort` (pins the asymmetry — Anthropic's knob is a no-op
  here by design).

`src/llm/anthropic.rs`:
- `build_request_emits_thinking_enabled_with_budget` — `Some(8000)` →
  `thinking.type == "enabled"`, `thinking.budget_tokens == 8000`,
  `temperature == 1.0`.
- `build_request_thinking_enabled_guarantees_max_tokens_headroom` —
  with `max_tokens=4096, thinking_budget=8000` the emitted `max_tokens`
  is ≥ 8000+4096; with a large `max_tokens` it is unchanged.
- `build_request_emits_thinking_disabled_for_zero` — `Some(0)` →
  `thinking.type == "disabled"`, temperature NOT forced to 1.
- `build_request_omits_thinking_by_default` — `None` → no `thinking` key.
- `build_request_ignores_reasoning_effort_on_anthropic` — `Some("high")`
  → no `reasoning_effort` key.

`crates/recursive-cli/src/main.rs` (or wherever `--effort` mapping is
tested):
- `effort_flag_sets_both_config_knobs` — one test asserting low→
  `thinking_budget=Some(0)` + `reasoning_effort=Some("low")`, high→
  `Some(16000)` + `Some("high")`, normal→ both `None` (keep it ONE test —
  env/CLI-parse tests must not be split, see `.dev/AGENTS.md` "Env-var
  tests must be ONE test").

`src/http/handlers.rs` (mirror existing request tests):
- `session_request_with_thinking_knobs_builds_overridden_provider` — POST
  with `thinking_budget` + `reasoning_effort` → 2xx (or the existing
  success shape); at minimum assert the override branch is exercised
  (e.g. a run-request test that previously used the shared provider still
  passes when knobs are present). If the in-process server tests build a
  real provider, assert via a mock-endpoint or unit-test the
  `provider_with_overrides` fallback: no knobs → `state.provider.clone()`
  (same Arc), knobs → a different provider whose request body carries the
  knob (use a local mock HTTP server like the existing provider tests).

Also add the Config construction compile-check implicitly: every
`Config { ... }` literal must compile after step 1.

### 9. Journal

`.dev/journal/manual-<YYYYMMDD>-goal350-reasoning-effort.md` — note:
- the before state (both request builders silent; `thinking_budget` /
  `--effort` / HTTP fields declared but never read — zero references in
  `src/llm/`),
- the two native knobs and why there is deliberately **no** budget↔effort
  conversion,
- the Anthropic constraints enforced in `build_request` (temperature=1,
  max_tokens headroom) and the decision NOT to send the
  `anthropic-beta: extended-thinking-2025-01-15` header (DeepSeek gateway
  serves thinking without it; official-Anthropic follow-up),
- the opt-in risk note: `reasoning_effort` is only emitted when the user
  sets it — some OpenAI-compatible gateways reject unknown fields, so
  default (`None`) behavior is byte-identical to before.

## Acceptance

- `cargo test --workspace` green (incl. the new tests above).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all --check` clean.
- Headline tests
  `build_request_emits_thinking_enabled_with_budget`,
  `build_request_includes_reasoning_effort_when_set`, and
  `effort_flag_sets_both_config_knobs` all pass.
- Default behaviour unchanged: no `--effort`, no HTTP knob → request
  bodies are byte-identical to before this goal (pinned by the
  `*_by_default` tests).
- No change to `src/run_core.rs`, the `ChatProvider` trait, or any
  response-side parsing.
- No new dependencies; no `unwrap()` in non-test code.

## Notes for the agent

- **The asymmetry is the design.** OpenAI has `reasoning_effort` (an
  enumerated level); Anthropic has `thinking.budget_tokens` (a token
  count). Do NOT invent a conversion factor. Each provider consumes only
  its native knob; the *other* param is accepted for signature symmetry and
  documented as intentionally ignored — the two `*_ignores_*` tests pin
  this so nobody "fixes" it later.
- **`build_request` is a free function** shared by the production calls and
  the unit tests. Adding params ripples to ~6 test call sites in openai.rs
  and ~4 in anthropic.rs — update them all (`None, None`) in the same
  change or the build fails.
- **Anthropic thinking constraints are enforced in `build_request`, not at
  the call site** — the constraint is a property of the API, so the
  serialiser enforces it. `Some(0)` (disable) must NOT force temperature
  or touch max_tokens.
- **Opt-in only, by default nothing changes.** Both knobs are only emitted
  when the user sets them. The existing `--effort normal` / `None` path
  must produce byte-identical requests to today (that is the no-regression
  contract; DeepSeek keeps default-thinking via the untouched request).
- **`reasoning_effort` and unknown fields:** some OpenAI-compatible
  gateways reject unknown request fields (the `reasoning_split` comment at
  openai.rs:934 documents this). `reasoning_effort` is emitted only when
  the user explicitly opted in via `--effort`/HTTP — if their gateway
  rejects it, that is their opt-in choice, not a default-behaviour
  regression. Do not add a model-name allowlist in this goal (brittle);
  the doc comment should say so.
- **HTTP override rebuilds the provider** — do not mutate
  `state.provider` (it is `Arc<dyn ChatProvider>`, shared). Build a fresh
  concrete provider from `state.config` exactly like
  `crates/recursive-cli/src/cli/builder.rs:349-371` does; reuse
  `state.config.api_key`/`base_url`/`model`. Keep the no-knob fast path
  (`state.provider.clone()`) so the shared provider stays untouched.
- **Reference for the deferred thinking note:**
  `.dev/journal/manual-20260618-anthropic-thinking.md` (the "Notes" section
  explicitly deferred this work to `build_request`).
- **`git` discipline:** this goal lands in the `recursive` sub-repo
  (`git -C recursive ...`), not the infra4agent monorepo root. Commit in
  the sub-repo. Never run `git` against the working tree yourself; the
  flow owns rollback. Do not edit `AGENTS.md`, `README.md`, or `.dev/`
  files other than your own journal entry unless the flow instructs you.
- Follow `.dev/AGENTS.md` "How to do work" to the letter: V4A patch
  format, four green gates (`cargo build/test/fmt/clippy`), no shell
  rewrite of `src/` files.

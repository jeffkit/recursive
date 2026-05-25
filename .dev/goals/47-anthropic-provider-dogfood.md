# Goal 47 — Anthropic Provider end-to-end dogfooding

> **Roadmap**: NOT a new feature — this is **dogfooding g34**
> (anthropic-provider, landed batch-12, never actually used in
> self-improve cycles). Adds a profile + selector so AnthropicProvider
> runs against a real backend (MiniMax or DeepSeek both expose the
> Anthropic Messages API).
> **Design principle check**: orthogonal — adds a provider selector
> in `main.rs` based on an env var. Default = OpenAI provider
> (existing behavior). Pluggable — opt-in via env. Testable — a
> Mock-backed Anthropic test plus an integration smoke that lives
> in `tests/`.

## What

1. **Add a provider-type selector in `main.rs`**: when
   `RECURSIVE_PROVIDER_TYPE=anthropic` (default `openai`), construct
   `AnthropicProvider` instead of `OpenAiProvider`. The
   `--api-base` and `--api-key` flags work for both.

2. **Add an anthropic profile in `self-improve.sh`**:

   ```bash
   anthropic-minimax)
     export RECURSIVE_PROVIDER_TYPE="anthropic"
     export RECURSIVE_API_BASE="https://api.minimaxi.com/anthropic"
     export RECURSIVE_MODEL="MiniMax-M2"
     export RECURSIVE_API_KEY="${MINIMAX_API_KEY:-}"
   ;;
   ```

   Mirror for `anthropic-deepseek`. **Important**: the actual API
   base URLs need to be confirmed against the providers' docs —
   the agent should check `web_fetch` or model documentation if
   the values above are not the canonical paths.

3. **Add an integration test** in `tests/anthropic_smoke.rs` that
   verifies an end-to-end call works against the Mock-like setup
   `LlmProvider` plumbing for AnthropicProvider. (NOT a network
   test — those need explicit network feature flags.)

## Why

g34 landed 4 batches ago and **has never actually run** in a
self-improve cycle. Static unit tests cover the request body shape
and the response parser, but not the agent-loop integration. If
there's a bug in how `Agent::run` dispatches against `LlmProvider`
when the provider is `AnthropicProvider` vs `OpenAiProvider`, it'll
surface here.

Also: having a real Anthropic profile widens the self-improve
provider rotation pool from 3 to 4-5, which is genuine value.

## Tests

- `anthropic_smoke_constructs_with_minimum_config` — assert
  `AnthropicProvider::new(...)` succeeds with realistic-shape args.
- `anthropic_provider_selector_main_no_panic` — extend the existing
  `build_agent_does_not_panic_*` regression tests with a
  variant that constructs with `RECURSIVE_PROVIDER_TYPE=anthropic`.
- `anthropic_full_agent_loop_with_mock_provider` — wire a stub
  Anthropic-shaped backend through the agent loop (or extend
  MockProvider to be provider-agnostic) and assert one round-trip
  works.

## Wiring

- `src/main.rs`: add provider-type branch in `build_agent_seeded`.
  Approx 15 LOC.
- `.dev/scripts/self-improve.sh`: add 2 new profile cases. Approx
  16 LOC.
- `tests/anthropic_smoke.rs` (new): smoke test.

## Acceptance

- `cargo build` green.
- `cargo test` green; +3 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- Existing behavior preserved when `RECURSIVE_PROVIDER_TYPE` is
  unset or `openai`.
- `self-improve.sh anthropic-minimax` profile applies without error
  (might fail to actually run if the API base URL is wrong — that's
  a separate "verify after landing" step the orchestrator will do).

## Out of scope (defer)

- Anthropic-native features like prompt-caching breakpoints. Use
  whatever the existing AnthropicProvider already implements.
- Streaming for Anthropic (separate goal after streaming-sse is
  generalized).
- Anthropic structured output (separate goal — the trait default
  errors for now).

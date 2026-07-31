Date: 2026-07-31
Goal: Manual fix for the `provider_stop:length` failure blocking Goal 349 (and any reasoning-heavy self-improve run)
Files touched:
  - src/llm/mod.rs            (new `DEFAULT_MAX_TOKENS` const + doc)
  - src/llm/openai.rs         (default 16384 → DEFAULT_MAX_TOKENS; fixed stale V3 comment)
  - src/llm/anthropic.rs      (default 4096 → DEFAULT_MAX_TOKENS)
  - src/config.rs             (`Config.max_tokens` field; three-tier resolution in `from_env`; 3 unit tests)
  - src/config_file.rs        (`AgentSection.max_tokens: Option<u32>`)
  - src/providers.rs          (`ModelSpec.max_tokens: Option<u32>`)
  - src/providers_cache.rs, src/multi.rs (test Config constructions)
  - crates/recursive-cli/src/{main.rs, cli/builder.rs, cli/init.rs}
  - crates/recursive-tui/src/{runtime_builder.rs, commands.rs, ollama_probe.rs}
  - tests/{v050_integration.rs, agui_e2e.rs, http_common/mod.rs}
Tests added: 3 (max_tokens_defaults_to_crate_default, max_tokens_env_var_overrides_default, max_tokens_default_is_not_the_old_16384)

## What happened

Goal 349 (TUI select & copy) ran twice via the self-improve flow
(`deepseek-v4-flash`), both times ending `verdict=skip-commit, files_changed=0`.
Root cause — confirmed by reading the saved transcript — was NOT context-window
exhaustion and NOT a crash. On the final turn the assistant emitted
`content=""` (no tool call) but `reasoning_content` of ~60K chars, and the run
terminated with `finishReason=provider_stop:length`. The agent had spent its
entire per-turn output budget reasoning and was truncated before emitting any
edit.

## Root cause

`src/llm/openai.rs` hard-coded `max_tokens: 16384` (with a stale comment
claiming "DeepSeek ceiling 8192 for v3"). DeepSeek V4 actually supports up to
**384K output tokens** (flash == pro), so 16384 was needlessly small — a
reasoning turn of ~15-20K tokens hit the cap. The cap was also not
configurable: `with_max_tokens()` existed but no env var or config field fed it,
and none of the 5 OpenAiProvider construction sites called it. `AnthropicProvider`
was even smaller (4096).

This is why `provider_stop:length` didn't auto-resume: the flow only auto-resumes
on `BudgetExceeded`, and the cap was the client-side `max_tokens`, not the
agent's step budget.

## The fix

1. New `crate::llm::DEFAULT_MAX_TOKENS = 65_536` (chosen to align with Claude
   Code's `ESCALATED_MAX_TOKENS`; generous for reasoning yet far below V4's
   384K real ceiling). Both OpenAI and Anthropic providers seed from it.
2. `Config.max_tokens: u32` resolved in `from_env` with three-tier precedence
   (user's requested design):
   - `RECURSIVE_MAX_TOKENS` env var (highest)
   - active preset's `ModelSpec.max_tokens` for the configured model, else the
     config file's `agent.max_tokens`
   - `DEFAULT_MAX_TOKENS` (final fallback)
3. `ModelSpec.max_tokens` and `AgentSection.max_tokens` added as `Option<u32>`
   (serde-optional, so existing `providers.toml` / config files are unaffected).
4. All 10 provider construction sites (5 OpenAi + 5 Anthropic across cli/tui)
   now call `.with_max_tokens(config.max_tokens)`.

## Verification

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace --lib`: recursive-tui 778 + tui-pty-harness 6, 0 fail
- 3 new unit tests pin the three-tier resolution + guard against a 16384
  regression.

## Notes

- We deliberately did NOT port FakeCC's full model-tier table or its
  "cap-low-then-escalate-on-hit" machinery (`getModelMaxOutputTokens` /
  `max_output_tokens_escalate`). That's a larger design; this change is the
  minimal "make it configurable + pick a sane default" fix that unblocks
  reasoning-heavy runs. The `ModelSpec.max_tokens` field leaves the door open
  for per-model tuning later.
- Research sources: DeepSeek API docs (V4 max output 384K), FakeCC
  `src/utils/context.ts` + `src/query.ts` (32K default / 64K escalate), Codex
  (~128K), GPT-4.1 (32768).

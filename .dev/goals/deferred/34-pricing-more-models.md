# Goal 34 — Extend `pricing_for` to cover GPT-4o, Claude, GLM-4.5

## Why

`pricing_for(model)` in `src/llm/mod.rs` currently knows about
MiniMax-M2, deepseek-chat, glm-4-flash, and glm-5.1 (placeholder).
Anyone setting `RECURSIVE_MODEL=gpt-4o-mini` (the doc-default) or
`claude-3-5-sonnet-latest` (a likely user) gets `(no pricing)` in
their cost line — meaning the agent's own observation pipeline
silently undercounts cost when users self-host on those models.

Add static pricing entries for:
- `gpt-4o-mini` (OpenAI)
- `gpt-4o` (OpenAI)
- `claude-3-5-sonnet-20241022` and `claude-sonnet-4-5` (Anthropic)
- `glm-4.5` (Zhipu — current generation alongside the placeholder
  glm-5.1)

with the public per-million-token rates as of 2026-05. Match the
existing `ModelPricing` shape (`input_per_million`,
`output_per_million`).

## Scope

Touches: `src/llm/mod.rs` only (plus tests in the same file).

1. In `pricing_for()`:
   - Add new match arms for the 5 models above. Use these rates
     (as of 2026-05; treat as approximate, link source in comment):
     - `gpt-4o-mini`: input 0.15, output 0.60
     - `gpt-4o`: input 2.50, output 10.00
     - `claude-3-5-sonnet-20241022`: input 3.00, output 15.00
     - `claude-sonnet-4-5`: input 3.00, output 15.00
     - `glm-4.5`: input 0.50, output 2.00 (placeholder — same as
       glm-5.1; calibrate later)
   - Comment above the new arms: "Public list rates as of 2026-05;
     prompt cache discounts not modeled here."

2. Tests in the same file:
   - **Test A**: `pricing_for("gpt-4o-mini")` returns Some with the
     expected input/output rates.
   - **Test B**: `pricing_for("nonexistent-model")` still returns
     None (regression).

## Acceptance

- `cargo build` green.
- `cargo test` green (138 baseline + 2 new = 140).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/llm/mod.rs`. Don't touch
  `openai.rs`, `agent.rs`, `main.rs`.
- The existing `pricing_for` match is the only anchor you need —
  add new arms before the catch-all `_ => None`.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- This is the simplest goal of batch 12. Don't overthink it.

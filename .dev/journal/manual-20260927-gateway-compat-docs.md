# manual-20260927-gateway-compat-docs

- Date: 2026-09-27
- Goal: Issue #34 — collect #15/#16/#17 implicit wire-protocol constraints into
  `docs/llm-gateway-compat.md` and link it from README (pure documentation).
- Files touched: `docs/llm-gateway-compat.md` (new), `README.md` (Docs index
  section before License).
- Tests added: none (pure markdown; per plan, no pseudo-tests).
- Notes: all constraint details cite code anchors
  (src/llm/anthropic.rs:811 sanitize_input_schema; src/llm/openai.rs:1050-1056
  serialize_message content:null; src/config.rs:236 resolve_max_tokens) plus
  regression-test names — no re-derivation. Gateway triage checklists for
  new-api/one-api and Bedrock are new content distilled from the #15/#16/#17
  fixes (commits a682400 / 6ed34d3 / 98ceb4c). Verified via the plan's grep
  checks; no cargo runs needed (zero product-code change).

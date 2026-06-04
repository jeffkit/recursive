# Manual edit: WebSearch tool

**Date**: 2026-06-04
**Goal**: Add a `WebSearch` tool that lets the agent search the web using configurable providers (brave, tavily, serper, bocha, bing).
**Files touched**:
- `src/tools/web_search.rs` — new tool implementation
- `src/tools/mod.rs` — module declaration + pub re-export + build_standard_tools registration (under `#[cfg(feature = "web_search")]`)
- `src/cli/builder.rs` — import + registration in `build_tools`
- `Cargo.toml` — added `web_search = []` feature, included in `default`

**Tests added**:
- `provider_from_str_recognises_all` — all five provider names parse correctly (case-insensitive)
- `format_results_empty` — empty result set returns "No results found."
- `format_results_numbered` — numbered list format with title/URL/summary fields
- `returns_unconfigured_message_when_no_env` — spec name and description verified
- `rejects_empty_query` — BadToolArgs on empty string
- `rejects_missing_query` — BadToolArgs on absent field
- `num_results_clamped_to_max` — clamping logic verification
- `load_config_returns_none_without_env` — graceful unconfigured path

**Design decisions**:
- Single tool, provider selected via `RECURSIVE_WEB_SEARCH_PROVIDER` env var
- Returns lightweight `title + URL + summary` list (no auto-fetch of page content)
- Graceful unconfigured path: returns human-readable message instead of error
- No new Cargo dependencies — reuses existing `reqwest` + `serde_json`
- Feature-gated as `web_search = []`, included in `default` features
- `side_effect_class` = `ReadOnly` (outbound HTTP GET, no local mutations)

**Notes**:
- Bocha API endpoint: `https://api.bochaai.com/v1/web-search` (国内友好)
- Bing uses Azure Cognitive Services endpoint with `Ocp-Apim-Subscription-Key` header
- `is_deferred` not yet in this branch's Tool trait — will be wired when goal-100 lands

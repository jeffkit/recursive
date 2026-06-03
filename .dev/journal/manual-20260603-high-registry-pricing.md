# Manual edit: high-registry-pricing

**Date**: 2026-06-03
**Goal**: Fix H2 (ToolRegistry per-turn clone), H3 (three pricing tables), H4 (hand-rolled YAML pricing parser)
**Files touched**:
- `providers.toml` — added optional `pricing` field to every model entry
- `src/providers.rs` — added `ModelPricingSpec`, `find_model_pricing()`
- `src/llm/mod.rs` — replaced `pricing_for()` hardcoded match + `load_pricing_from_yaml` with providers.toml lookup; removed stale YAML tests
- `src/cost.rs` — removed `external_pricing` parameter from `CostTracker::new()`
- `src/tui/cost.rs` — removed `default_pricing_table()`, simplified `estimate_cost()` to 3-arg
- `src/tui/app/mod.rs` — removed `pricing: HashMap` field from `App`
- `src/tui/app/state.rs` — updated tests for new `estimate_cost` signature
- `src/tui/ui/status.rs` — removed `&app.pricing` arg
- `src/tui/ui/modal.rs` — replaced HashMap lookup with `crate::llm::pricing_for()`
- `src/cli/output.rs` — removed `external_pricing` from `print_usage()`
- `src/cli/resume.rs` — removed `external_pricing` from `cmd_resume` / `run_resumed`
- `src/main.rs` — removed `--pricing-file` CLI flag and all `external_pricing` propagation
- `src/run_core.rs` — changed `RunCore.tools: ToolRegistry` → `Arc<ToolRegistry>`; parallel batch spawns use `Arc::clone` (O(1)) instead of BTreeMap clone
- `src/kernel.rs` — `kernel.run()` wraps `self.tools.clone()` in `Arc::new()`

**Tests added**: updated `pricing_for_known_models` to use MiniMax-M3 (M2 no longer in providers.toml)
**Notes**:
- H3+H4: pricing is now single-source-of-truth in `providers.toml`; Ollama local models omit `pricing` field (Option)
- H2: `AgentKernel.tools` stays `ToolRegistry` to allow builder-time and between-turn mutation via `tools_mut()`; only `RunCore.tools` becomes `Arc<ToolRegistry>`, eliminating repeated BTreeMap clones inside parallel tool dispatch batches

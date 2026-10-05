# Journal — Issue #94 — mid-run token/cost budgets (`max_budget_usd`, `thinking_budget`, conservative step/wall defaults)

- **Date**: 2026-10-05
- **Goal**: issue #94 (gap sheet, P1) — `max_budget_usd` / `thinking_budget` were
  dead fields; `max_steps` / `wall_timeout_secs` defaulted to `0` = unlimited,
  so only the transcript char cap bounded a runaway turn.
- **Branch**: pipeline worktree for the `#94` goal.
- **Baseline**: `9aafb9e2` (issue text quotes `9165b9a0`; the gap was still open
  at the newer HEAD — `grep max_budget_usd src/run_core.rs` was empty).

## Files touched

| File | Change |
|------|--------|
| `src/config.rs` | `DEFAULT_MAX_STEPS = 200`, `DEFAULT_WALL_TIMEOUT_SECS = 3600` (both env/flag overridable, explicit `0` = unlimited); new `RECURSIVE_THINKING_BUDGET` and `RECURSIVE_MAX_BUDGET_USD` parsers feed `Config::thinking_budget` / `Config::max_budget_usd` (previously only settable from CLI flags, never from env). Struct docs rewritten to describe actual behaviour. |
| `src/run_core.rs` | New `CostBudget { limit_usd, pricing }` guard. `RunCore` gained `cost_budget: Option<CostBudget>`; `process_tool_results` now receives `total_usage` and, after every tool batch is fully paired, calls `cost_budget_finish` → ends the turn with `FinishReason::BudgetExceeded` before the next LLM call. Unpriced models degrade to a token ceiling at `FALLBACK_USD_PER_MILLION_TOKENS = 10.0` ($10/M — pessimistic). |
| `src/kernel.rs` | `AgentKernel` + `AgentKernelBuilder` carry `max_budget_usd` / `budget_pricing`; `AgentKernelBuilder::cost_budget(ceiling, pricing)`; `AgentKernel::run` builds the `CostBudget` for the `RunCore`. |
| `src/runtime/builder.rs` | `AgentRuntimeBuilder::cost_budget(...)` forwarder (same pattern as `wall_timeout_secs`). |
| `src/llm/anthropic.rs` | New `thinking_budget` field + `with_thinking_budget()`; `apply_thinking_budget()` injects `thinking = {type: "enabled", budget_tokens: n}`, drops `temperature` (the Messages API only accepts 1 alongside thinking) and raises `max_tokens` above the budget when needed. New `request_body()` helper means the streaming and non-streaming paths cannot diverge. |
| `src/llm/factory.rs` (**new**) | `build_llm_provider(config, api_key, retry, max_search_rounds, thinking_budget)` moved into the library — the HTTP layer needs to build a provider *per request* for a per-request thinking budget, and the Anthropic/OpenAI arms must not drift between frontends. |
| `src/llm/mod.rs` | Declares/re-exports `factory::build_llm_provider`. |
| `crates/recursive-cli/src/cli/builder.rs` | `build_llm_provider` now delegates to the library (passing `config.thinking_budget`); the agent/REPL/resume runtime path chains `.cost_budget(config.max_budget_usd, pricing_for(&config.model))`. |
| `crates/recursive-cli/src/main.rs` | Loop-mode builder gets the same `.cost_budget(...)`; `--max-budget-usd` now also reads `RECURSIVE_MAX_BUDGET_USD`; flag help no longer claims "only checked after each completed turn". |
| `src/http/mod.rs` | New `SessionOverrides { max_budget_usd, thinking_budget }` (serde + `Default`); request-field docs rewritten to state the *per-turn* semantics (the old "Agent stops after any turn that would exceed this limit" was written by nobody who had wired it). |
| `src/http/handlers.rs` | `build_session_runtime` takes `SessionOverrides`; `provider_for_request()` builds a per-request Anthropic provider when `thinking_budget` differs from the server default; `POST /run` + `POST /sessions` pass body values through. |
| `src/http/cold_load.rs` | `SessionMeta.overrides` (serde-defaulted) so a cold-loaded session keeps the budget its creator asked for — same drift class #98 fixed. |
| `README.md`, `website/{en,zh}/guide/config.md` | Env tables: new defaults (200 / 3600), the two new knobs, and the per-request HTTP fields. |

## Design notes / decisions

- **Why per-turn, not per-session.** `RunCore` owns the accumulation for one
  turn and is already where wall-clock/step/transcript budgets live; the
  issue's stated risk ("模型打环时**一个 turn** 可无感知烧掉数美元") is
  per-turn. Cross-turn accumulation would have to live in `AgentRuntime`
  (new state + a check in the turn entry path). The HTTP request docs now say
  "per turn" explicitly instead of pretending to cap a session.
- **Check placement.** The check runs at the step wrap-up inside
  `process_tool_results` (0 new lines in `run_inner`, which is at 146/150 of
  the invariant-#1 budget) and *after* the tool-result loop, so invariant #8
  (tool-call ↔ tool-result pairing) still holds when the turn ends early.
  The ceiling is therefore enforced at the first step boundary at or past it —
  `usage ≤ ceiling` holds for the reported usage because the crossing step is
  the last one charged.
- **Unpriced models.** `CostBudget::spend_usd` falls back to
  `tokens × $10/M` when `pricing_for(model)` returns `None`, so a custom
  gateway model still gets a bound rather than silently unbounded spend.
- **`thinking_budget` in HTTP.** The budget is a per-request Anthropic body
  field, so a request-level value needs its own provider. Rather than
  duplicating the provider match in `src/http`, the CLI's
  `build_llm_provider` moved to `src/llm/factory.rs`; the CLI keeps its
  signature (zero call-site churn) and HTTP builds one only when the request
  differs from the server default (otherwise it reuses `AppState::provider`).
- **`Some(0)` vs `None` for thinking.** `None` = model default, `Some(0)` =
  explicitly disabled — preserved on the wire by leaving the body untouched in
  both cases (Anthropic's default is thinking-off), which also keeps
  `--effort normal` behaviour identical.
- **Defaults.** `max_steps` 200 matches `.dev/OPERATIONS.md` ("Default
  `RECURSIVE_MAX_STEPS=200`", auto-resume to 400) and finally agrees with the
  root `AGENTS.md` budget line; the stale website default of `32` is fixed.
  `wall_timeout_secs` 3600 is deliberately roomier than the HTTP session
  default (1800) so it does not fire before the step ceiling on slow
  providers. Both keep `0` = unlimited, the documented escape hatch for long
  `recursive loop` sessions (the HANDOFF warning about hidden caps is honoured:
  the cap is visible, documented, and env-revertible).
- **TUI not wired.** `crates/recursive-tui` keeps its current behaviour (it has
  no budget flag and a human watching); touching it would trigger the flow's
  `tui-mutants` hard gate for a knob that surface cannot set today.

## Tests added

- `src/run_core.rs`: `cost_budget_disabled_for_absent_or_non_positive_ceiling`,
  `cost_budget_uses_pricing_when_the_model_is_priced`,
  `cost_budget_degrades_to_a_token_ceiling_when_the_model_is_unpriced`,
  `run_inner_stops_with_budget_exceeded_at_the_usd_ceiling` (3 steps of a
  4-step script consumed, `spent == ceiling`, transcript preserved).
- `tests/invariants/finish_reason_data.rs`:
  `cost_budget_stops_the_run_mid_goal_with_budget_exceeded` — the acceptance
  assertion end-to-end through `AgentRuntime::run` (invariant #7: data, not
  `Err`; transcript kept; `spent ≤ budget`).
- `src/kernel/tests.rs`: budget defaults to unbudgeted, `cost_budget`
  forwards ceiling + rate card, configured ceiling yields a guard.
- `src/runtime/tests.rs`: `cost_budget` forwarding + default `None`.
- `src/llm/anthropic.rs`: absent/zero leaves the body untouched; enabling drops
  `temperature`; `max_tokens` is raised above the budget; provider-level
  `request_body` carries the configured budget.
- `src/llm/factory.rs`: both provider arms build (thinking budget + search
  round cap), unknown provider type falls back to the OpenAI arm, Anthropic
  deferred-tool support still follows the endpoint.
- `src/config.rs`: finite default + explicit-`0` escape + explicit-N override;
  the two new env knobs (absent / `0` / set); malformed `RECURSIVE_MAX_BUDGET_USD`
  is a startup error.

## Gates

- `cargo test --workspace`: **all targets ok, 0 failed** (lib: 2662 passed;
  `tests/invariants` incl. the new budget test; TUI/CLI/AG-UI targets green).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean
  (13m 20s under CPU contention with the sibling pipeline-93 build — the flow's
  `cli-mutants` gate will be long for the same reason as
  `.dev/AGENTS.md` failure mode #8, since `crates/recursive-cli/src/main.rs` is
  one of the changed files).
- `cargo fmt --all --check`: clean.
- No `crates/recursive-tui/src/` change, so the TUI gates do not apply.

## Post-hoc checks (no test can see these)

- `process_tool_results` has exactly one production call site
  (`run_core.rs:1606`) and it passes the accumulated `total_usage`; the only
  other exit from the tool-call path is `handle_no_tool_calls`, which ends the
  turn — so a looping model cannot reach a second LLM call without passing the
  cost check.
- `build_request(` has exactly one production call site (inside `request_body`,
  `anthropic.rs:127`); every other occurrence is in the `#[cfg(test)]` module
  that starts at `anthropic.rs:1126` — thinking cannot apply to one wire path
  only.
- `RunCore::run_inner` body: 147 lines (invariant #1 limit 150).
- `src/llm/` has no production `use crate::tools` (invariant #2); the only
  occurrence is inside `#[cfg(test)]`.

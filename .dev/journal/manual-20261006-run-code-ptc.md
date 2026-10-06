# manual-20261006-run-code-ptc

- **Date:** 2026-10-06
- **Goal:** #134 `feat(exec)` — programmatic tool calling (`run_code` / PTC):
  one program replaces many ReAct tool round-trips. Borrows the DSH
  `ptc-runtime` execution seam. Baseline HEAD `4e1aee98`.
- **Depends on:** #127 (preset tier) — landed; the tool is mounted through
  `preset::ToolProfile::run_code`.

## Files touched

- `src/tools/execution/run_code/mod.rs` (new) — the `RunCode` tool: spec,
  `execute`, registry-backed `ToolInvoker`. Rejects an illegal binding name
  *before* spawning anything.
- `src/tools/execution/run_code/bindings.rs` (new) — portable binding names
  (`PORTABLE_RESERVED_WORDS` = ECMAScript ∪ Python, `RESERVED_BINDING_GLOBALS`,
  `RESERVED_ERROR_MEMBERS`) + `BindingTable` pre-run validation.
- `src/tools/execution/run_code/ledger.rs` (new) — ordered output ledger with
  UTF-8-safe truncation (log lines + completion value share one budget).
- `src/tools/execution/run_code/protocol.rs` (new) — stdout/stdin JSON-line
  protocol, the orthogonal failure taxonomy, `SandboxFacts` (reported
  separately from success/failure) and `RunReport::render`.
- `src/tools/execution/run_code/runner.rs` (new) — fresh-process driver: three
  budgets (wall clock 120 s / cap 600 s, output 2 MiB — the tool layer's hard
  cap, heap 512 MiB), scrubbed env, abort token, failure classification.
- `src/tools/execution/run_code/bootstrap.js` (new) — embedded Node bootstrap:
  console shim (5 methods), stdout funnel, binding functions, protocol loop.
- `src/tools/execution/mod.rs` — declare + re-export `run_code`.
- `src/tools/mod.rs` — `pub use execution::run_code`.
- `src/preset.rs` — `ToolProfile::run_code` (default **off**), the
  `programmatic-tool-calling` capability row (toggle `RECURSIVE_RUN_CODE`),
  `PresetEnv::run_code` + `resolve_tools` env overlay, `apply` wiring.
- `src/runtime/builder.rs` — `with_run_code` flag; `build()` registers
  `RunCode` over a shared `clone()` of the session registry, only when the
  transport executes on the host (presence-guarded by `surface_filtered`,
  like the plan-mode tools).

> Superseded in part by `manual-20261006-run-code-ptc-review.md` (review
> round: transport gate, portable-name skipping, shared session state,
> bounded line reads, 2 MiB output budget, empty-env toggle).

## Tests added

- `bindings`: portable identifier / reserved word / runtime global / duplicate
  rejection, and "one bad name rejects the whole table" (acceptance 3).
- `ledger`: ordered truncation, straddling fragment, UTF-8 safety, zero budget.
- `protocol`: event parsing, classification precedence, heap-OOM detection,
  stable status strings, render (sandbox facts vs failure, completion value).
- `runner`: documented default budgets, timeout clamp, allowed-env set,
  failure descriptions.
- `run_code` (integration, gated on a `node` on `PATH`): ≥5-binding
  aggregation in one run; a throwing binding rejects the awaited call; the
  **timeout**, **output** and **heap** budgets each trip and are classified
  `timeout` / `output-limit` / `heap-limit` (acceptance 2); an illegal binding
  name is rejected pre-run and consumes no execution (acceptance 3).
- `preset`: `run_code` off by default, env override both ways, capability row
  present (off-by-default discoverability).

## Notes

- **No new dependencies.** The program language is JavaScript executed by a
  `node` subprocess resolved from `PATH` (override with
  `RECURSIVE_RUN_CODE_NODE`); a "fresh process + empty env" contract is exactly
  what DSH's `ptc-runtime-node` does, so no engine is embedded. A missing
  runtime is reported as the `sandbox-unavailable` *fact*, not an error.
- Programmatic tool calls go back through `ToolRegistry::invoke`, so they hit
  the same permission pipeline / audit path as a model-issued call.
- The default `standard` preset has `run_code: false`; mount it with
  `RECURSIVE_RUN_CODE=1` (or a preset that declares `run_code: true`).
- Acceptance 1 (token / wall-clock delta on a batch task) is measured by the
  benchmark in issue #129, not here.

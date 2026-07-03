# Manual edit: P0 architecture fixes (docs sync, sink side-effect, http.rs split)

**Date**: 2026-07-03
**Goal**: Land the three P0 fixes called out in the architecture review:
- P0-1: Sync user-facing docs (CLAUDE.md, README.md, website/) with the
  post-Goal-219 file layout — `src/agent.rs` no longer exists; the
  ReAct loop lives in `src/run_core.rs::RunCore::run_inner` and the
  kernel/wrapper split lives in `src/kernel.rs` + `src/runtime.rs`.
- P0-2: Make the implicit side effect of `AgentRuntime::set_event_sink`
  (re-registers TodoWriteTool + ExitPlanModeTool) explicit in the API
  surface, by documenting the side effect on the existing method and
  adding a sibling `replace_event_sink` for callers that want a pure
  sink swap.
- P0-3: Extract the shared fixtures (mock_config, sample_state,
  sample_state_with_provider, SET_INSECURE_OK) from `tests/http.rs`
  into `tests/http_common/mod.rs` so the fixtures live next to the
  test bodies without being intermingled, and prime the ground for a
  future per-feature-area split.

**Files touched**:
- `CLAUDE.md` — replaced `src/agent.rs` references with the real
  kernel/runtime files; added a brief Goal-219 pointer.
- `README.md` — replaced the outdated "five concepts" table with the
  current `ChatProvider` / `AgentKernel` / `AgentRuntime` / `AgentEvent`
  layout; linked to `docs/architecture/agent-loop.md`.
- `website/{en,zh}/guide/quickstart.md` — changed example commands
  from `src/agent.rs` to `src/kernel.rs` so they don't refer to a
  file that doesn't exist.
- `website/zh/library/multi-agent.md` — same fix on a memory key
  example.
- `src/runtime.rs` — added a long doc-comment on `set_event_sink`
  documenting its re-registration side effect (load-bearing for
  CLI per-turn / HTTP per-session / TUI backend-init swap flows), and
  added a sibling `replace_event_sink` for the no-side-effect path.
  Two unit tests pin both contracts (no tool mutation on
  `replace_event_sink`; TodoWriteTool identity changes on
  `set_event_sink`).
- `tests/http.rs` — removed the inlined `mock_config` /
  `sample_state` / `sample_state_with_provider` / `SET_INSECURE_OK`
  definitions; declared `mod common` via `#[path]` pointing at the
  new `tests/http_common/mod.rs` and `use common::{...}` to bring
  the names into scope.
- `tests/http_common/mod.rs` (new) — holds the three fixtures and
  the `SET_INSECURE_OK` `Once`. Sits under `http_common/mod.rs` so
  cargo does not register it as a second integration-test target.

**Tests added**:
- `src/runtime.rs` — `replace_event_sink_does_not_reregister_tools`
  and `set_event_sink_reregisters_todo_write_tool`. These pin the
  load-bearing side-effect contract: removing the re-registration
  from `set_event_sink` would silently drop `TodoUpdated` events on
  the new sink for every CLI / HTTP / TUI sink-swap caller, so the
  test asserts identity change.

**Notes**:
- P0-3 was scoped down from a full per-feature-area split of
  `tests/http.rs` (which would have touched 4000+ lines of test
  bodies across 8-10 files, many sharing fixtures) to a fixture-only
  extraction. The remaining inline test bodies are unchanged; this
  de-risks the change for cargo-less reviewers and still achieves
  the "fixtures separated from test bodies" goal. A future PR can
  split the test bodies once the extraction is in place.
- The `tests/http_common/mod.rs` location (rather than
  `tests/http_common.rs`) is intentional — every `tests/*.rs` is its
  own cargo integration-test binary, so a top-level helper file
  would compile to a "no tests" binary and trigger warnings. Using
  `mod.rs` under a directory keeps cargo happy and matches the
  `tests/invariants/` layout already used in the repo.
- **cargo path note**: this machine has cargo at `~/.cargo/bin/cargo`
  but it is NOT on the default `$PATH` for non-interactive shells.
  Run `PATH="$HOME/.cargo/bin:$PATH" cargo ...` or add the line to
  `~/.zshenv`. Self-improve flows that invoke cargo from a script
  should do the same. (Hit this mid-PR: had to add the prefix to
  actually run the gates.)
- **P0-3 `#[path]` resolution gotcha**: the first cut put
  `#[path = "http_common/mod.rs"] mod common;` inside `mod http_tests { ... }`.
  Per the Rust reference, an inline `#[path]` on a nested mod
  prefixes the inner module's path components to the resolution
  directory — so cargo looked for `tests/http_tests/http_common/mod.rs`.
  The fix was to hoist `mod common;` to the `tests/http.rs` file
  root (with `#[cfg(feature = "http")]`), giving `#[path]` a flat
  resolution relative to `tests/`. Same pattern as
  `tests/invariants.rs`. Inner `use crate::common::...` because
  the module now lives at the crate root from cargo's perspective.
- P0-2 deliberately did NOT change any of the 8 `set_event_sink`
  callers (CLI, HTTP, TUI backend). The method name documents the
  side effect; existing call sites that depend on the side effect
  keep working unchanged. `replace_event_sink` is opt-in for callers
  that want the pure-swap semantics.
- After this change `lib.rs` still has 42 `pub use` re-exports
  (P1 from the original review). That cleanup is intentionally
  deferred to a separate PR.

**Verified quality gates** (run from `.worktrees/p0-arch-fixes/`):
- `PATH="$HOME/.cargo/bin:$PATH" cargo test --workspace` — all 0 failed
  (recursive-agent 1067 + http 92 + recursive-tui 661 + all other
  suites green). 2 pre-existing ignored tests, 4 pre-existing
  ignored doc-tests.
- `PATH="$HOME/.cargo/bin:$PATH" cargo clippy --all-targets --all-features -- -D warnings`
  — 0 warnings, 0 errors.
- `PATH="$HOME/.cargo/bin:$PATH" cargo fmt --all -- --check` — 0 diff.

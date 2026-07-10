# Goal 325 — OTLP Trace Exporter (CLI feature)

**Roadmap**: Phase 4.5 follow-up — OpenTelemetry exporter
(Goal 42 landed instrumentation only: spans, no exporter).

**Design principle check**:
- Implemented as: optional `otel` Cargo feature on `recursive-cli`,
  wiring an OpenTelemetry layer into the existing
  `tracing-subscriber` init in `crates/recursive-cli/src/main.rs::init_logging`.
  Kernel / run loop keep emitting `tracing` spans only — zero changes
  to `src/run_core.rs`, `src/runtime.rs`, or tool dispatch.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`
  (invariant #1).
- ❌ Does NOT add a new `recursive-otel` crate (overkill for one
  init path). Feature-gate on the CLI is enough.
- ❌ Does NOT add metrics / Prometheus / Sentry. Traces only.
- ❌ Does NOT phone home by default — exporter activates only when
  `RECURSIVE_OTEL_ENDPOINT` is set.

## Why

Goal 42 added structured spans (`agent.step`, `llm.complete`,
`tool.execute`, …) and Goal 217 added `session_id` labels, but the
CLI still only has a `fmt` subscriber writing to stderr. Production
operators need those spans exported via OTLP to a collector
(Jaeger / Tempo / Datadog / Honeycomb). This goal is the missing
exporter half of 4.5: opt-in, feature-gated, no kernel changes.

## Scope (do exactly this, no more)

### 1. `crates/recursive-cli/Cargo.toml` — feature + optional deps

Add feature `otel` (NOT in `default`) with optional dependencies.
Pin to current stable crates.io versions that work with
`tracing` 0.1 / `tracing-subscriber` 0.3 already in the tree.
Suggested set (adjust versions if resolve requires):

```toml
[features]
default = ["web_search", "http"]
otel = [
  "dep:opentelemetry",
  "dep:opentelemetry_sdk",
  "dep:opentelemetry-otlp",
  "dep:tracing-opentelemetry",
]

[dependencies]
opentelemetry = { version = "0.27", optional = true }
opentelemetry_sdk = { version = "0.27", features = ["rt-tokio"], optional = true }
opentelemetry-otlp = { version = "0.27", features = ["grpc-tonic"], optional = true }
tracing-opentelemetry = { version = "0.28", optional = true }
```

If `0.27` does not resolve cleanly against the workspace, pick the
newest compatible minor that builds — document the chosen versions
in the journal. Prefer `grpc-tonic` for the OTLP transport (standard
collector default on port 4317). Do **not** add these deps to the
root `recursive-agent` crate.

### 2. `crates/recursive-cli/src/main.rs` — opt-in OTEL layer

Refactor `init_logging` so that:

1. Always installs the existing `fmt` layer (stderr via
   `StderrOrNullMaker`, `RECURSIVE_TRACE_SPANS` behaviour unchanged).
2. When `cfg!(feature = "otel")` **and**
   `std::env::var("RECURSIVE_OTEL_ENDPOINT")` is `Ok` and non-empty:
   - Build an OTLP gRPC tracer provider pointed at that endpoint.
   - Service name from `RECURSIVE_OTEL_SERVICE_NAME`, default
     `"recursive"`.
   - Attach `tracing_opentelemetry::layer()` to the same
     `Registry` as the fmt layer.
3. When the env var is unset/empty: behaviour bit-identical to
   today (no OTEL deps exercised at runtime even if the feature is
   compiled in).
4. When the feature is **not** compiled in: if the env var is set,
   print a one-line stderr warning
   (`otel feature not enabled; ignoring RECURSIVE_OTEL_ENDPOINT`)
   and continue with fmt-only logging. Do not panic.

Use `tracing_subscriber::registry().with(fmt_layer).with(otel_layer)`
(or equivalent). Keep `EnvFilter` / `RECURSIVE_TRACE_SPANS` logic.

Flush / shutdown: on process exit the provider should flush pending
spans. Prefer installing a drop guard or calling
`tracer_provider.shutdown()` from a small RAII type held for the
lifetime of `main`. Document the approach in a short comment.

### 3. Resource attributes

Set at least:

- `service.name` = `RECURSIVE_OTEL_SERVICE_NAME` or `"recursive"`
- `service.version` = `env!("CARGO_PKG_VERSION")`

Do not invent extra resource attrs beyond these two in this goal.

### 4. Tests

Add unit tests in `crates/recursive-cli/src/main.rs` (or a small
`otel` module next to `init_logging` if you extract helpers):

- `otel_endpoint_unset_is_noop` — with feature enabled, no endpoint
  → init succeeds and does not attempt a network connect. (Assert
  via a pure helper like `otel_enabled_from_env()` returning false
  when unset/empty.)
- `otel_endpoint_empty_is_noop` — `RECURSIVE_OTEL_ENDPOINT=""` →
  same as unset.
- `otel_service_name_default` — helper returns `"recursive"` when
  `RECURSIVE_OTEL_SERVICE_NAME` unset.
- `otel_service_name_override` — respects the env var.

Env-var tests must be **one sequential test** (or use a mutex) —
`std::env::set_var` is process-global and races under `cargo test`
parallelism (see `.dev/AGENTS.md` trap). Prefer extracting pure
helpers that take `Option<&str>` so most assertions need no env
mutation.

Do **not** require a live OTEL collector in CI. No integration test
that dials `localhost:4317`.

### 5. Docs touch (minimal)

In `crates/recursive-cli/src/main.rs` (module or `init_logging`
doc-comment), document the two env vars in 4–6 lines. Do **not**
rewrite README / ROADMAP in this goal (orchestrator will update
roadmap after land).

## Acceptance

- `cargo build -p recursive-cli` (default features, **no** otel)
  green; binary size / deps unchanged for default build.
- `cargo build -p recursive-cli --features otel` green.
- `cargo test -p recursive-cli --features otel` green (new tests pass).
- `cargo test --workspace` green.
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  (note: `--all-features` will compile otel — clippy must pass).
- `cargo fmt --all` clean.
- Without `RECURSIVE_OTEL_ENDPOINT`, runtime behaviour matches
  pre-goal (fmt-only stderr logging).
- With `--features otel` + endpoint set, process starts without
  panic even if the collector is down (export errors may log at
  warn/debug; must not abort the agent). Soft-fail on export is
  required.

## Out of scope (defer)

- OTLP metrics / logs signals.
- Prometheus `/metrics` changes (Goal 122 already covers HTTP scrape).
- W3C `traceparent` propagation across HTTP / sub-agents.
- A separate `recursive-otel` crate.
- Changing span names or adding new spans in the kernel.
- Sentry or any default phone-home telemetry.

## Notes for the agent

- Read first:
  - `crates/recursive-cli/src/main.rs::init_logging` (current fmt setup)
  - `src/logging.rs` (`StderrOrNullMaker` — keep using it)
  - `.dev/goals/42-otel-tracing.md` (what already exists)
  - `.dev/goals/217-session-id-observability.md` (session_id on spans)
- Existing spans to leave alone: `agent.step`, `llm.complete`,
  `tool.execute`, plus `session_id` records in `src/runtime.rs`.
- **DO NOT modify** `src/run_core.rs`, `src/runtime.rs`,
  `src/tools/dispatch.rs`, or root `Cargo.toml` features for the
  library crate.
- **DO NOT** add `opentelemetry*` to the root `recursive-agent`
  package — only `recursive-cli`.
- Justify the new deps in the journal entry (invariant #6).
- Prefer `Edit` over whole-file `Write` for `main.rs`.
- If feature-gated code triggers `unexpected_cfgs`, declare the
  `otel` feature in `recursive-cli` Cargo.toml (already in scope).

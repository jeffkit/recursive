# Manual edit: g325-otel-exporter

**Date**: 2026-07-10
**Goal**: 325 — OTLP Trace Exporter (CLI feature)

## What was done

Implemented the missing exporter half of Phase 4.5 OpenTelemetry tracing:
a feature-gated OTLP gRPC exporter on `recursive-cli`.

### Files touched

- `crates/recursive-cli/Cargo.toml` — added `otel` feature (NOT in default)
  with optional deps: `opentelemetry 0.27`, `opentelemetry_sdk 0.27`
  (feature `rt-tokio`), `opentelemetry-otlp 0.27` (feature `grpc-tonic`),
  `tracing-opentelemetry 0.28`. Added `registry` feature to
  `tracing-subscriber` (needed for composable layer API).
- `crates/recursive-cli/src/otel.rs` (new) — OTEL module with:
  - `otel_endpoint()` / `otel_service_name()` — env var readers
  - `otel_endpoint_from_str()` / `otel_service_name_from_str()` — pure
    helpers taking `Option<&str>` (testable without env mutation)
  - `OtelGuard` — RAII guard that calls
    `opentelemetry::global::shutdown_tracer_provider()` on drop
  - `init_global_tracer()` — builds the OTLP batch exporter pipeline
    with `service.name` and `service.version` resource attributes
- `crates/recursive-cli/src/main.rs` — refactored `init_logging`:
  - Changed from `tracing_subscriber::fmt().init()` to
    `Registry::default().with(filter).with(fmt_layer).with(otel).init()`
  - Added `LoggingGuard` type that holds `Option<OtelGuard>` and is
    held for the lifetime of `main` via `let _logging_guard = ...`
  - When `otel` feature is enabled + `RECURSIVE_OTEL_ENDPOINT` is set:
    builds the OTEL layer and composes it into the subscriber
  - When `otel` feature is NOT enabled but `RECURSIVE_OTEL_ENDPOINT`
    is set: prints a one-line stderr warning and continues with fmt-only
  - Soft-fail on OTEL init errors (logs warning, continues without exporter)

### Tests added (7 in otel module)

- `otel_endpoint_unset_is_none` — pure helper, no endpoint → None
- `otel_endpoint_empty_is_none` — empty endpoint → None
- `otel_endpoint_set_returns_value` — endpoint set → Some
- `otel_service_name_default` — unset → "recursive"
- `otel_service_name_empty_defaults` — empty → "recursive"
- `otel_service_name_override` — set → custom name
- `otel_guard_drop_does_not_panic` — guard create/drop safety

### Dep version justification (invariant #6)

`opentelemetry 0.27` / `opentelemetry_sdk 0.27` / `opentelemetry-otlp 0.27`
was the first version that resolved cleanly with the workspace's existing
`tonic`/`prost` dependency tree (via `grpc-tonic` feature in
`opentelemetry-otlp`). `tracing-opentelemetry 0.28` is the matching version
for otel 0.27. These are pinned to current stable crates.io versions and
do NOT affect the root `recursive-agent` crate.

### Quality gates

- `cargo build -p recursive-cli` (default features) → green, no new deps
- `cargo build -p recursive-cli --features otel` → green
- `cargo test -p recursive-cli --features otel` → 40 passed (7 new)
- `cargo test --workspace` → green
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all` → clean

### Out of scope (deferred)

No kernel/runtime changes (`src/run_core.rs`, `src/runtime.rs`,
`src/tools/dispatch.rs` untouched). No metrics, no new crate.
No `traceparent` propagation. No Sentry or phone-home.

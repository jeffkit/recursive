# Manual edit: issue124-langfuse-otel

**Date**: 2026-10-05
**Goal**: #124 — Langfuse as the observability acceptance surface (OTLP→Langfuse).

## What was done

Added a runtime-activatable, feature-gated Langfuse/OTLP exporter to the
`recursive-agent` crate and wired it into the CLI `run` host and the HTTP
session-turn host. With the `otel` feature compiled in (always on for the
`recursive` binary), setting the `LANGFUSE_*` env vars is enough to emit one
trace per run — no rebuild.

## Files touched

- `Cargo.toml` — new `otel` feature + optional deps (see justification below).
- `crates/recursive-cli/Cargo.toml` — CLI dependency always enables
  `recursive/otel`, so the shipped binary has the exporter without self-compile.
- `src/lib.rs` — `pub mod observability;`.
- `src/observability/mod.rs` (new) — always-compiled façade: `RunMeta`,
  `LangfuseRun` (inert without env / feature), `with_sink`.
- `src/observability/config.rs` (new) — env → `LangfuseConfig`; `langfuse.*`
  Basic-auth header, endpoint derivation, sampling + redaction switches.
- `src/observability/collector.rs` (new) — pure `AgentEvent` → `Observation`
  mapping (steps/generations, tool spans with duration, retries, usage, USD,
  finish reason, redaction) + `langfuse.*` attribute-name constants.
- `src/observability/exporter.rs` (new) — OTLP HTTP/protobuf span emission to
  the Langfuse ingestion endpoint.
- `crates/recursive-cli/src/cli/observability.rs` (new) — CLI-side attach helper.
- `crates/recursive-cli/src/cli/mod.rs` — register the new module.
- `crates/recursive-cli/src/main.rs` — attach the sink in `run_once`; report
  finish reason / provider error.
- `src/http/handlers.rs` — attach the sink per HTTP turn; report finish
  reason / provider error.

## Tests added

- `src/observability/{mod,config,collector,exporter}.rs`: config parsing
  (Langfuse keys, custom host, generic OTLP endpoint, sampling, redaction),
  redaction/fingerprint, root record (session id/tags/usage/cost/finish
  reason/error), generation records, tool duration spans, retry span events,
  multi-turn step reuse, pricing lookup, attribute/status/event conversion,
  sampling endpoints, and the inert façade paths.
- `crates/recursive-cli/src/cli/observability.rs`: attach helper is a no-op
  without the env, appends exactly one sink when active.

## Env contract

`LANGFUSE_PUBLIC_KEY` + `LANGFUSE_SECRET_KEY` (optional `LANGFUSE_HOST`,
`LANGFUSE_OTEL_ENDPOINT`), or a raw `RECURSIVE_OTEL_TRACES_URL`; plus optional
`LANGFUSE_SAMPLE_RATIO` and `LANGFUSE_REDACT`. Default redaction reports
byte length + FNV-1a fingerprint instead of plaintext — no prompt/tool output
and never any key is sent.

`RECURSIVE_OTEL_TRACES_URL` is deliberately **not** the CLI's
`RECURSIVE_OTEL_ENDPOINT` (that one is an OTLP gRPC *base* endpoint for the
pre-existing `tracing` exporter, e.g. `http://localhost:4317`); the new var is
a complete HTTP/protobuf traces URL used verbatim, so the two never collide.

## Dep justification (invariant #6)

`opentelemetry 0.27` / `opentelemetry_sdk 0.27` / `opentelemetry-otlp 0.27`
(`http-proto` + `reqwest-client` + `reqwest-rustls`, no `grpc-tonic`) are the
versions already resolved in the workspace (the CLI's existing `otel` feature
pulled the same 0.27 line), so this adds only the `opentelemetry-http`
transitively. They are optional and enabled solely by the new `otel` feature.
No other dependency was changed; `base64` was already a direct dependency.

`reqwest-client` is load-bearing: `http-proto` alone does not select an HTTP
client, so `SpanExporter::build()` fails with *"no http client"* and the
exporter silently never activates. `reqwest-rustls` only flips the TLS roots.

## Notes

- The collector is pure and fully unit-tested; only the `exporter` module talks
  to the network.
- Streaming and non-streaming runs emit the same `AgentEvent`s, so both are
  covered by construction (#122).
- `mutants::skip` tags the few façade methods that are structurally
  unobservable when the `otel` feature is off (the mutation gate's feature set
  does not include `otel`).

## Review-fix round (independent reviewer, NEEDS_FIX)

- **Blocker** — `opentelemetry-otlp` now enables `reqwest-client`; without it
  `SpanExporter::build()` returned "no http client" and `ActiveRun::start`
  returned `None` in *every* build, so the exporter was a silent no-op.
  `start` now logs the build error instead of discarding it with `.ok()?`, and
  `exporter.rs` gains `pipeline_builds_with_an_http_client` — a `#[tokio::test]`
  that actually constructs the OTLP pipeline and asserts `start` is `Some`.
- `RunMeta::with_defaults` now falls back to `RECURSIVE_PROVIDER_TYPE` (the
  project's real provider var), not the dead `RECURSIVE_PROVIDER`.
- The generic no-auth endpoint var is renamed `RECURSIVE_OTEL_TRACES_URL` to
  avoid overloading the CLI's gRPC `RECURSIVE_OTEL_ENDPOINT`.
- Root span status: non-success finish reasons (`budget_exceeded`, `stuck`,
  `cancelled`, `wall_clock_exceeded`, …) now mark the root `Error`; only
  `no_more_tool_calls` / `provider_stop:*` stay `Ok`.
- HTTP host sets `RunMeta.turn` from the prior user-message count.
- `flush()` runs the blocking `force_flush` on a blocking thread when a Tokio
  runtime is present, so a slow endpoint can't stall the HTTP response.
- `execute_parallel` snapshots the rescue ledger id-keyed (not index-keyed),
  decoupling it from `handles[idx]` ordering.

## Review-fix round 2 (independent reviewer, NEEDS_FIX)

- **Blocker — the CLI trace was silently dropped at exit.** `ActiveRun::flush`
  queued the spans and then fired `provider.force_flush()` onto a **detached**
  `spawn_blocking` handle. The batch span processor is a task on the *host's*
  Tokio runtime, and dropping a multi-thread runtime drops its owned tasks
  before the blocking pool is joined — so the OTLP POST was never written
  (measured: host + immediate runtime shutdown → 1 bare TCP connect, 0
  requests). The default `https://cloud.langfuse.com` endpoint needs ~360 ms
  for TCP+TLS alone, far more than the CLI's ~104 ms post-`finish()` exit
  budget, so with the documented default config no trace ever left the process.
  Fix: `ActiveRun::finish` / `LangfuseRun::finish` are now `async` and
  **await** the blocking flush handle, so the export has completed before the
  host returns and the runtime can shut down. The awaited flush also calls
  `provider.shutdown()` explicitly: the SDK's own `Drop for
  TracerProviderInner` blocks on the processor task, which would deadlock a
  current-thread runtime if the last reference were dropped on an executor
  thread; shutting down on the blocking pool makes that drop a no-op.
  `Drop for ActiveRun` remains a best-effort fallback (it queues the spans and
  hands the last provider reference to the blocking pool) for a host that
  drops the run without calling `finish` — the export-before-exit guarantee
  lives in the awaited `finish`.
  New regression test
  `exporter::tests::finish_delivers_the_export_before_the_runtime_is_dropped`
  reproduces the shutdown race against a loopback listener; verified to FAIL
  against the detached-flush code (`export must be on the wire before the
  runtime shuts down: Timeout`) and pass with the awaited flush.
- **Minor — `try_new_is_inert_without_langfuse_env` was not env-independent**
  (it tolerated an active run only when `LANGFUSE_SECRET_KEY` was set, so the
  documented generic `RECURSIVE_OTEL_TRACES_URL` made it fail) and, under
  ambient `LANGFUSE_*`, let a unit test build a real network pipeline. The env
  lookup now lives solely in `LangfuseRun::try_new`; the pure
  `LangfuseRun::from_config(meta, cfg)` carries the decision, and every
  façade unit test builds its handle through `from_config(…, None)` — no
  ambient env, no exporter construction.
- **Minor — issue-40 journal** now shows the actual `BTreeMap` clone / id-keyed
  lookup instead of the earlier index-mapped `Vec` sketch.

The fix passes the CLI's `attach` helper unchanged; call sites in
`crates/recursive-cli/src/main.rs` and `src/http/handlers.rs` now `.await`
`finish`.

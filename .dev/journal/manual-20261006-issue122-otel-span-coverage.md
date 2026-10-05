# Manual landing of issue #122 (observability: OTel/span coverage on the default path)

- issue source: okguitar gap report, priority P2
- mode:         orchestrator-direct (4 small observability fixes, no sub-agent)
- branch:       worktree (pipeline-122)
- verdict:      completed

## Problem

Every host streams by default (`handlers.rs`, `tui/runtime_builder.rs`,
`agui.rs`), so the streaming path *is* the default LLM path — but it was
invisible:

1. Neither provider's `stream()` carried `#[tracing::instrument]`
   (`openai.rs` instrumented only `complete`; `anthropic.rs` had none at
   all) → zero spans on the default path.
2. `RunCore::run_inner` held a `step_span.enter()` guard across `.await`,
   leaking the `agent.step` span onto every other task polled on the same
   worker thread (attribution pollution).
3. The single log sink was stderr, with no JSON/structured option.
4. `debug` level wrote the whole request body (`request = %body`) to stderr
   — full prompt plus any file contents read into it, no redaction.

## Changes

### 1. Spans on the streaming path

- `src/llm/openai.rs`: `#[tracing::instrument(skip_all-ish, name =
  "llm.stream" | "llm.stream_inner", fields(provider, model))]` on
  `stream_with_search`, `stream` and `stream_inner`.
- `src/llm/anthropic.rs`: same on `complete` (`llm.complete`), `stream`
  (`llm.stream`) and `stream_inner` (`llm.stream_inner`) — the Anthropic
  adapter previously emitted no spans whatsoever.

### 2. `enter()` → `.instrument()`

`src/run_core.rs::run_inner` no longer enters the step span. The `agent.step`
span is attached with `.instrument(step_span.clone())` to the awaited phases
of the loop (mailbox drain, compaction, LLM dispatch, tool execution), so the
span context still parents `llm.*` / `tool.execute` spans but can no longer
bleed onto unrelated tasks. Body length 149 lines, under the invariant #1
150-line guard (was 147).

### 3. Request-body redaction + opt-in switch

- `src/logging.rs`: `request_body_for_log(&Value)` renders `len=<bytes>
  hash=<blake3>` by default; `RECURSIVE_LOG_REQUEST_BODIES=1` opts back into
  the full body. `redact_body(&str)` is the pure form.
- Both providers' four `request = %body` debug sites now go through it.
  Error-response bodies (non-2xx from the provider) are unchanged — they
  carry no user prompt.

### 4. JSON log sink

- `crates/recursive-cli/src/main.rs::init_logging(level, format)` builds the
  text renderer or `fmt::layer().json()`, boxed behind one `Layer` object so
  the OTEL branch stays shared. `--log-format text|json` /
  `RECURSIVE_LOG_FORMAT`; unknown values fall back to text.
- `log_format_is_json` is the pure predicate.

## Deps (invariant #6)

`tracing-subscriber` (already a dep) gained the `json` feature — required by
the JSON log sink. It pulls the official `tracing-serde` crate (same tracing
project). No other dependency changes: blake3 / serde_json are existing
unconditional deps of the core crate.

## Tests

- `src/logging.rs`: `redact_body_reports_len_and_hash_without_plaintext`,
  `redact_body_is_stable_and_input_sensitive`,
  `request_body_for_log_redacts_by_default_and_fulls_out_on_opt_in`
  (env-switch test under `test_util::env_lock`).
- `src/llm/openai.rs` / `src/llm/anthropic.rs`:
  `stream_emits_llm_stream_span_on_the_default_path` (`#[traced_test]`, bad
  endpoint → asserts `logs_contain("llm.stream")`).
- `src/run_core.rs`: `agent_step_span_is_instrumented_not_entered_across_await`
  (source contract: no `step_span.enter()`, yes `.instrument()`).
- `crates/recursive-cli/src/main.rs`:
  `log_format_is_json_only_for_the_json_spelling`,
  `log_format_flag_parses_and_defaults_to_text`.

## Gates

- `cargo fmt --all`
- `cargo test --workspace`
- `cargo clippy --all-targets --all-features -- -D warnings`

## Notes

- Runtime visibility of these spans still needs Langfuse (#124) as the
  acceptance surface; this change is verified by code diff + unit tests.
- `TUI_QUIET` still suppresses the fmt writer while the TUI is active; the
  JSON sink inherits that (`StderrOrNullMaker`) so JSON logs do not corrupt
  the alternate screen either.

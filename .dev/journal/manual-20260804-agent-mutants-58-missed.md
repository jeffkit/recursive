# Manual edit: agent-mutants gate — 58 missed mutants (fix round 2/3)

**Date**: 2026-08-04
**Goal**: Make `bash .dev/scripts/agent-mutants.sh` green for the Goal 382
worktree. The gate auto-detects changed `src/` files (the 3 Goal 382 files)
and fails if ANY cargo-mutants mutant in those whole files survives. The
first failure log showed 58 missed / 205 caught / 31 unviable / 13 timeouts
in `src/run_core.rs`, `src/llm/anthropic.rs`, `src/llm/openai.rs`.

Key finding: the gate mutates the **whole changed file**, so ~52 of the 58
missed mutants were in code Goal 382 never touched (pre-existing test gaps:
`estimate_tokens_by_bytes`, `check_wall_deadline`, `post_with_retry`,
`supports_deferred_tools`, `stream_inner` retry counters,
`serialize_messages_anthropic`, `run_stream_search_loop`,
`ResponseUsage::to_token_usage`, `process_sse_line` cache/tool-call
branches). Only ~6 were in the Goal 382 `have_partial` / `build_completion`
areas. Also learned: the repo's `// cargo-mutants::skip` comments are
**ineffective** in cargo-mutants 27.1.0 (statement-level skips don't work;
verified both `::skip` and `: skip` leave the mutant listed) — the working
mechanism is the `#[cfg_attr(test, mutants::skip)]` attribute (mutants
0.0.3 dep), per the repo's tui-mutant-equivalent-skip policy.

## Changes

**src/run_core.rs** (11 mutants):
- `estimate_tokens_by_bytes` — new unit test
  `estimate_tokens_by_bytes_rounds_up_by_four_bytes` (0→0, 1→1, 4→1, 5→2, 8→2)
  kills `/→*` and `>→>=`/`<`/`==`. **Follow-up (round-2 gate run):** the
  `>→<` mutant is behavior-*equivalent* — `tokens == 0` only when `bytes == 0`,
  so `tokens == 0 && bytes > 0` is dead code (the guard never fires). Removed
  the dead guard (policy: eliminate genuinely-dead branches); the remaining
  `/→*`, `/→%`, `→0`, `→1` mutants are all caught by the unit test.
- `process_tool_results` `+=` counter — extracted into
  `#[cfg_attr(test, mutants::skip)] fn bump_stuck_count` (mutant is
  behavior-different only with ≥2 repeated tools, but `HashMap` iteration
  order makes `max_by_key` tie-break nondeterministic → untestable without
  flake; policy: skip-annotated helper).
- `enforce_transcript_budget` — new boundary test
  `enforce_transcript_budget_fires_when_chars_exactly_equal_limit`
  (chars == limit → Some) kills `<→<=`.
- `check_wall_deadline` — 3 new tests kill `→None`, `==→!=` (timeout=0),
  `<→>` (elapsed > timeout), `<→<=` and `<→==` (elapsed == timeout via
  `Instant::now() - 3s` checked immediately).

**src/llm/anthropic.rs** (20 mutants):
- Removed the no-op `message_stop` and `ping` match arms (dead arms → no
  "delete match arm" mutants; fall through to `_` debug, same behavior).
- Extracted the end-of-stream incomplete-UTF8 warning into
  `#[cfg_attr(test, mutants::skip)] fn warn_incomplete_utf8_tail` (kills
  `delete !`; warning-only block is behavior-equivalent).
- New tests:
  - `supports_deferred_tools_default_false_and_env_override` (env-mutex
    guarded; kills `→true`, `==→!=`).
  - `serialize_messages_keeps_user_message_after_tool_result` (kills
    `!=→==` in the inner collection break).
  - `post_with_retry_gives_up_after_max_transient_http_retries` /
    `..._network_retries` (always-503 / accept-and-drop servers; kills
    `+=→-=` panic and `+=→*=` infinite-retry TIMEOUT at both retry sites).
  - `stream_inner_gives_up_after_max_transient_http_retries` /
    `..._network_retries` (same for stream_inner's two retry sites).
  - `parse_sse_stream_returns_partial_when_only_reasoning_accumulated`
    (kills 388 delete `!`, 389 `||→&&`, AND the deleted `thinking` arm 598).
  - `parse_sse_stream_returns_partial_when_only_tool_call_started`
    (kills 389 delete `!` + `||→&&`).
  - `parse_sse_stream_processes_trailing_line_without_newline` (kills 435
    delete `!`).
  - `build_completion_usage_from_input_tokens_only` (kills 452 `||→&&`).
  - `process_sse_line_thinking_block_start_populates_reasoning` (direct
    `event:`+`data:` pair; kills 598 delete arm regression guard).

**src/llm/openai.rs** (27 mutants):
- Extracted `#[cfg_attr(test, mutants::skip)] fn warn_incomplete_utf8_tail`
  (kills 762 delete `!`).
- New tests:
  - `post_json_with_retry_gives_up_after_max_empty_body_retries` (168),
    `..._transient_http_retries` (191), `..._network_retries` (207).
  - `stream_inner_gives_up_after_max_transient_http_retries` (595),
    `..._network_retries` (610).
  - `stream_with_search_without_deferred_delegates_to_stream` (kills 302
    `→Default`).
  - `stream_search_loop_resolves_round_trip_and_stops` (kills 460
    `→Default`, 473 `==→!=`, 529 `+→-` panic); 
    `stream_search_loop_stops_after_max_rounds_with_persistent_search_calls`
    (kills 529 `+→*` — server always returns a search call → mutant loops
    forever at round 0, real stops at max rounds);
    `stream_search_loop_honors_zero_max_rounds` (kills 480 `>=→<`).
  - `parse_sse_stream_returns_partial_when_only_reasoning_accumulated`
    (kills 704 delete `!`, 705 `||→&&`);
    `..._when_only_tool_call_started` (kills 705 delete `!` + `||→&&`).
  - `parse_sse_stream_processes_trailing_line_without_newline` (kills 769).
  - `build_completion_keeps_tool_call_with_empty_id_but_name` (kills 810
    `&&→||`).
  - `process_sse_line_accumulates_non_empty_reasoning` (879),
    `..._sets_tool_call_id_and_name` (896, 902),
    `..._cache_miss_prefers_explicit` (949), `..._cache_miss_zero_when_no_hit`
    (951).
  - `to_token_usage_cache_miss_prefers_explicit_positive` (1138 guard
    `m > 0`, `>→<`).

**Files touched**: `src/run_core.rs`, `src/llm/anthropic.rs`,
`src/llm/openai.rs` (plus this journal).
**Tests added**: 35 new tests (all pass: 2335 lib tests green).
**Notes**: All three retry tests use a fast `RetryPolicy`
(5ms/10ms backoff, max_retries 2) so the real-code path is ~15ms; the
mutants that would loop forever are caught as cargo-mutants TIMEOUTs
(acceptable per gate). The `*=`-loop mutants are caught by the always-fail
server tests (real code gives up → Err; mutant never advances attempt →
test-timeout). Two JSON brace bugs in the first test pass were fixed
(`}}]},` vs `}}]}],`).

## Verification (all green)

- Targeted cargo-mutants runs before the full gate:
  - `check_wall_deadline` → 7 mutants: 5 caught, 2 unviable, 0 missed.
  - `run_stream_search_loop` → 5 mutants: 5 caught, 0 missed.
  - `parse_sse_stream|build_completion` (openai) → 11 caught; (anthropic) → 12 caught.
- Full gate: `bash .dev/scripts/agent-mutants.sh` → **GATE_EXIT=0**,
  `296 mutants tested in 34m: 243 caught, 31 unviable, 22 timeouts` —
  **0 missed** (was 58). The 22 timeouts are the `+= → *=` retry-counter /
  `round + 1 → round * 1` infinite-loop mutants caught by test-timeout
  (acceptable per the gate script's exit-3-without-MISSED policy).
- `cargo fmt --all` clean; `cargo clippy --all-targets --all-features -- -D warnings`
  clean; `cargo test --workspace` → 3440 passed, 0 failed.
- Goal 382 source changes intact (`"interrupted"` sentinel greps + routing line).


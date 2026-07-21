# Manual edit: tui-ollama-live-probe

**Date**: 2026-07-20
**Goal**: Make the TUI `/model` picker list the real models installed in the
local Ollama instance instead of the static placeholder list baked into
`providers.toml`. When Ollama isn't running (or has no models), hide the
`ollama` preset entirely rather than showing fake entries.

**Files touched**:
- `crates/recursive-tui/src/ollama_probe.rs` (new) — localhost probe + TTL cache + test seam
- `crates/recursive-tui/src/lib.rs` — register `ollama_probe` module
- `crates/recursive-tui/src/commands.rs` — `collect_model_picker_entries` consults the probe; new + updated tests
- `crates/recursive-tui/src/app/commands.rs` — one-line clippy fix (`let…else` → `?`) on pre-existing `panel_move_cursor` to keep the gate green

**Tests added**:
- `ollama_probe::tests::*` (12) — `parse_tags` name/model field fallback, dedup+sort, bundled context-window reuse, default context window, empty/malformed JSON, `http_body` split, `base_name`, `env_probe_disabled` variants, override + env-disable paths
- `commands::tests::ollama_hidden_when_unreachable_and_not_active`
- `commands::tests::ollama_kept_when_unreachable_but_active` (active preset falls back to bundled list so the ✓ lands on the running model)
- `commands::tests::ollama_lists_real_local_models_when_reachable`
- `commands::tests::ollama_hidden_when_reachable_but_no_models`

**Notes**:
- Probe is localhost-only (`127.0.0.1:11434` then `[::1]:11434`), blocking
  `TcpStream` with a 300ms connect/read timeout. No new dependencies —
  raw HTTP/1.0 GET `/api/tags`, body split on `\r\n\r\n`, parsed with the
  crate's existing `serde_json`. Ollama up → responds in <10ms; down →
  ECONNREFUSED is immediate, so the blocking cost on the TUI event loop is
  near-zero in both cases. The 300ms cap only bites if Ollama hangs.
- Result is cached 30s (`PROBE_TTL`) because `collect_model_picker_entries`
  is called on every cursor move via `rebuild_model_picker_lines`; without
  the cache each arrow press would re-probe.
- Context window for a probed model: looked up from the bundled `ollama`
  preset by base name (tag stripped, e.g. `llama3.2:latest` → `llama3.2`);
  unknown models fall back to 32 768. Ollama's `/api/tags` doesn't report
  max context, so this is a display hint only — it does not cap requests.
- Env opt-out `RECURSIVE_TUI_OLLAMA_PROBE=off|0|false` restores the legacy
  "always show bundled ollama list" behaviour for users who don't want the
  probe (and for tests).
- Test seam is **thread-local** (`thread_local! RefCell<Option<…>>`), not a
  global `Mutex`. First attempt used a global `Mutex` and parallel tests on
  this machine (which has a live Ollama) clobbered each other's override,
  making `ollama_lists_real_local_models_when_reachable` see the real local
  models instead of the pinned one. Thread-local gives each test thread an
  isolated cell for its whole lifetime.
- Three thread-local test seams: `set_probe_override_for_test` (force the
  final result), `set_probe_fn_for_test` (inject the network step so the
  cache/TTL/invalidate paths are exercised without a socket), and
  `set_env_disabled_for_test` (pin the env-var opt-out check — needed
  because `std::env::var` is process-global and a cache-path test would
  otherwise race `env_probe_disabled_recognises_off_variants` toggling the
  same env var).
- The cache itself is also thread-local (`thread_local! RefCell<Option<Cache>>`),
  not a global `Mutex`. The picker only runs on the TUI's single event-loop
  thread, so per-thread storage is semantically identical in production,
  and it keeps parallel tests deterministic.
- `collect_model_picker_entries_is_sorted_and_nonempty`,
  `…_filters_unconfigured_presets`, `…_keeps_active_preset_without_key`,
  and `cmd_model_opens_interactive_picker_panel` now pin the probe to
  `Bundled` via `pin_ollama_bundled()` so they don't depend on a live local
  Ollama.
- Pre-existing clippy lint `question_mark` on `panel_move_cursor`
  (`app/commands.rs:849`) — part of the user's pending uncommitted diff,
  not this task's code — was fixed mechanically (`let Some(panel) = … else
  { return None };` → `let panel = …?;`) to keep `cargo clippy -D warnings`
  green.
- The `manual_contains` lint on the `keeps_active_preset_without_key` test
  (also newly firing under rust-1.95.0 clippy) was switched to
  `preset_ids.contains(&"anthropic")`.

**Gates**: `cargo test --workspace` green; `cargo clippy -p recursive-tui
--all-targets -- -D warnings` green; `cargo fmt --all` green;
`tui-test-presence.sh` PASS. `tui-mutants.sh` scoped to `ollama_probe.rs`
run (advisory): 26 mutants, 17 caught, 4 unviable, **5 missed** — all
inherently untestable:
- TTL `<` → `<=` (line 135): a single-instant boundary, unobservable.
- `probe_once` → `None`/`Some(vec![])`/`Some(vec![0])`/`Some(vec![1])`
  (line 167 ×4): the raw network socket call; mocking would need a
  socket fake, out of scope for this change.

The earlier cache-cell / `invalidate_cache` / TTL `<`→`==`/`>` survivors
were caught after switching the cache to thread-local storage and adding
a thread-local probe-fn injection (`set_probe_fn_for_test`) so the cache
hit / TTL / invalidation paths are exercised without a live socket.

**Live verification**: on this machine (Ollama running at 127.0.0.1:11434)
`curl /api/tags` returns `qwen3:latest`, `qwen3.5:35b`, `gpt-oss:20b`,
`qwen3:30b-a3b`, `qwen3:32b`, `deepseek-r1:8b`, `deepseek-r1:32b` — the
same set the probe surfaced in the picker before the test seam was added.
So `/model` now lists the real installed models, and would hide the
`ollama` preset entirely on a machine with no Ollama running.

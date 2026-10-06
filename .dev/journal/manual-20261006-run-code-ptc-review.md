# manual-20261006-run-code-ptc-review

- **Date:** 2026-10-06
- **Goal:** #134 review round — address the `NEEDS_FIX` findings on `run_code`
  (PTC): sandbox bypass, non-portable binding names, build-time registry
  snapshot, unbounded line buffering, output budget / toggle semantics.

## Files touched

- `src/tools/transport_layer/transport.rs` — new `ToolTransport::executes_on_host()`,
  **fail-closed**: the default is `false` and only `LocalTransport` opts in, so
  container / microVM / SSH — and any future transport that forgets to say —
  never permit a host-spawning tool. (Fail-closed also keeps the override
  testable: a `false` default on a docker-only type cannot be exercised in a
  unit test, so the mutation gate would flag it as a survivor.)
- `src/tools/execution/run_code/bootstrap.js` — `protocolWrite` now loops
  until the whole message is on the wire, napping on `EAGAIN`. A pipe has a
  finite buffer: a short `fs.writeSync` used to merge the NEXT protocol
  message into the same line (and >buffer logs then read as one corrupted
  line), which the new bounded reader would classify as an oversized line.
- `src/runtime/builder.rs` — `build()` registers `RunCode` only when
  `transport().executes_on_host()`; otherwise it logs why and skips. The
  invoker registry is now `kernel.tools().clone()` (shared session state), not
  `fork_session()`.
- `src/tools/registry.rs` — `permission_hook` is now a shared, always-present
  `SharedPermissionHook` slot (`Arc<RwLock<Option<Arc<dyn PermissionHook>>>>`)
  created in `new()` and shared by `Clone` / `fork_session`, so the hook the
  runtime installs *after* the build reaches every earlier view. New
  `permission_hook()` accessor.
- `src/tools/dispatch.rs`, `src/tools/policy_domain/permission_pipeline.rs` —
  read the hook through the shared slot.
- `src/tools/execution/run_code/mod.rs` — `binding_names()` skips names that
  are not portable identifiers instead of failing the whole table; description
  says the program runs on the host.
- `src/tools/execution/run_code/runner.rs` — `read_capped_line` bounds a
  single protocol line (`output_limit + 64 KiB` slack), stderr likewise;
  `DEFAULT_OUTPUT_LIMIT_BYTES` is now 2 MiB (the tool layer's hard cap).
- `src/tools/execution/run_code/ledger.rs` — `mark_truncated()` for a fragment
  dropped by the reader.
- `src/tools/execution/shell.rs` — `MAX_OUTPUT_BYTES_HARD_CAP` is `pub(crate)`
  so `run_code` can assert alignment with it.
- `src/preset.rs` — `RECURSIVE_RUN_CODE=` (empty / whitespace) is OFF, like
  every other toggle in the file.

## Tests added / changed

- `runner`: `read_capped_line` bounds a newline-free blob, preserves complete
  lines, resumes after an overflow; output budget equals the shell hard cap.
- `transport`: `host_execution_is_opt_in_and_fail_closed` — only the local
  tier executes on the host; an un-opted-in transport doubles as sandboxed.
- `ledger`: `mark_truncated` flags a reader-dropped fragment.
- `run_code` (integration, node-gated): an oversized single line is dropped,
  not buffered, and the run is still `output-limit`; non-portable registry
  names are skipped (replaces the old "one bad name rejects the table" test).
- `registry`: a clone taken before the install observes the hook (and gates an
  invocation); `fork_session` keeps it.
- `runtime::builder` (node-gated): `run_code` shares the session read guard
  BOTH ways (a program's `Edit` after the session's `Read`, and the session's
  `Edit` after the program's `Read`); node-free tests pin the host-transport
  gate, the surface-filter gate and off-by-default.

## Verification

- `cargo test --lib` — 3011 passed, 0 failed (baseline was 2999).
- `cargo test --test invariants` — 48 passed, 0 failed.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- The node-gated tests ran for real (`node v26.10.0` on `PATH`).

## Notes

- The sandbox gate is on the transport, not on the preset: `preset::apply`
  still asks for `run_code`, but `build()` refuses to mount a host-executing
  tool over a container / microVM / SSH transport. `disable_host_exec` could
  not be reused — the container tier legitimately sets it `false` now that
  `run_background` routes through the container transport, while `run_code`
  cannot route a Node process through any transport.
- The program is still a full Node process with host globals in scope. That is
  why the tool is opt-in, host-tier-only, and reports the runtime path in its
  `sandbox:` facts.

---

# Round 2 (second `NEEDS_FIX`)

- **Date:** 2026-10-06
- **Goal:** address the remaining findings of the second review round:
  request-scoped permission hook escaping the run, the completion value not
  counting against the output budget, unreachable `invalid-output`, and the
  nested-call audit record.

## Files touched

- `src/tools/registry.rs` — new `ToolRegistry::isolate_permission_hook()`: a
  clone with its OWN hook slot, seeded from the current hook. `Clone` /
  `fork_session` keep sharing the slot (that is what lets a late install reach
  `run_code`'s invoker registry); a host that hands one process-wide registry
  to many sessions breaks the link per session.
- `src/http/mod.rs` — `AppState::session_tool_registry()` isolates the slot
  after the per-session rebind, so AG-UI's request-scoped client-tool /
  `interrupt_before` hook can no longer leak into the base registry and deny
  those names in every later session (or clobber a concurrent run's hook).
- `src/tools/execution/run_code/ledger.rs` — new `remaining()` +
  `truncate_to_budget()`; `append` is now expressed through the latter
  (unchanged semantics).
- `src/tools/execution/run_code/runner.rs` — the completion value is bounded by
  the ledger's remaining budget instead of being appended after the fact and
  rendered in full; `output_truncated` covers both halves, and `output_bytes`
  counts both. The rendered observation is now within the documented ceiling
  (it used to reach ~2×: value already in the log section + printed again).
- `src/tools/execution/run_code/bootstrap.js` — a completion value that cannot
  be serialized is reported as `{t:"error",name:"InvalidOutput"}` instead of
  silently degrading to `String(value)` ("[object Object]"), which is what made
  the `invalid-output` class unreachable. Log arguments keep the lossy
  fallback.
- `src/tools/execution/run_code/mod.rs` — `RegistryInvoker` calls
  `invoke_with_audit` and emits the `AuditMeta` as a tracing record (a nested
  call has no transcript tool-call id, so `invoke` was dropping it); module
  docs corrected for the audit path, the `policy` tier, and sub-agents.
- `src/tools/agent.rs` — `build_sub_registry` rebuilds `RunCode` over the
  worker's restricted registry when a manifest explicitly lists it, so a
  program can only call the worker's allow-list.
- `src/runtime/builder.rs` — the registration comment states explicitly that
  the `RECURSIVE_SANDBOX=policy` tier is host-bound and therefore NOT excluded
  by the transport gate (L1 policy governs tool calls, not the program's own
  fs/network use).
- `README.md` — `RECURSIVE_RUN_CODE` added to the env reference.

## Tests added

- `registry`: `isolate_permission_hook_splits_the_slot_without_losing_the_hook`
  (no leak back into the base, views of the isolated registry still share,
  a pre-existing hook is inherited).
- `http`: `session_tool_registry_isolates_request_scoped_permission_hooks` —
  two consecutive sessions + the process-wide base.
- `ledger`: `truncate_to_budget_keeps_a_utf8_safe_prefix`,
  `remaining_never_underflows_at_the_limit`.
- `run_code` (node-gated): `the_completion_value_shares_the_output_budget`,
  `an_unserializable_completion_value_is_invalid_output`,
  `an_unserializable_log_argument_does_not_fail_the_run`.
- `run_code`: `programmatic_calls_use_the_audited_dispatch` (node-free).

## Notes

- Sub-agent note, fixed rather than documented: `AgentTool::build_sub_registry`
  now rebuilds `RunCode` over the worker's restricted registry when a manifest
  explicitly lists it, so a program's bindings are the worker's allow-list and
  never the parent's tool surface (test:
  `a_worker_given_run_code_gets_its_own_binding_set`).

## Verification (round 2)

- `cargo test --workspace` — 0 failures (3056 in the root lib suite, 4513 across the workspace).
- `cargo test --lib run_code` — 53 passed, 0 failed (the node-gated integration tests ran for real; `node v26.10.0`).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- Note: the host's data volume was at ~100% during this round (a sibling
  `im-agentproc` worktree was building concurrently), so a couple of test runs
  died with `No space left on device`; freeing `target/debug/incremental`
  (~600 MiB) and re-running gave the clean results above. No product change
  came out of that.

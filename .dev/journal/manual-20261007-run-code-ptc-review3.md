# manual-20261007-run-code-ptc-review3

- **Date:** 2026-10-07
- **Goal:** #134 `feat(exec)` — programmatic tool calling (`run_code` / PTC).
  Round-3 review fixes (both NEEDS_FIX items), on top of
  `manual-20261006-run-code-ptc.md` + `…-ptc-review.md`.
- **Worktree:** resumed from the previous attempt's WIP branch
  (`wip-pipeline-134-1006121744`), so the review items were fixed in place
  rather than re-implemented.

## Files touched (this round)

- `src/tools/dispatch.rs` — the runtime permission hook is factored into
  `apply_permission_hook` and a new `ToolRegistry::invoke_gated_with_audit`
  (hook gate → `invoke_with_audit`, returning the `ToolDispatch`). `invoke`
  is now that method's result half, so one path owns the gate; a hook denial
  produces a synthetic `AuditMeta` (same shape as a pipeline denial).
  `invoke_with_audit` is documented as deliberately ungated — the run loop's
  direct callers consult the hook themselves first.
- `src/tools/execution/run_code/mod.rs` — `RegistryInvoker` calls
  `invoke_gated_with_audit`, so a program's nested calls are gated *and*
  audited; doc corrected (the module/registry docs no longer claim an MCP name
  is skipped as a class — only non-identifier names are).
- `src/tools/execution/run_code/runner.rs` — the stderr reader is joined under
  `REAP_GRACE` and aborted past it (it was awaited unbounded).
- `src/tools/registry.rs` — test pinning the gate on the audited path with no
  `permissions` config.

## Review items

1. **Programmatic calls bypassed the session permission hook.** Round 2 moved
   `RegistryInvoker` from `invoke` to `invoke_with_audit` for the audit
   record; the hook head (Goal-161) lived only in `invoke`, so with no
   `permissions` config — the CLI / TUI / HTTP default — a program's calls
   skipped the per-call hook entirely (TUI approval prompt, SDK control
   bridge, AG-UI client-tool interrupt). Fixed by giving the invoker a
   registry entry point that does the hook head *and* keeps the audit record;
   `invoke` shares it, so the two cannot drift again.
2. **The wall-clock budget was escapable.** A program that leaves a detached
   process holding the inherited stderr pipe kept the reader alive after the
   runtime was killed: a 2 s budget produced a 20.3 s call (`child.wait` and
   the stdout loop were bounded; this join was not). Fixed by bounding the
   join with `REAP_GRACE` and aborting the reader past it.

Non-blocking review notes left as-is (out of scope for this round):
`RunProgramRequest.abort` is still always `None` from `RunCode::execute` (real
Ctrl-C plumbing would need a session-scoped token in the tool), and the
Goal-190 plan-mode guard is still not applied to nested calls (unreachable in
plan mode, since `RunCode` is not read-only).

## Tests added

- `registry::tests::the_hook_gates_the_audited_path_without_a_permissions_config`
  — deny hook + **no** `permissions` config: the audited path denies and still
  yields an audit record; an allow hook flows through with a populated
  `args_hash`.
- `run_code::tests::programmatic_calls_are_gated_by_the_permission_hook`
  (node-free) — `RegistryInvoker` over a deny-hook registry rejects.
- `run_code::tests::a_detached_pipe_holder_cannot_outlast_the_budget`
  (node-gated) — a program that `spawn(detached, stdio: inherit)`s a 20 s child
  is still cut off at its 0.4 s budget (asserts `status=timeout` and < 15 s
  wall clock; pre-fix it returns at ~20 s).

## Verification (round 3)

- `cargo test --lib run_code` — 55 passed, 0 failed (`node v26.10.0` on PATH,
  so the node-gated budget tests ran for real).
- `cargo fmt --all -- --check` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean (8m30s,
  cold tree).
- `cargo test --workspace` — exit 0, **4516 passed / 0 failed** (+3 vs the
  round-2 run: the three tests above), no `FAILED` / `panicked` line.

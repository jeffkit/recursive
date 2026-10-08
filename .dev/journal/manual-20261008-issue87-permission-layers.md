# Issue #87 — request `permission_mode` must not wipe operator permission layers

- **Date**: 2026-10-08
- **Goal**: Issue #87 (P0 security). An HTTP request body carrying `permission_mode`
  replaced the whole permissions config on the session registry
  (`with_permissions(LayeredPermissionsConfig { mode, layers: Vec::new() })`), discarding the
  operator-configured allow/deny/interactive layers loaded from
  `RECURSIVE_TOOL_PERMISSIONS_FILE` / `config.toml [permissions]`. One JSON field could strip a
  server-side deny policy.
- **Fix**: the request-level override now swaps only the *mode* and carries the operator layers
  over, on a fresh `Arc` so the shared base config is never mutated.

## Files touched

- `src/tools/registry.rs` — new `ToolRegistry::with_permission_mode(mode)`: reads the attached
  layers, builds a new config `{ mode, layers }`, and drops it into a fresh `Arc<RwLock<..>>`
  (so a session's requested mode cannot leak into the process-wide base registry).
- `src/http/handlers.rs` — new `apply_request_permission_mode(registry, mode_str, allow_bypass)`
  helper; `run_agent` and `create_session` call it instead of `with_permissions`. The now-unused
  `LayeredPermissionsConfig` import was dropped.
- `src/http/cold_load.rs` — restored sessions go through the same helper (they used the same
  layer-wiping pattern before).

## Tests added

- `tools::registry::tests::with_permission_mode_preserves_rule_layers` — mode swaps, operator deny
  layer survives.
- `tools::registry::tests::with_permission_mode_does_not_mutate_source_registry` — the source
  registry (shared base) keeps its mode.
- `tools::registry::tests::with_permission_mode_on_bare_registry_applies_mode` — no operator config
  → requested mode applied, layer list stays empty.
- `http::handlers::tests::request_permission_mode_keeps_operator_deny_rules` — acceptance:
  operator `deny: ["Bash"]` + request `permission_mode:"default"` → `Bash` still denied.

## Scope notes

- Reachable request modes are `default` / `auto` / `strict` (plus `bypass`, gated by
  `allow_bypass_permissions`). `check_static` consults the static deny rules for all of them, so
  preserving the layers closes the reported bypass.
- The mode itself stays request-settable (e.g. `strict` → `default`). This implements the issue's
  second suggested option ("operator layer as an irremovable base; the request layer stacks on
  top"). The first option ("clients may only tighten") would need an invented mode-strictness
  ordering and is a behaviour change beyond the acceptance criterion.

## Gates

- `cargo fmt --all` ✅
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅
- `cargo clippy --lib --no-default-features -- -D warnings` ✅
- `cargo test --workspace --no-fail-fast` ✅

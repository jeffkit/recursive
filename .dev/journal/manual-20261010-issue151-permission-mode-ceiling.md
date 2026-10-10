# Issue #151 — request `permission_mode` must not loosen the operator mode

- **Date**: 2026-10-10
- **Goal**: Issue #151 (P0 security), the bypass surface left open by issue #87.
  #87 stopped a request from wiping the operator's rule **layers**, but the
  request still replaced the **mode**: `POST /sessions {"permission_mode":
  "default"}` (or any typo) turned an operator `[permissions] mode = "strict"`
  session back into `Default`, where `run_shell` / `Write` / `Edit` match no
  rule, resolve to `Unknown`, and are implicitly allowed (`check_static` only
  denies-by-default in `Strict`). The same JSON field could also drop
  `dontAsk` / `plan`. `parse_permission_mode` folded every unrecognised string
  into `Default`, so the bogus value did not even have to be a real mode.
- **Fix**: the operator mode is now a **ceiling**. A request may tighten it,
  never loosen it; an unknown value — or `bypass` on a server without the
  `RECURSIVE_ALLOW_BYPASS_PERMISSIONS` opt-in — is a 400 with the reason in
  the body, instead of a silent `Default`.

## Files touched

- `src/permissions/mod.rs` — new `PermissionMode::restrictiveness() -> u8`: the
  ladder (`bypass` 0 < `acceptEdits` 1 < `default` 2 < `auto` 3 < `dontAsk` 4 <
  `plan` 5 < `strict` 6), ordered to mirror `check_static`'s own precedence.
- `src/http/handlers.rs` — `parse_permission_mode` now returns
  `Result<PermissionMode, String>` (unknown / gated `bypass` are errors);
  `request_mode_allowed(requested, operator, allow_bypass)` is the pure ceiling
  predicate; `apply_request_permission_mode(&registry, ..)` borrows the source
  registry, returns `Result<ToolRegistry, ApiError>` (400 on refusal), and both
  call sites (`run_agent`, `create_session`) propagate with `?`.
- `src/http/cold_load.rs` — the replay path no longer re-applies a persisted
  mode verbatim: a refused value is logged and dropped, so the session
  restores at the operator (strictest) mode instead of bricking or downgrading.
- `src/http/mod.rs` — `RunRequest` / `CreateSessionRequest` doc comments and
  the OpenAPI `permission_mode` schema now state the ceiling + 400 contract.

## Ceiling semantics (why not a plain total order)

- Equal or stricter on the ladder → accepted (`strict` may tighten `default`;
  re-asserting the same mode is a no-op).
- `auto` ranks *above* `default`: its classifier can deny an otherwise-allowed
  call, so it is not a loosening (also keeps `POST /sessions
  {"permission_mode":"auto"}` working on a default server — issue #98's
  round-trip test).
- `bypass` is weaker than everything, so it is reachable only with the
  operator opt-in — and never over `strict` / `dontAsk` / `plan`, which each
  deny a class of calls `bypass` would run.
- `dontAsk` / `plan` deny along an axis the request vocabulary cannot express,
  so no request value is a superset: only an identical mode is accepted (i.e.
  nothing, since neither is request-parseable). This is what makes the
  "any value must not loosen DontAsk / Plan" requirement hold.

## Tests added

- `permissions::tests::test_permission_mode_restrictiveness_ladder` — every
  variant at a distinct rank, in that exact order (a re-ordering silently
  changes which requests are accepted); the `plan` payload does not affect rank.
- `http::handlers::tests::parse_permission_mode_rejects_unknown_and_gated_bypass`
  — `xyz` / `""` / `"strict "` / `"allow"` are 400s that echo the value;
  `bypass` without the opt-in names the env var.
- `http::handlers::tests::request_mode_allowed_treats_the_operator_mode_as_a_ceiling`
  — the `(operator, requested, allow_bypass)` table, 15 cases.
- `http::handlers::tests::request_permission_mode_cannot_loosen_operator_strict`
  — issue #151 acceptance: operator `mode="strict"` + `allow=["Read","Grep"]`,
  requests `default` / `xyz` / `""` / `bypass` (both `allow_bypass` values) →
  400, session registry still `strict`, `run_shell` still denied.
- `http::handlers::tests::request_permission_mode_tightens_the_operator_mode`
  — the tightening direction still works.
- `http::cold_load::tests::cold_load_keeps_operator_strict_over_a_loosening_persisted_mode`
  — the same acceptance through the replay path, for all four persisted values.
- `http::handlers::tests::request_permission_mode_keeps_operator_deny_rules`
  updated for the new `Result`/`&registry` signature (the #87 guarantee).

Negative control: neutralising `request_mode_allowed` to `return true` makes
both #151 acceptance tests fail (the `Ok(_) => panic!(..)` arm), and the tests
pass again once restored — they pin the fix, not just the code path.

## Scope notes

- The rule layers keep the #87 semantics (preserved, never request-settable) —
  this issue only adds the mode ceiling.
- `allow_bypass_permissions=true` keeps working for a `default`-posture
  operator (the documented opt-in); it is deliberately not treated as an
  override of an explicit `strict` / `dontAsk` / `plan` operator mode.
- Legacy persisted sessions whose stored `permission_mode` is now refused stay
  loadable (warn + operator mode) — a stored blob must not 500 a restart.

## Gates

- `cargo fmt --all` ✅
- `cargo clippy --all-targets --all-features -- -D warnings` ✅
- `cargo test --workspace --no-fail-fast` ✅

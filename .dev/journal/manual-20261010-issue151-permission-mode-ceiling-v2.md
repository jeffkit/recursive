# Issue #151 (revision 2) — the request-mode ceiling is an explicit policy table

- **Date**: 2026-10-10
- **Goal**: Issue #151 implementation spec (补充 · 2026-10-10). The first landing
  (`b0441cae`) fixed the bypass — a request can no longer replace the operator
  mode with a looser one — but implemented the ceiling as a **numeric ladder**
  (`PermissionMode::restrictiveness()`, `>=` comparison), which the spec
  explicitly rules out: the modes are not totally ordered (`plan` blocks every
  write, `dontAsk` denies the interactive tools — neither contains the other),
  so a rank comparison silently accepts pairs that do not actually narrow.
  Revision 1 replaced the comparison with the spec's explicit policy table and
  added the request vocabulary the table is written against. **Revision 2**
  (this one) fixes the table rows an independent review found to be unsound,
  the `plan` payload anchoring, and the docs.

## Files touched

- `src/permissions/mod.rs` — `request_mode_allowed(operator, requested) -> bool`:
  the ceiling as an explicit `(operator, requested)` table, plus a table-driven
  unit test. `restrictiveness()` is kept only as the informal ranking it is —
  the policy no longer reads it.
- `src/http/handlers.rs` — the private ladder-based `request_mode_allowed` is
  deleted; `apply_request_permission_mode` calls
  `crate::permissions::request_mode_allowed(&operator, &requested)`. On an
  accepted `plan` request the payload (`pre_plan_mode` / `bypass_available`) is
  re-anchored to the operator mode, so a request cannot choose which mode
  leaving `plan` falls back to, nor whether plan mode blocks writes.
  `parse_permission_mode` also accepts `acceptEdits` / `dontAsk` / `plan`
  (case-insensitive, snake_case spellings included), so the ceiling can be
  expressed over the whole vocabulary, and its error now lists every accepted
  value.
- `src/http/cold_load.rs` — acceptance test renamed to
  `cold_load_cannot_replay_looser_mode` and widened (`acceptEdits` / `auto` /
  `dontAsk` / `plan` added to the refused persisted values).
- `src/http/mod.rs` — OpenAPI `permission_mode` enum + descriptions (both
  request schemas, and `GET /sessions/:id`) list the full vocabulary and the
  `bypass` caveat.
- `src/config.rs` — `allow_bypass_permissions` doc: the flag makes `"bypass"`
  *parseable*; it is not an override of the operator ceiling.

## Policy (why these pairs)

The table is written against the denials `check_static` adds on top of the rule
layers (which a request never touches):

| mode | denies, beyond the rules |
|---|---|
| `bypass` | nothing — it skips the rule checks as well |
| `acceptEdits` | nothing, but *allows* writes the rules would deny |
| `default` / `auto` / `plan{bypass_available: true}` | nothing |
| `plan` | every write (bar `exit_plan_mode`) |
| `dontAsk` | every interactive tool |
| `strict` | everything without an explicit allow rule |

A requested mode is admitted only when it denies at least everything the
operator mode denies:

- `strict` → `strict` only. Its catch-all deny exists *only while the mode is
  `strict`*, so `dontAsk` / `plan` / `default` all let an unlisted
  (non-interactive, read-only, …) tool through — the widening the issue is
  about.
- `dontAsk` → `dontAsk` only; `plan` (write-blocked) → `plan` (write-blocked)
  only. The three axes are pairwise incomparable.
- `plan{bypass_available: true}` is `default` again, so it accepts everything
  that narrows `default`.
- `default` → anything but `acceptEdits` (`acceptEdits` auto-allows writes
  *before* the deny rules run → wider).
- `acceptEdits` → anything (it is the widest mode that still runs the rules, so
  even `default` is a narrowing: it drops the write auto-approval).
- `auto` as operator → `auto` / `strict` / `dontAsk` / `plan`; the classifier
  can only deny, and dropping it for `default` / `acceptEdits` widens.
- `bypass` is strictly weaker than everything → only the operator may hand it
  out; a request for it is refused even with `RECURSIVE_ALLOW_BYPASS_PERMISSIONS`
  set (the opt-in makes the value parseable, the ceiling decides whether it is
  honoured).
- Re-asserting the operator's own mode is always allowed (not a loosening).

## Tests

- `permissions::tests::request_mode_allowed_policy_table` — 51
  `(operator, requested)` pairs pinned, `plan` payloads included.
- `http::handlers::tests::parse_permission_mode_all_variants` extended with the
  new spellings; `request_mode_cannot_loosen_strict` (now also covers
  `dontAsk` / `plan` over a `strict` operator) / `request_mode_can_tighten_default` /
  `request_mode_can_tighten_acceptedits` / `request_mode_rejects_acceptedits_over_default` /
  `unknown_mode_is_bad_request` / `request_plan_is_anchored_to_the_operator_mode`
  cover the HTTP surface.
- `http::cold_load::tests::cold_load_cannot_replay_looser_mode` — the replay
  path cannot restore a mode the live path would refuse (`dontAsk` / `plan`
  included).
- `#87`'s `request_permission_mode_keeps_operator_deny_rules` still passes: the
  revision only changes how the mode is admitted, never whether the rule layers
  survive.

## Gates

- `cargo fmt --all` ✅
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅
- `cargo test --workspace --no-fail-fast` ✅

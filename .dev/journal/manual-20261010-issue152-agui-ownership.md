# Issue #152 — AG-UI threads must carry session ownership (the #85 gap)

- **Date**: 2026-10-10
- **Goal**: Issue #152 (P0 security). #85 gave `/sessions/:id*` an ownership
  model (`may_access_session` over the owner's subject + tenant), but the two
  AG-UI entry points never took an identity at all: `agui_run` and
  `agui_cancel` had no `Extension<AuthIdentity>`. The thread directory is
  derived only from the client-chosen `threadId` (`agui_session.rs`,
  `blake3(thread_id)`), and after #147 each run seeds itself from that
  directory's persisted `transcript.jsonl`. So tenant B could POST
  `/agui {"threadId": "<A's id>"}` with B's own valid credential, get seeded
  with A's full history, append to A's transcript — and `POST
  /agui/<id>/cancel` let any credential holder stop A's in-flight run.
- **Fix**: AG-UI threads now record their creator on first write and enforce
  the same `may_access_session` rule `/sessions` uses. A thread with no
  `.meta.json` yet is new (nothing to leak); an existing thread is reachable
  by its recorded owner or an admin, `403` otherwise, checked **before**
  `prepare_run` reads the transcript. When auth is disabled there is no
  identity, so the legacy single-user pass-through is preserved.

## Files touched

- `src/session/mod.rs` — `SessionMeta` gains backward-compatible
  `owner` / `tenant` (`#[serde(default, skip_serializing_if = "Option::is_none")`,
  no schema bump); the shape mirrors the #85 `cold_load::SessionMeta`.
- `src/session/writer.rs` — new `SessionWriter::set_owner(owner, tenant)`
  (read-modify-write on `.meta.json`, like `set_derived_from` / `update_identity`),
  plus `owner: None, tenant: None` in the two `SessionMeta` constructors.
- `src/agui_session.rs` — `open_thread_writer` takes `owner` / `tenant` and
  calls `set_owner` **only when it creates** the thread's meta (an existing
  owner is sticky — access was already decided against it). Migrated pre-#57
  threads synthesize `owner: None` (admins-only, the #85 default-deny).
- `src/http/agui.rs` — `AguiRunContext` carries `owner` / `tenant`;
  `spawn_agui_run` registers the cancel token under
  `thread_session_key` (the same namespace as the run fence and the thread
  directory) instead of the raw thread id.
- `src/http/handlers.rs` — `ensure_agui_thread_access` (the single AG-UI
  authorisation point) + `agui_api_error_response`; `agui_run` /
  `agui_cancel` extract `Option<Extension<AuthIdentity>>` and call it; the
  cancel lookup is now keyed by `thread_session_key`.
- `src/http/mod.rs` — doc comment on `AppState::agui_active_runs` (key changed).
- `src/http/session_mirror.rs`, `src/workspace/{activity,registry}.rs` — the
  remaining `SessionMeta` literals get the new fields (`None`).
- `tests/agui_auth.rs` (new) — end-to-end acceptance over the real router.

## Tests added

- `tests/agui_auth.rs`: `agui_run_rejects_other_tenant_thread` (403 + A's
  transcript byte-identical), `agui_cancel_rejects_other_tenant` (403, A's
  token untouched; admin still cancels), `agui_admin_can_access_any_thread`,
  `agui_ownership_survives_restart` (fresh `AppState`, same workspace),
  `agui_single_user_auth_disabled_still_runs`.
- `src/http/handlers.rs::agui_thread_access_enforces_owner_and_admin` — the
  pure decision: owner ok, foreign denied, admin ok, `None` identity
  pass-through, unattributed thread open to its first caller.
- `src/agui_session.rs`: `a_new_thread_records_its_owner_and_keeps_it`,
  `owner_survives_finalize`.

## Notes

- **Migration semantics**: threads written before #152 carry no `owner`, so
  they become **admins-only** — the same rule #85 applied to unattributed
  sessions. A non-admin will get 403 on their own pre-#152 AG-UI thread; an
  admin can still use it. Documented here deliberately.
- **Ordering**: the run fence is taken before the ownership check, so the
  check and the thread's first write cannot be interleaved by a competing
  run. A foreign *run* racing an in-flight owner run gets 409 (busy) rather
  than 403 (no transcript is read either way); `cancel` has no fence and
  always answers 403.
- No new dependencies. `cargo fmt` / `clippy --all-targets --all-features -D warnings`
  / targeted test binaries all clean.

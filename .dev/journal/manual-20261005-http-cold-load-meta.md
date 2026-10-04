# manual-20261005 — http cold-load session restore parity (#98)

- Date: 2026-10-05
- Goal / issue: #98 fix(http): 冷加载恢复的会话无 compactor/streaming/storage
  —— system_prompt/permission_mode 被静默替换 (P1)
- Base HEAD: 22a973e9

## Files touched

- `src/http/cold_load.rs`
  - New `SessionMeta` (serde) + `session_meta_key` / `persist_session_meta` /
    `load_session_meta` / `update_persisted_title`. Persisted in the storage
    backend's generic KV space (`session-meta/<id>`, same space as the delete
    tombstone) so it works for local + S3. Load/parse failures degrade to
    `None` (server defaults), never an error.
  - `build_restored_runtime` now takes `Option<&SessionMeta>` and routes
    through `handlers::build_session_runtime` — so a restored session gets the
    compactor / microcompactor / transcript cap (`apply_context_management`),
    `.streaming(true)` and `.storage(...)` that fresh sessions get. Previously
    it assembled a bare `AgentRuntimeBuilder` (no compaction, no streaming, no
    persistence).
  - Restores the persisted `system_prompt`, `permission_mode` (re-parsed
    through `parse_permission_mode`, so `allow_bypass_permissions` still
    gates it), `title` and `max_steps`.
- `src/http/handlers.rs`
  - `build_session_runtime` / `parse_permission_mode` → `pub(super)` so cold
    load reuses the single build/parse path.
  - New `permission_mode_label` (API vocabulary).
  - `create_session` persists `SessionMeta` (only a request-supplied
    `system_prompt` / `append_system_prompt` is frozen; a default session keeps
    tracking the server default).
  - `patch_session` mirrors a new title into the persisted metadata (IO moved
    outside the sessions lock).
  - `GET /sessions/:id` reports `permission_mode` (read off the live registry).
- `src/http/mod.rs` — `SessionDetailResponse.permission_mode` + OpenAPI schema.
- `src/runtime.rs` — `#[cfg(test)] has_compactor()` (deterministic assertion).
- `tests/http_cold_load.rs` — restart acceptance test.

## Tests added

- `src/http/cold_load.rs` (unit):
  - `session_meta_roundtrips_through_the_storage_kv`
  - `update_persisted_title_keeps_the_other_fields`
  - `cold_load_restores_custom_prompt_mode_and_context_management`
    (custom prompt + `auto` mode survive; restored runtime has a compactor and
    actually compacts a long transcript)
  - `cold_load_ignores_persisted_bypass_when_server_disallows_it`
  - extended `cold_load_restores_single_system_and_valid_pairing` to pin the
    server-default fallback when no meta exists.
- `src/http/handlers.rs` (unit): `permission_mode_label_covers_every_variant`.
- `tests/http_cold_load.rs` (integration):
  `restart_preserves_custom_prompt_and_permission_mode` — create with custom
  prompt + `auto`, run a turn, `flush_all_sessions` (restart), GET the
  restored session and assert BOTH the prompt and the mode.

## Notes

- The issue text suggests `.meta.json`; this repo persists through the
  `StorageBackend` KV space instead (backend-neutral, works for S3 too) — the
  intent (per-session metadata round-trips) is the same.
- `created_at` stays synthesized: presentation metadata, not a per-session
  override.

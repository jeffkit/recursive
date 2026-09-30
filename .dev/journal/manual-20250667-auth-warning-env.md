# Manual change — issue #67: HTTP auth warning read wrong env vars

Date: 2025-06-XX
Goal: Fix the `recursive http` startup auth warning, which read
`RECURSIVE_API_KEY` (outbound LLM key) / non-existent `RECURSIVE_JWT_SECRET`
instead of the inbound `RECURSIVE_HTTP_AUTH_KEYS` /
`RECURSIVE_HTTP_AUTH_JWT_SECRET`, producing false negatives ("auth off"
warning never fired when inbound auth WAS configured, and could stay silent
when it wasn't).

## Files touched
- `src/http/auth.rs` — added `pub const ENV_AUTH_KEYS` /
  `ENV_AUTH_JWT_SECRET`; `auth_config_from_env` now reads through them.
- `src/http/mod.rs` — re-export the two constants.
- `crates/recursive-cli/src/main.rs` — startup check uses the exported
  constants; warning text names the correct vars; added single combined
  env-mutation test `auth_warning_tracks_inbound_env_vars_in_one_test`
  (asserts: no inbound creds + outbound key set → warning; keys set → none;
  JWT only → none). Existing warning-text test updated.

## CORS
Not added — the issue marks it optional and lower priority than ①; web-chat
ships a same-origin gateway (`proxy.mjs`), which is the recommended
deployment. Revisit if cross-origin browser deployment becomes a
requirement.

## Tests
- `cargo test -p recursive-cli --bin recursive auth` → 4 passed.
- root `cargo test --lib` → 2384 passed.
- `cargo clippy --all-targets --all-features --workspace -- -D warnings` clean;
  `cargo fmt --all` applied.

## Notes
- Env-var test deliberately merged into ONE test (`.dev/AGENTS.md` lesson:
  parallel `set_var`/`remove_var` races).
- Full `cargo test --workspace` not run to completion (long); lib + CLI bin
  suites plus clippy cover the change surface.

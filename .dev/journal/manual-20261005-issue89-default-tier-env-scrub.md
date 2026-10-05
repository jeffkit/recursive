# manual-20261005 — issue #89: default-tier shell env scrub

**Date:** 2026-10-05
**Goal:** #89 security(exec) — the default (`RECURSIVE_SANDBOX` unset /
`none` / `policy`) tier handed the whole service-process env to `sh -c`,
so a prompt-injected `printenv` shipped the upstream LLM key
(`RECURSIVE_API_KEY`), the inbound HTTP auth keys
(`RECURSIVE_HTTP_AUTH_KEYS`) and the JWT signing secret
(`RECURSIVE_HTTP_AUTH_JWT_SECRET`) into the plaintext transcript.

## Files touched

- `src/tools/transport_layer/transport.rs`
  - new `fn is_sensitive_env_var(&str) -> bool` — drops the `RECURSIVE_`
    namespace and names containing `KEY`/`SECRET`/`TOKEN`/`PASSWORD`/
    `PASSWD`/`CREDENTIAL` or an `AUTH` `_`-delimited segment.
  - new `pub(crate) fn scrub_child_env(&mut Command)` — the shared
    `env_clear` + non-sensitive re-add.
  - `LocalTransport::exec_shell` calls `scrub_child_env`, then applies the
    tool call's explicit `env` pairs on top (always win).
- `src/tasks/run_background.rs` — the legacy **host path** (a
  `RunBackground` built without a transport) also calls `scrub_child_env`,
  closing the same `printenv` channel there.
- `src/tools/execution/shell.rs` — `Bash` `env` arg schema now states the
  local-tier credential scrub (wording still satisfies
  `env_schema_description_matches_per_tier_reality`).
- `docs/architecture/execution-environments.md` — capability matrix row,
  "Environment-variable invariant" section, `none`-tier threat-model
  bullet.
- `.dev/AGENTS.md` — invariant #3 extension gains the issue #89 note.
- `tests/issue51_sandbox_env_inheritance.rs` — stale "inherits host env by
  design" comment corrected.

## Tests added

- `local_transport_exec_shell_scrubs_sensitive_env`
  (`src/tools/transport_layer/transport.rs`) — one consolidated env test
  (`.dev/AGENTS.md` rule): asserts the literal acceptance
  `printenv | grep -c RECURSIVE` == 0, that `OPENAI_API_KEY` /
  `AWS_SECRET_ACCESS_KEY` / `GITHUB_TOKEN` / `DB_PASSWORD` /
  `SSH_AUTH_SOCK` are absent, and that `CARGO_TEST_BENIGN_VAR` /
  `GIT_AUTHOR_NAME` survive (the latter pins that the `AUTH` segment
  match does not swallow `AUTHOR`).
- `run_background_host_path_scrubs_sensitive_env`
  (`src/tasks/run_background.rs`) — same acceptance via the host path.
- `cargo test -p recursive-agent --lib transport::tests`: 17 passed.

## Notes

- The scrub lives in the **transport**, not in `RunShell::execute`'s
  `env_pairs`, so the issue #51 contract (sandbox transports receive only
  the explicit pairs) is untouched.
- `RECURSIVE_*` is stripped wholesale (the agent's own credentialed
  namespace); child commands that need it must be given the value
  explicitly.
- No new deps.

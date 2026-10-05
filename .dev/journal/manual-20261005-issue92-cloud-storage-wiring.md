# Manual — issue #92: HTTP cloud-storage wiring (S3) + honest docs

- **Date**: 2026-10-05
- **Issue**: #92 `feat(http): cloud-runtime 在 http 模式被显式拒绝接线 + transcript 仅 teardown 落盘`
- **Worktree**: `.flowcast/runs/pipeline-92-1005072749/worktree` (branch `v2-pipeline-92-1005072749`)
- **Baseline**: `9165b9a0` (worktree base includes Goal 396/397/398 on top)

## What the gap actually was

Post-Goal-396/397 the HTTP server *did* persist transcripts, but it always used
`LocalStorageBackend`: `crates/recursive-cli/src/main.rs` detected
`RECURSIVE_REDIS_URL` / `RECURSIVE_S3_BUCKET`, printed "recognized but not yet
wired in http mode", and hard-coded the local backend. The bundled compose stack
was doubly broken — the `Dockerfile` built `recursive-cli --features http` only,
so the `cloud-runtime` modules were not even compiled in, and the env vars were
inert.

## Scope decision (what this change does and does not do)

The issue offers three suggestions; taken literally all three (wire Redis + S3,
append-only per-turn persistence, externalise the session table) is three goals,
not one. This change lands the part that is contained, verifiable and actually
removes a user-visible lie, and documents the rest precisely:

1. **S3 — wired.** `RECURSIVE_S3_BUCKET` (feature `cloud-runtime`) now selects
   `S3StorageBackend` for the HTTP server's transcript / memory / session-metadata
   storage. Combined with Goal 397 cold load, a session torn down on one pod is
   now restorable on a sibling replica that shares the bucket — the first real
   step toward the "stateless pods" story the README promised.
2. **Redis — deliberately NOT wired, now documented as such.** `SessionStore`
   (`RedisSessionStore`) is held by the kernel but *never called* — the only
   production references to `save_state`/`load_state`/`delete_state` are the
   trait definition and its tests (`rg "save_state\(" src/` finds no caller).
   Injecting a `RedisSessionStore` into the HTTP runtime would therefore create a
   connection pool that is never used — a no-op that would make the docs *more*
   wrong, not less. Instead the server logs a precise note when
   `RECURSIVE_REDIS_URL` is set, and the docs point at
   `AgentRuntimeBuilder::session_store` as the library-API path. Goal 396's own
   note ("half-wired cloud storage is worse than none") is the rationale.
3. **Per-turn / incremental persistence — unchanged.** Still teardown-only
   (DELETE / idle eviction / graceful shutdown), matching Goal 396's documented
   tradeoff (full-overwrite `save_transcript`; per-turn would be O(N²) or need a
   new append API on every backend). Documented in README + website, not silently
   claimed.

## Files touched

| File | Change |
|------|--------|
| `src/storage/mod.rs` | `ENV_S3_*`/`ENV_REDIS_URL` consts, `HttpStorage` enum, pure `select_http_storage` (+ 5 unit tests), cfg-dispatched `http_storage_backend` (builds `S3StorageBackend` when the feature is on), `warn_unwired_cloud_env` |
| `src/lib.rs` | re-export `http_storage_backend` / `select_http_storage` / `warn_unwired_cloud_env` / `HttpStorage` |
| `crates/recursive-cli/src/main.rs` | drop the print-only env loop + hard-coded `LocalStorageBackend`; call `storage::http_storage_backend(user_workspace_dir(&config.workspace)?)`; new `#[tokio::test]` pinning the local-fallback contract |
| `src/runtime/builder.rs`, `src/http/mod.rs` | doc comments: S3 selection + why Redis is not injected |
| `Dockerfile` | `ARG FEATURES=http` → `--features "$FEATURES"` (default keeps the e2e image lean; compose overrides) |
| `docker-compose.yml` | `FEATURES: "http,cloud-runtime"`; header/env comments truthful about Redis |
| `README.md` | head bullet, §Full cloud stack, Redis/S3 env-table notes, cheatsheet |
| `website/{en,zh}/deployment/cloud.md`, `docker.md`, `website/{en,zh}/guide/config.md` | same corrections (multi-replica now says sticky sessions required; Docker needs the feature) |
| `.env.example`, `CHANGELOG.md` | notes + Unreleased entry |

## Design notes / traps handled

- **Testability without the feature / a live S3.** `cargo test --workspace` and
  `agent-mutants.sh` both build *without* `cloud-runtime`, so the selection logic
  was factored into a pure `select_http_storage(&dyn Fn(&str)->Option<String>)`
  that is fully unit-tested (bucket, prefix/tenant overrides, blank handling).
  The env-reading wrapper and the logging helper are `#[cfg_attr(test,
  mutants::skip)]` — environment plumbing with no pinnable behaviour, following
  the existing `src/llm/openai.rs` precedent for warn-only helpers. The CLI crate
  gets a black-box-ish `#[tokio::test]` (`http_storage_backend_defaults_to_local_filesystem`)
  that pins the exact contract `main.rs` depends on.
- **No new module / no untracked file in the mutants scope.** The S3
  construction is inlined into the skip-marked `http_storage_backend` rather than
  living in its own `storage/cloud.rs`; a new untracked file would be invisible
  to the gate scopes on the first pass yet enter `agent-mutants`'s `main...HEAD`
  scope after a rebase, where its untestable S3 arm could surface as a survivor.
- **`cargo-mutants` only honours `#[cfg_attr(test, mutants::skip)]`**, not the
  `// cargo-mutants::skip` comment form (see
  `.dev/journal/manual-20260804-agent-mutants-58-missed.md`); used the attribute.
- **No new crates.** S3/Redis deps already existed behind `cloud-runtime`;
  this change adds no dependency.
- **Empty-key handling.** A blank `RECURSIVE_S3_BUCKET` stays local (compose /
  k8s often pass an empty default through), rather than constructing an S3
  backend with an empty bucket name.

## Tests added

- `src/storage/mod.rs::tests`: `select_http_storage_defaults_to_local_when_unset`,
  `..._reads_bucket_and_fills_defaults`, `..._honours_prefix_and_tenant_overrides`,
  `..._ignores_blank_bucket`, `..._blank_prefix_and_tenant_fall_back_to_defaults`.
- `crates/recursive-cli/src/main.rs::tests::http_storage_backend_defaults_to_local_filesystem`
  — pins the exact contract the CLI's `recursive http` startup depends on
  (local layout `<workspace>/.recursive/sessions/<id>.jsonl` + round-trip).

## Quality gates

On the final tree, in the worktree:

- `cargo fmt --all -- --check` — OK.
- `cargo clippy --all-targets --all-features -- -D warnings` — exit 0 (also
  compiles the `cloud-runtime` S3 arm).
- `cargo test --workspace` — exit 0.
- Presence gates: `agent-test-presence.sh` PASS, `cli-test-presence.sh` PASS.

Not run here: the Docker e2e gate (needs a release binary + Docker image build).
The change does not alter e2e behaviour — the Dockerfile's `FEATURES` arg
defaults to `http`, matching the previous hard-coded value, and no e2e suite sets
`RECURSIVE_S3_BUCKET`.

## Follow-ups (not in scope)

- Per-turn / append-only transcript persistence (issue #92 suggestion 2) — needs
  an append API on `StorageBackend` (local: append JSONL; S3: multipart or
  read-modify-write, so the design has to be backend-specific).
- Redis as a shared session table so round-robin replicas can find in-flight
  sessions without sticky routing (issue #92 suggestion 3).
- Wire `SessionStore` into the kernel's per-turn checkpointing so
  `RedisSessionStore` stops being dead code.

## Review fixes (2026-10-05)

Independent review returned NEEDS_FIX — the "honest docs" sweep missed four
sibling pages that still carried the Redis/stateless claim the change had just
declared false:

- `website/{en,zh}/deployment/index.md` — the deployment landing page's
  feature-comparison table (`Session hot-state` / `Horizontal scaling`) now
  mirrors `README.md:381-387` and `cloud.md` (sticky sessions, S3 teardown
  timing).
- `website/{en,zh}/cli/sessions.md` — dropped "stored in Redis and replicated to
  S3"; now states `recursive http` writes to S3 when `RECURSIVE_S3_BUCKET` is set
  and does not consume Redis.
- `website/{en,zh}/cli/http.md` — dropped "stateless … add Redis and S3 for
  horizontal scaling".
- `website/.vitepress/config.ts` — sidebar labels reordered to `S3 + Redis` to
  match the `cloud.md` titles updated by this change.

Minor review items, also fixed:

- `src/storage/mod.rs::warn_unwired_cloud_env` now routes both checks through
  `non_blank`, so a blank `RECURSIVE_S3_BUCKET` (what `.env.example` ships) no
  longer logs a spurious "set but … using LocalStorageBackend" warning.
- The Redis notice was promoted `info!` → `warn!` so it survives
  `--log warn` / `RUST_LOG=warn`, matching `README.md`'s unqualified "logs a note".
- `.dev/mutant-debt-20260709-agent.md` "Accepted non-debt" extended with the two
  new `#[cfg_attr(test, mutants::skip)]` helpers.

Gates re-run on the fixed tree: `cargo fmt --all -- --check`, `cargo clippy
--all-targets --all-features -- -D warnings`, `cargo test --workspace` — green.

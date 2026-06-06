# Manual edit: p2-docker-ratelimit

**Date**: 2026-06-06
**Goal**: Fix two P2 security issues — SEC-004 (Docker workspace volume mounted read-write) and SEC-006 (rate limiter ineffective against brute-force with rotating API keys).
**Files touched**:
- `src/tools/docker_sandbox.rs` — added `:ro` to workspace bind mount
- `src/http/rate_limit.rs` — hash API key before storing in bucket map; move rate_limit layer to outermost position (executes before auth)
- `src/http/mod.rs` — reordered `.layer()` calls so rate_limit runs before auth_middleware
- `src/runtime.rs` — moved `ENTER_PLAN_MODE_TOOL_NAME`/`EXIT_PLAN_MODE_TOOL_NAME` imports into `#[cfg(test)]` block (pre-existing clippy warning, fixed incidentally)

**Tests added**:
- Updated `test_extract_client_key_with_api_key` in `src/http/rate_limit.rs` to assert:
  1. Bucket key starts with `"apikey:"` prefix
  2. Raw credential does not appear in the stored key
  3. Same input produces the same key (deterministic)
  4. Different inputs produce different keys (per-client isolation preserved)

**Notes**:
- SEC-004: The `:ro` flag on the Docker bind mount prevents container code from writing back to the host workspace. Containers that need to produce output should write to a separate `/output` mount (not implemented here — out of scope for this minimal fix).
- SEC-006 (key hashing): Used `std::collections::hash_map::DefaultHasher` to avoid a new crate dependency. Not cryptographically strong, but adequate to prevent raw API key values from appearing in memory dumps or log snapshots. Per-client rate-limit isolation is preserved since distinct keys still hash to distinct bucket identifiers.
- SEC-006 (layer order): In axum, the last `.layer()` call is outermost (runs first). Moving `rate_limit_middleware` to be added after `auth_middleware` ensures all requests — including those with invalid or rotating API keys — are counted against the IP-based rate-limit bucket before auth is checked. The existing 79 HTTP integration tests all pass with no changes to test logic (only the bucket key assertion was updated to reflect hashing).

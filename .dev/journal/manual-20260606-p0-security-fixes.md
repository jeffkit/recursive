# Manual edit: p0-security-fixes

**Date**: 2026-06-06
**Goal**: Fix 5 P0-severity issues identified in the architecture review
**Files touched**:
- `src/run_core.rs` — C-1: parallel tool audit mis-attribution
- `src/tools/web_fetch.rs` — SEC-001: SSRF private IP filtering
- `src/http/auth.rs` — SEC-003: default auth-disabled silent pass-through
- `src/session_lock.rs` — C1-storage: TOCTOU race in lock acquisition
- `src/http/mod.rs` — B1-interface: no request body size limit (OOM vector)

**Tests added**:
- `web_fetch.rs`: `validate_url_blocks_ssrf_targets` covering localhost, 127.x, 169.254.x, RFC-1918, IPv6 loopback, GCP metadata endpoint

**Notes**:
- `run_core.rs` C-1: replaced `Vec::iter().find()` with `HashMap::get()` for O(1) audit lookup by tool_call_id. The linear find always hit the first matching id in a batch, causing silent audit mis-attribution when multiple parallel calls had the same name but different ids.
- `web_fetch.rs` SEC-001: added `is_private_ip()` helper and hostname-level blocklist (localhost, metadata.google.internal). DNS-based filtering would require a custom resolver; hostname/IP-literal filtering covers the common injection vectors without breaking the reqwest client API.
- `http/auth.rs` SEC-003: `is_valid()` now returns `false` for empty key set (was `true`). The middleware's `is_enabled()` check handles the legitimate "auth disabled = pass-through" path cleanly. Added startup `tracing::warn!` when auth is disabled.
- `session_lock.rs` C1-storage: replaced `is_file()` + `write()` with `OpenOptions::create_new(true)` for atomic exclusive creation. Falls back to reading existing sentinel on `AlreadyExists`, preserving stale-lock recovery.
- `http/mod.rs` B1-interface: added `DefaultBodyLimit::max(1 MiB)` layer and `MAX_BODY_BYTES` constant.

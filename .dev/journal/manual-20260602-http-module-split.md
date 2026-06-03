# Manual edit: http-module-split

**Date**: 2026-06-02
**Goal**: Goal 215 — convert src/http.rs (2666 lines) into a src/http/ directory module
**Files touched**:
- `src/http/auth.rs` (new) — AuthConfig, JwtConfig, auth_config_from_env, auth_middleware
- `src/http/rate_limit.rs` (new) — RateLimiter, TokenBucket, rate_limiter_from_env, extract_client_key, metrics_middleware, rate_limit_middleware + rate-limiter unit tests
- `src/http/handlers.rs` (new, renamed from http.rs) — all HTTP handler functions, AguiConverter, map_agent_event, helper fns + SSE/agui unit tests
- `src/http/mod.rs` (new) — AppState, all public types, SseEvent, SseContentBlock, build_router*, build_openapi_spec, Metrics
- `src/http.rs` (deleted)

**Tests added**: none (existing tests moved into rate_limit.rs and handlers.rs)
**Notes**: Public API unchanged — crate::http::AuthConfig, AppState, build_router* resolve identically. Two build errors fixed: private struct types (PatchSessionRequest, ForkSessionResponse, PlanConfirmRequest, PlanRejectRequest) needed pub(super) visibility; unused SseContentBlock import in test module needed removal.

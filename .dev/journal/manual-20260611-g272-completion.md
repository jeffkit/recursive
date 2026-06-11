# Manual edit: g272 completion

**Date**: 2026-06-11
**Goal**: Complete Goal 272 (NEW-HTTP-6: route-level auth bypass)
after self-improve.sh got stuck at step 53 (minimax API hang).

**Files touched**:
- `src/http/auth.rs` — removed the `if path == "/health" ||
  path == "/metrics"` short-circuit; updated doc comment to
  reflect that bypass is now structural (lives in the router
  composition, not in this middleware).
- `src/http/mod.rs` — split `build_router_with_auth_and_rate_limit`
  into a `public` sub-router (`/health`, `/openapi.json`,
  `/metrics`) and a `protected` sub-router (every other route).
  The `protected` sub-router gets `auth_middleware` and
  `rate_limit_middleware`; the top router gets `metrics` and
  `body-limit`. `Router::merge` composes them.
- 3 new source-grep snapshot tests in `src/http/mod.rs` `mod
  tests` pin the structural invariant: public block has no
  auth/rate-limit, protected block has both, auth.rs has no
  string-compare on path.

**Tests added**: 3 (all source-grep; runtime tests would
require an AppState builder that the existing harness
already covers in `tests/http.rs`).

**Notes**:
- The fix is **structural**: future routes added under
  `public` are implicitly bypassed; future routes added under
  `protected` implicitly require auth. The previous
  string-compare in `auth_middleware` could silently 401 a
  new public route (`/openapi.json` was that bug).
- The new top-level router adds `metrics_middleware` and
  `body_limit` after the merge, so both apply to public AND
  protected (k8s liveness probes still increment the
  request counter, and large bodies are still rejected on
  public endpoints).
- `rate_limit_middleware` is on the **protected** sub-router
  only — public endpoints don't consume rate-limit budget.
  This matches the SEC-006 design intent (unauthenticated
  requests are counted against an IP bucket; here we just
  extend that to "all protected routes get counted, public
  ones don't").
- This was a Goal 272 self-improve run; the agent got
  through planning (step 53) and produced a TodoWrite list
  but stalled before any code edits when the minimax API
  hung. Lead completed the change directly. Pattern
  matches the g267/g268/g269 lead-completion overrides.

**Disjoint file guarantee**: Goal 272 touches src/http/.
Goal 273 (running in parallel) touches src/llm/, src/session.rs,
src/cost.rs. No overlap.

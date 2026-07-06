//! Authentication middleware and configuration for the HTTP server.
//!
//! Provides API-key and JWT bearer-token authentication. When no
//! credentials are configured (the default), the middleware returns
//! 503 Service Unavailable unless the explicit debug escape hatch
//! `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` is set. The escape hatch is
//! **honoured only in debug builds** (`cargo run`, `cargo test`); a
//! release build silently ignores it and returns 503 instead, so a
//! Docker image or `--release` binary cannot be tricked into running
//! unauthenticated by an operator "temporarily" setting the env var
//! in production.

use axum::http::StatusCode;
use std::sync::Arc;

/// API key authentication for the HTTP server.
///
/// Configured from `RECURSIVE_HTTP_AUTH_KEYS`, a comma-separated list of
/// keys the server will accept in the `X-API-Key` request header. An empty
/// key set (the default) disables auth entirely — every route is reachable
/// without credentials. This preserves zero-config behavior and keeps the
/// public default backward-compatible.
///
/// Distinct from `RECURSIVE_API_KEY` (singular): that variable holds the
/// **outbound** credential the agent uses to talk to its LLM provider.
/// `RECURSIVE_HTTP_AUTH_KEYS` (plural) holds the **inbound** credentials
/// the HTTP server accepts from clients. The names are deliberately
/// dissimilar to avoid confusion at the operator's shell.
///
/// `/health` and `/metrics` are always exempt (k8s liveness probes and
/// Prometheus scrapers must work unauthenticated).
#[derive(Clone, Default)]
pub struct AuthConfig {
    pub(super) keys: Arc<Vec<String>>,
    pub(super) jwt: Option<JwtConfig>,
}

impl AuthConfig {
    /// Build an `AuthConfig` from an explicit key list. Pass an empty
    /// vec to disable API-key auth (a JWT verifier may still be
    /// attached via [`AuthConfig::with_jwt`]).
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
            jwt: None,
        }
    }

    /// Attach a JWT verifier. Call after [`AuthConfig::new`] to get
    /// "X-API-Key OR Bearer JWT" semantics — either valid credential
    /// type lets a request through. Without this call the behavior
    /// is X-API-Key-only (the original g135 behavior).
    pub fn with_jwt(mut self, jwt: JwtConfig) -> Self {
        self.jwt = Some(jwt);
        self
    }

    /// Constant-time check whether `presented` is in the configured
    /// API-key set.
    ///
    /// Returns `false` when no API keys are configured — callers must
    /// use [`AuthConfig::is_enabled`] first to detect the "auth
    /// disabled" pass-through mode (handled by [`auth_middleware`]).
    ///
    /// The loop runs over **every** configured key regardless of an
    /// early match, to keep the comparison constant-time and avoid
    /// leaking key-set membership timing.
    pub fn is_valid(&self, presented: &str) -> bool {
        if self.keys.is_empty() {
            return false;
        }
        let mut found = false;
        let presented_bytes = presented.as_bytes();
        for k in self.keys.iter() {
            let k_bytes = k.as_bytes();
            if k_bytes.len() != presented_bytes.len() {
                continue;
            }
            let mut diff: u8 = 0;
            for (a, b) in k_bytes.iter().zip(presented_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                found = true;
            }
        }
        found
    }

    /// Whether ANY auth modality is enabled — non-empty API key set
    /// OR a JWT verifier attached. When this returns `false`, the
    /// middleware is a pass-through.
    pub fn is_enabled(&self) -> bool {
        !self.keys.is_empty() || self.jwt.is_some()
    }
}

/// JWT bearer token verification config.
///
/// Verify-only: this server validates tokens minted elsewhere; it does
/// not issue them. HS256 (HMAC-SHA256 with a shared secret) is the
/// only supported algorithm in this revision — keeps secret management
/// simple (one env var). RSA/ECDSA can be added later if a deployment
/// needs JWKS-driven key rotation.
///
/// Configured from:
/// - `RECURSIVE_HTTP_AUTH_JWT_SECRET` — HMAC secret bytes (UTF-8). Empty
///   or unset disables JWT auth.
/// - `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` — optional `aud` claim that
///   tokens must contain. Unset = audience claim ignored (still valid
///   JWT spec, just less strict).
///
/// `exp` claim is always required (RFC 7519 says optional; we make it
/// mandatory to prevent unbounded-validity tokens).
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: jsonwebtoken::DecodingKey,
    validation: jsonwebtoken::Validation,
}

impl JwtConfig {
    /// Build an HS256 verifier. Returns `None` if `secret` is empty
    /// (parallels `AuthConfig`'s "empty = disabled" pattern).
    ///
    /// `audience` is optional: `Some("my-app")` requires tokens carry
    /// `"aud": "my-app"`; `None` skips audience checking entirely.
    pub fn hs256(secret: &str, audience: Option<String>) -> Option<Self> {
        if secret.is_empty() {
            return None;
        }
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.set_required_spec_claims(&["exp"]);
        if let Some(aud) = audience {
            validation.set_audience(&[aud]);
        } else {
            validation.validate_aud = false;
        }
        Some(Self {
            decoding_key: jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
            validation,
        })
    }

    /// Verify a token. Returns true iff signature, exp, and (when
    /// configured) audience all check out.
    pub fn is_valid(&self, token: &str) -> bool {
        jsonwebtoken::decode::<serde_json::Value>(token, &self.decoding_key, &self.validation)
            .is_ok()
    }
}

/// Build `AuthConfig` from env vars:
///
/// - `RECURSIVE_HTTP_AUTH_KEYS` — comma-separated API keys (g135).
/// - `RECURSIVE_HTTP_AUTH_JWT_SECRET` — HMAC secret for JWT (g136).
/// - `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` — optional `aud` claim.
///
/// All unset = auth disabled (back-compat zero-config default).
pub(super) fn auth_config_from_env() -> AuthConfig {
    let raw = std::env::var("RECURSIVE_HTTP_AUTH_KEYS").unwrap_or_default();
    let keys: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut config = AuthConfig::new(keys);
    let jwt_secret = std::env::var("RECURSIVE_HTTP_AUTH_JWT_SECRET").unwrap_or_default();
    let jwt_audience = std::env::var("RECURSIVE_HTTP_AUTH_JWT_AUDIENCE")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(jwt) = JwtConfig::hs256(&jwt_secret, jwt_audience) {
        config = config.with_jwt(jwt);
    }
    if !config.is_enabled() {
        tracing::error!(
            "HTTP auth is NOT configured. Set \
             RECURSIVE_HTTP_AUTH_KEYS=... or RECURSIVE_HTTP_AUTH_JWT_SECRET=... \
             to enable. For local dev only, set \
             RECURSIVE_HTTP_AUTH_INSECURE_OK=1 to bypass (NEVER in production)."
        );
    }
    config
}

/// Axum middleware: enforce auth on requests.
///
/// Tries `X-API-Key` first (cheap); falls back to
/// `Authorization: Bearer <jwt>`. Either valid credential lets the
/// request through.
///
/// Layered only over the protected sub-router — public routes
/// (`/health`, `/metrics`, `/openapi.json`) are merged in at the
/// top level without going through this middleware. See
/// `build_router_with_auth_and_rate_limit` in `src/http/mod.rs`.
///
/// When auth is disabled (no API keys AND no JWT verifier
/// configured) and `RECURSIVE_HTTP_AUTH_INSECURE_OK` is not set to
/// `1` or `true`, the middleware returns 503 — default-deny for
/// production safety (Goal 277 / SEC-003).
pub(super) async fn auth_middleware(
    axum::extract::State(auth): axum::extract::State<AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !auth.is_enabled() {
        // SEC-007: `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` is a development-only
        // escape hatch. Release builds **ignore it** so a Docker image or
        // built binary cannot be tricked into running unauthenticated by an
        // operator "temporarily" setting the env var in production. Debug
        // builds (the default `cargo run` / `cargo test` flow) still honour
        // it so local dev stays frictionless.
        let insecure_ok_set = std::env::var("RECURSIVE_HTTP_AUTH_INSECURE_OK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if insecure_ok_set && cfg!(debug_assertions) {
            tracing::warn!(
                "RECURSIVE_HTTP_AUTH_INSECURE_OK=1 set — bypassing auth. \
                 Honoured because this is a debug build; release builds \
                 ignore this switch and return 503 instead. Never use \
                 this in production."
            );
            return next.run(req).await;
        }
        if insecure_ok_set && cfg!(not(debug_assertions)) {
            tracing::error!(
                "RECURSIVE_HTTP_AUTH_INSECURE_OK is set but ignored in \
                 release builds. Configure RECURSIVE_HTTP_AUTH_KEYS or \
                 RECURSIVE_HTTP_AUTH_JWT_SECRET, or unset the env var."
            );
        } else {
            tracing::error!(
                "HTTP server is running with NO auth configured. \
                 Set RECURSIVE_HTTP_AUTH_KEYS=<comma-separated-keys> \
                 or RECURSIVE_HTTP_AUTH_JWT_SECRET=<secret>. \
                 (Debug builds also honour RECURSIVE_HTTP_AUTH_INSECURE_OK=1 \
                 for local dev.)"
            );
        }
        let mut resp = axum::response::Response::new(axum::body::Body::from(
            "auth not configured; set RECURSIVE_HTTP_AUTH_KEYS or \
             RECURSIVE_HTTP_AUTH_JWT_SECRET (release builds ignore \
             RECURSIVE_HTTP_AUTH_INSECURE_OK)",
        ));
        *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return resp;
    }
    // Try X-API-Key first (cheaper than JWT verify).
    if !auth.keys.is_empty() {
        if let Some(presented) = req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) {
            if auth.is_valid(presented) {
                return next.run(req).await;
            }
        }
    }
    // Then try Authorization: Bearer <jwt>.
    if let Some(ref jwt) = auth.jwt {
        if let Some(authz) = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(token) = authz.strip_prefix("Bearer ") {
                if jwt.is_valid(token) {
                    return next.run(req).await;
                }
            }
        }
    }
    let mut resp = axum::response::Response::new(axum::body::Body::from("unauthorized"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    // ── AuthConfig::is_valid unit tests ──────────────────────────────────────

    #[test]
    fn auth_config_is_valid_accepts_correct_key() {
        // kills `replace AuthConfig::is_valid -> bool with false` and `diff == 0` mutations
        let cfg = AuthConfig::new(vec!["correct-key".to_string()]);
        assert!(cfg.is_valid("correct-key"), "correct key must be accepted");
    }

    #[test]
    fn auth_config_is_valid_rejects_wrong_key() {
        // kills `replace AuthConfig::is_valid -> bool with true` mutation
        let cfg = AuthConfig::new(vec!["correct-key".to_string()]);
        assert!(!cfg.is_valid("wrong-key"), "wrong key must be rejected");
    }

    #[test]
    fn auth_config_is_valid_rejects_prefix_of_correct_key() {
        // kills length-check removal mutations (constant-time comparison)
        let cfg = AuthConfig::new(vec!["long-key-value".to_string()]);
        assert!(
            !cfg.is_valid("long-key"),
            "prefix of correct key must be rejected"
        );
    }

    #[test]
    fn auth_config_is_valid_returns_false_for_empty_key_set() {
        // kills `if self.keys.is_empty() { return false; }` removal mutation
        let cfg = AuthConfig::new(vec![]);
        assert!(
            !cfg.is_valid("anything"),
            "empty key set must always reject"
        );
    }

    #[test]
    fn auth_config_is_valid_accepts_any_of_multiple_keys() {
        // kills the `found = true` assignment being replaced with a return
        let cfg = AuthConfig::new(vec!["key-a".to_string(), "key-b".to_string()]);
        assert!(cfg.is_valid("key-a"), "first key must be accepted");
        assert!(cfg.is_valid("key-b"), "second key must be accepted");
    }

    #[test]
    fn auth_config_is_enabled_true_with_api_keys() {
        // kills `replace AuthConfig::is_enabled -> bool with false`
        let cfg = AuthConfig::new(vec!["k".to_string()]);
        assert!(cfg.is_enabled());
    }

    #[test]
    fn auth_config_is_enabled_false_without_keys_or_jwt() {
        // kills `replace !self.keys.is_empty() with false` or `|| self.jwt.is_some()` mutations
        let cfg = AuthConfig::new(vec![]);
        assert!(!cfg.is_enabled(), "no keys and no JWT must be disabled");
    }

    fn router_with_auth(auth: AuthConfig) -> axum::Router {
        axum::Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(auth, auth_middleware))
    }

    /// Goal 277: When auth is not configured and INSECURE_OK is unset,
    /// the middleware returns 503. With INSECURE_OK=1, it passes through.
    /// Single combined test to avoid env-var races.
    #[tokio::test]
    async fn auth_inscure_ok_toggles() {
        // Ensure we start from a clean env for this test binary.
        unsafe {
            std::env::remove_var("RECURSIVE_HTTP_AUTH_INSECURE_OK");
        }

        let auth = AuthConfig::default(); // is_enabled() == false

        // --- Without INSECURE_OK: expect 503 ---
        {
            let app = router_with_auth(auth.clone());
            let resp = app
                .oneshot(
                    axum::extract::Request::builder()
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "expected 503 without INSECURE_OK"
            );
        }

        // --- INSECURE_OK=1: passes through (200) ---
        unsafe {
            std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");
        }
        {
            let app = router_with_auth(auth.clone());
            let resp = app
                .oneshot(
                    axum::extract::Request::builder()
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "expected 200 with INSECURE_OK=1"
            );
        }

        // --- INSECURE_OK=0 (falsy): expect 503 ---
        unsafe {
            std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "0");
        }
        {
            let app = router_with_auth(auth.clone());
            let resp = app
                .oneshot(
                    axum::extract::Request::builder()
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "expected 503 with INSECURE_OK=0"
            );
        }

        // --- INSECURE_OK=true (case-insensitive): passes through (200) ---
        unsafe {
            std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "true");
        }
        {
            let app = router_with_auth(auth.clone());
            let resp = app
                .oneshot(
                    axum::extract::Request::builder()
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "expected 200 with INSECURE_OK=true"
            );
        }

        // Clean up.
        unsafe {
            std::env::remove_var("RECURSIVE_HTTP_AUTH_INSECURE_OK");
        }
    }

    // SEC-007 regression test: the bypass must be gated on `cfg!(debug_assertions)`,
    // and the release branch must explicitly reject it. We use source-grep
    // because a runtime test cannot observe `cfg!` from inside the test
    // binary (which is always compiled with debug_assertions). This mirrors
    // the pattern used in `src/http/mod.rs::goal_272_route_level_auth_bypass`
    // for pinning the structural auth invariant.
    #[test]
    fn insecure_ok_bypass_is_gated_on_debug_assertions() {
        let src = include_str!("auth.rs");
        let middleware_body = src
            .split("pub(super) async fn auth_middleware")
            .nth(1)
            .expect("auth_middleware must exist");
        // The bypass branch must be conditional on cfg!(debug_assertions).
        // Without this gate the bypass would also fire in release builds,
        // defeating the whole point of SEC-007.
        assert!(
            middleware_body.contains("cfg!(debug_assertions)"),
            "auth_middleware must gate INSECURE_OK bypass on cfg!(debug_assertions)"
        );
        // And the release branch must explicitly check + log when the env
        // var was set but ignored, so an operator who sets it in prod gets
        // a loud error rather than a silent 503 with no clue why.
        assert!(
            middleware_body.contains("cfg!(not(debug_assertions))"),
            "auth_middleware must have an explicit release-build branch that \
             surfaces a misuse warning when INSECURE_OK is set"
        );
    }
}

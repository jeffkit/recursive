//! Authentication middleware and configuration for the HTTP server.
//!
//! Provides API-key and JWT bearer-token authentication. When no
//! credentials are configured (the default), the middleware returns
//! 503 Service Unavailable unless the explicit debug escape hatch
//! `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` is set — this must NEVER be
//! used in production.

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
        if !std::env::var("RECURSIVE_HTTP_AUTH_INSECURE_OK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::error!(
                "HTTP server is running with NO auth configured. \
                 Set RECURSIVE_HTTP_AUTH_KEYS=<comma-separated-keys> \
                 or RECURSIVE_HTTP_AUTH_JWT_SECRET=<secret>. \
                 To override for local dev only, set \
                 RECURSIVE_HTTP_AUTH_INSECURE_OK=1."
            );
            let mut resp = axum::response::Response::new(axum::body::Body::from(
                "auth not configured; set RECURSIVE_HTTP_AUTH_KEYS or \
                 RECURSIVE_HTTP_AUTH_INSECURE_OK=1 (local dev only)",
            ));
            *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return resp;
        }
        tracing::warn!(
            "RECURSIVE_HTTP_AUTH_INSECURE_OK=1 set — bypassing auth. \
             This must NEVER be used in production."
        );
        return next.run(req).await;
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
}

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
//!
//! This module is also the server's **identity** layer (issue #85): a
//! valid credential resolves to an [`AuthIdentity`], not just a boolean.
//! The middleware attaches it to the request extensions, and the session
//! handlers assert session ownership against it — a server that accepts
//! several credentials must not let each of them read every other
//! caller's sessions.

use axum::http::StatusCode;
use std::sync::Arc;

/// Env var holding the comma-separated inbound API keys.
pub const ENV_AUTH_KEYS: &str = "RECURSIVE_HTTP_AUTH_KEYS";
/// Env var holding the inbound JWT HMAC secret.
pub const ENV_AUTH_JWT_SECRET: &str = "RECURSIVE_HTTP_AUTH_JWT_SECRET";
/// Env var attributing API keys to subjects: a comma-separated list of
/// `subject=key` entries (split on the first `=`). `/sessions` visibility is
/// per subject, and the subject of a session is recorded as its owner.
pub const ENV_AUTH_KEY_OWNERS: &str = "RECURSIVE_HTTP_AUTH_KEY_OWNERS";
/// Env var listing the subjects carrying the `admin` role — comma-separated,
/// matched against a JWT `sub` or an API-key subject. Admin identities reach
/// every session, not just their own.
pub const ENV_AUTH_ADMINS: &str = "RECURSIVE_HTTP_AUTH_ADMINS";

/// Subject a bare `RECURSIVE_HTTP_AUTH_KEYS` entry authenticates as when
/// `RECURSIVE_HTTP_AUTH_KEY_OWNERS` does not map it to one. Every unmapped
/// key therefore resolves to this single principal — configure owners (or use
/// JWT) when callers must not see each other's sessions.
pub const DEFAULT_KEY_SUBJECT: &str = "api-key";

/// The authenticated principal behind a request (issue #85).
///
/// Resolved from a verified credential by [`AuthConfig::identify`] /
/// [`AuthConfig::identify_bearer`] and handed to the handlers through the
/// request extensions. Sessions record their creator's subject and tenant;
/// access to a session is granted to its owner and to admins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthIdentity {
    /// Principal id — a JWT `sub` claim, or an API key's configured subject.
    pub subject: String,
    /// Tenant the subject belongs to (JWT `tenant` claim). Scopes the
    /// subject: two tenants can mint the same `sub`, so the same subject in
    /// another tenant is a different owner.
    pub tenant: Option<String>,
    /// Whether the subject carries the `admin` role and may therefore reach
    /// every session.
    pub admin: bool,
}

impl AuthIdentity {
    /// The unrestricted identity used for requests on a server that has auth
    /// disabled (the debug escape hatch / single-user deployment) and for
    /// server-side work with no inbound request (triggers).
    pub fn local() -> Self {
        Self {
            subject: "local".to_string(),
            tenant: None,
            admin: true,
        }
    }

    /// Whether this identity may read or mutate a session owned by
    /// (`owner`, `tenant`).
    ///
    /// An admin reaches everything. Otherwise both the subject **and** the
    /// tenant must match. `owner == None` is a session written before the
    /// identity model existed: it belongs to nobody, so only an admin may
    /// touch it (default-deny for unattributed data).
    pub fn may_access_session(&self, owner: Option<&str>, tenant: Option<&str>) -> bool {
        if self.admin {
            return true;
        }
        match owner {
            Some(o) => self.subject == o && self.tenant.as_deref() == tenant,
            None => false,
        }
    }
}

/// One accepted API key and the subject it authenticates as.
#[derive(Clone, Debug)]
pub(super) struct ApiKey {
    pub(super) secret: String,
    pub(super) subject: String,
}

/// API key authentication for the HTTP server.
///
/// Configured from `RECURSIVE_HTTP_AUTH_KEYS`, a comma-separated list of
/// keys the server will accept in the `X-API-Key` request header. An empty
/// key set with no JWT verifier means auth is *not configured*: since
/// Goal 277 the middleware answers 503 rather than letting requests through
/// (see [`auth_middleware`]).
///
/// Distinct from `RECURSIVE_API_KEY` (singular): that variable holds the
/// **outbound** credential the agent uses to talk to its LLM provider.
/// `RECURSIVE_HTTP_AUTH_KEYS` (plural) holds the **inbound** credentials
/// the HTTP server accepts from clients. The names are deliberately
/// dissimilar to avoid confusion at the operator's shell.
///
/// `/health` and `/metrics` are always exempt (k8s liveness probes and
/// Prometheus scrapers must work unauthenticated).
///
/// Issue #85: a key is not just a yes/no — it authenticates as a subject
/// ([`AuthConfig::with_key_subject`], default [`DEFAULT_KEY_SUBJECT`]) which
/// owns the sessions it creates.
#[derive(Clone, Default)]
pub struct AuthConfig {
    pub(super) keys: Arc<Vec<ApiKey>>,
    /// Subjects carrying the `admin` role (see [`ENV_AUTH_ADMINS`]).
    pub(super) admins: Arc<Vec<String>>,
    pub(super) jwt: Option<JwtConfig>,
}

impl AuthConfig {
    /// Build an `AuthConfig` from an explicit key list. Pass an empty
    /// vec to disable API-key auth (a JWT verifier may still be
    /// attached via [`AuthConfig::with_jwt`]).
    ///
    /// Every key authenticates as [`DEFAULT_KEY_SUBJECT`]; use
    /// [`AuthConfig::with_key_subject`] to attribute a key to a caller.
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(
                keys.into_iter()
                    .map(|secret| ApiKey {
                        secret,
                        subject: DEFAULT_KEY_SUBJECT.to_string(),
                    })
                    .collect(),
            ),
            admins: Arc::new(Vec::new()),
            jwt: None,
        }
    }

    /// Attribute `secret` to `subject`, adding the key when it is not
    /// configured yet. Two callers sharing one key share one identity; two
    /// keys mapped to two subjects cannot see each other's sessions.
    pub fn with_key_subject(
        mut self,
        secret: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        let secret = secret.into();
        let subject = subject.into();
        let mut keys = (*self.keys).clone();
        match keys.iter_mut().find(|k| k.secret == secret) {
            Some(existing) => existing.subject = subject,
            None => keys.push(ApiKey { secret, subject }),
        }
        self.keys = Arc::new(keys);
        self
    }

    /// Give `subject` the `admin` role: it may read and mutate every session,
    /// not only the ones it owns. Applies to JWT `sub` claims and API-key
    /// subjects alike.
    pub fn with_admin(mut self, subject: impl Into<String>) -> Self {
        let subject = subject.into();
        let mut admins = (*self.admins).clone();
        if !admins.iter().any(|a| a == &subject) {
            admins.push(subject);
        }
        self.admins = Arc::new(admins);
        self
    }

    /// Whether `subject` carries the `admin` role.
    pub fn is_admin(&self, subject: &str) -> bool {
        self.admins.iter().any(|a| a == subject)
    }

    /// Resolve a presented API key to the identity it authenticates as, or
    /// `None` when it matches no configured key.
    ///
    /// The scan runs over **every** configured key regardless of an early
    /// match, so the comparison stays constant-time and does not leak *which*
    /// key matched — the property [`AuthConfig::is_valid`] had, kept now that
    /// a match yields a value rather than a boolean.
    pub fn identify(&self, presented: &str) -> Option<AuthIdentity> {
        let presented_bytes = presented.as_bytes();
        let mut matched: Option<&ApiKey> = None;
        for k in self.keys.iter() {
            let k_bytes = k.secret.as_bytes();
            if k_bytes.len() != presented_bytes.len() {
                continue;
            }
            let mut diff: u8 = 0;
            for (a, b) in k_bytes.iter().zip(presented_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                matched = Some(k);
            }
        }
        matched.map(|k| AuthIdentity {
            subject: k.subject.clone(),
            tenant: None,
            admin: self.is_admin(&k.subject),
        })
    }

    /// Resolve a bearer token to an identity, applying the configured admin
    /// role. `None` when no JWT verifier is attached or the token is not
    /// authentic (bad signature / expired / wrong audience) or not
    /// attributable (no `sub` claim).
    pub fn identify_bearer(&self, token: &str) -> Option<AuthIdentity> {
        let mut identity = self.jwt.as_ref()?.identity(token)?;
        identity.admin = self.is_admin(&identity.subject);
        Some(identity)
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
    pub fn is_valid(&self, presented: &str) -> bool {
        self.identify(presented).is_some()
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
    ///
    /// This is the *authenticity* question only. The middleware asks the
    /// attribution question instead ([`AuthConfig::identify_bearer`]), which
    /// additionally requires a `sub` claim — a token both valid and
    /// unattributable is rejected there.
    pub fn is_valid(&self, token: &str) -> bool {
        jsonwebtoken::decode::<serde_json::Value>(token, &self.decoding_key, &self.validation)
            .is_ok()
    }

    /// Verify a token and read the identity out of its claims.
    ///
    /// `None` when the token is not authentic (bad signature / expired /
    /// wrong audience) or carries no usable `sub`: a credential the server
    /// cannot tie to an owner is refused rather than collapsed onto a shared
    /// anonymous principal — that would hand every such token the same
    /// sessions.
    fn identity(&self, token: &str) -> Option<AuthIdentity> {
        let data =
            jsonwebtoken::decode::<JwtClaims>(token, &self.decoding_key, &self.validation).ok()?;
        let subject = data.claims.sub.filter(|s| !s.is_empty())?;
        Some(AuthIdentity {
            subject,
            tenant: data.claims.tenant.filter(|t| !t.is_empty()),
            // The admin role is a server-side decision
            // (`RECURSIVE_HTTP_AUTH_ADMINS`), never a token's self-declaration.
            admin: false,
        })
    }
}

/// The claims the server reads off a verified token (issue #85).
///
/// Everything else in the token is deliberately dropped. `sub` identifies the
/// owner; `tenant` (optional, non-standard) scopes it, because a `sub` is only
/// unique per issuer/tenant.
#[derive(serde::Deserialize)]
struct JwtClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    tenant: Option<String>,
}

/// Build `AuthConfig` from env vars:
///
/// - `RECURSIVE_HTTP_AUTH_KEYS` — comma-separated API keys (g135).
/// - `RECURSIVE_HTTP_AUTH_KEY_OWNERS` — `subject=key` pairs attributing keys
///   to callers (issue #85). An entry for a key that is not in
///   `RECURSIVE_HTTP_AUTH_KEYS` also adds it.
/// - `RECURSIVE_HTTP_AUTH_ADMINS` — subjects with the `admin` role.
/// - `RECURSIVE_HTTP_AUTH_JWT_SECRET` — HMAC secret for JWT (g136).
/// - `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` — optional `aud` claim.
///
/// All unset = auth disabled (back-compat zero-config default).
pub(super) fn auth_config_from_env() -> AuthConfig {
    let mut config = AuthConfig::new(comma_list(ENV_AUTH_KEYS));
    for entry in comma_list(ENV_AUTH_KEY_OWNERS) {
        match entry.split_once('=') {
            Some((subject, key)) if !subject.is_empty() && !key.is_empty() => {
                config = config.with_key_subject(key, subject);
            }
            _ => tracing::warn!(
                "ignoring malformed {ENV_AUTH_KEY_OWNERS} entry (expected \
                 `subject=key`)"
            ),
        }
    }
    for subject in comma_list(ENV_AUTH_ADMINS) {
        config = config.with_admin(subject);
    }
    let jwt_secret = std::env::var(ENV_AUTH_JWT_SECRET).unwrap_or_default();
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

/// Parse a comma-separated env var, trimming each entry and dropping blanks.
fn comma_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Axum middleware: enforce auth on requests and attach the caller's
/// [`AuthIdentity`] to the request extensions.
///
/// Tries `X-API-Key` first (cheap); falls back to
/// `Authorization: Bearer <jwt>`. Either valid credential lets the
/// request through — and resolves to an identity the session handlers
/// use for ownership assertions.
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
            // No credential to attribute: the request runs as the single
            // implicit local operator, which is also what it is.
            return run_with_identity(req, AuthIdentity::local(), next).await;
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
    // Try X-API-Key first (cheaper than JWT verify), then Authorization.
    let identity = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .and_then(|presented| auth.identify(presented))
        .or_else(|| {
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|authz| authz.strip_prefix("Bearer "))
                .and_then(|token| auth.identify_bearer(token))
        });
    match identity {
        Some(identity) => run_with_identity(req, identity, next).await,
        None => {
            let mut resp = axum::response::Response::new(axum::body::Body::from("unauthorized"));
            *resp.status_mut() = StatusCode::UNAUTHORIZED;
            resp
        }
    }
}

/// Hand `req` to the rest of the stack with `identity` attached, where the
/// session handlers read it via `Extension<AuthIdentity>`.
async fn run_with_identity(
    mut req: axum::extract::Request,
    identity: AuthIdentity,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(identity);
    next.run(req).await
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

    // ── Issue #85: identity resolution ──────────────────────────────────────

    #[test]
    fn identify_defaults_every_bare_key_to_the_shared_subject() {
        let cfg = AuthConfig::new(vec!["k1".to_string(), "k2".to_string()]);
        assert_eq!(
            cfg.identify("k1").map(|i| i.subject),
            Some(DEFAULT_KEY_SUBJECT.to_string())
        );
        assert_eq!(
            cfg.identify("k2").map(|i| i.subject),
            Some(DEFAULT_KEY_SUBJECT.to_string())
        );
    }

    #[test]
    fn identify_returns_none_for_an_unknown_key() {
        // kills `matched = Some(k)` being unconditional
        let cfg = AuthConfig::new(vec!["k1".to_string()]);
        assert!(cfg.identify("k2").is_none());
        assert!(cfg.identify("k").is_none(), "prefix must not match");
    }

    #[test]
    fn with_key_subject_attributes_existing_and_new_keys() {
        // k1 exists (subject overridden), k9 does not (key added).
        let cfg = AuthConfig::new(vec!["k1".to_string()])
            .with_key_subject("k1", "alice")
            .with_key_subject("k9", "carol");
        assert_eq!(cfg.identify("k1").map(|i| i.subject), Some("alice".into()));
        assert_eq!(cfg.identify("k9").map(|i| i.subject), Some("carol".into()));
    }

    #[test]
    fn admins_are_resolved_from_the_subject() {
        let cfg = AuthConfig::new(vec!["k1".to_string(), "k2".to_string()])
            .with_key_subject("k1", "root")
            .with_admin("root");
        assert!(cfg.identify("k1").expect("k1").admin, "root is an admin");
        assert!(!cfg.identify("k2").expect("k2").admin, "unlisted ≠ admin");
        assert!(cfg.is_admin("root"));
        assert!(!cfg.is_admin("alice"));
    }

    #[test]
    fn with_admin_ignores_duplicates() {
        let cfg = AuthConfig::new(vec![])
            .with_admin("root")
            .with_admin("root");
        assert_eq!(cfg.admins.len(), 1);
    }

    #[test]
    fn may_access_session_requires_the_same_subject() {
        let alice = AuthIdentity {
            subject: "alice".into(),
            tenant: None,
            admin: false,
        };
        assert!(alice.may_access_session(Some("alice"), None));
        assert!(!alice.may_access_session(Some("bob"), None));
        // A session written before the identity model belongs to nobody.
        assert!(!alice.may_access_session(None, None));
    }

    #[test]
    fn may_access_session_scopes_the_subject_by_tenant() {
        // Two tenants can mint the same `sub`, so the tenant is part of the
        // ownership key — otherwise acme's alice reaches globex's sessions.
        let alice = AuthIdentity {
            subject: "alice".into(),
            tenant: Some("acme".into()),
            admin: false,
        };
        assert!(alice.may_access_session(Some("alice"), Some("acme")));
        assert!(!alice.may_access_session(Some("alice"), Some("globex")));
        assert!(!alice.may_access_session(Some("alice"), None));
    }

    #[test]
    fn admin_reaches_any_session_including_unattributed_ones() {
        let root = AuthIdentity {
            subject: "root".into(),
            tenant: None,
            admin: true,
        };
        assert!(root.may_access_session(Some("bob"), Some("globex")));
        assert!(root.may_access_session(None, None));
    }

    #[test]
    fn local_identity_is_an_admin_of_its_own_subject() {
        // The no-auth (debug escape hatch) and server-side (trigger) identity.
        let local = AuthIdentity::local();
        assert_eq!(local.subject, "local");
        assert!(local.admin);
        assert!(local.may_access_session(Some("alice"), None));
    }

    fn mint_jwt(secret: &str, claims: serde_json::Value) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("mint jwt")
    }

    fn exp_in(secs: i64) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        now + secs
    }

    #[test]
    fn identify_bearer_reads_sub_and_tenant() {
        let cfg = AuthConfig::new(Vec::new())
            .with_jwt(JwtConfig::hs256("s3cret", None).expect("verifier"));
        let token = mint_jwt(
            "s3cret",
            serde_json::json!({"exp": exp_in(60), "sub": "alice", "tenant": "acme"}),
        );
        let id = cfg.identify_bearer(&token).expect("valid token");
        assert_eq!(id.subject, "alice");
        assert_eq!(id.tenant.as_deref(), Some("acme"));
        assert!(!id.admin);
    }

    #[test]
    fn identify_bearer_rejects_an_unattributable_token() {
        // Authentic but no `sub`: there is no owner to attribute, so it is not
        // an identity — the middleware answers 401 rather than collapsing it
        // onto a shared anonymous principal.
        let jwt = JwtConfig::hs256("s3cret", None).expect("verifier");
        let token = mint_jwt("s3cret", serde_json::json!({"exp": exp_in(60)}));
        assert!(jwt.is_valid(&token), "signature/exp still check out");
        let cfg = AuthConfig::new(Vec::new()).with_jwt(jwt);
        assert!(cfg.identify_bearer(&token).is_none());
    }

    #[test]
    fn identify_bearer_ignores_bad_tokens() {
        let cfg = AuthConfig::new(Vec::new())
            .with_jwt(JwtConfig::hs256("s3cret", None).expect("verifier"));
        let wrong_secret = mint_jwt("other", serde_json::json!({"exp": exp_in(60), "sub": "a"}));
        let expired = mint_jwt(
            "s3cret",
            serde_json::json!({"exp": exp_in(-300), "sub": "a"}),
        );
        assert!(cfg.identify_bearer(&wrong_secret).is_none());
        assert!(cfg.identify_bearer(&expired).is_none());
    }

    #[test]
    fn identify_bearer_is_none_without_a_jwt_verifier() {
        let cfg = AuthConfig::new(vec!["k".to_string()]);
        let token = mint_jwt("s3cret", serde_json::json!({"exp": exp_in(60), "sub": "a"}));
        assert!(cfg.identify_bearer(&token).is_none());
    }

    #[test]
    fn jwt_admin_role_comes_from_config_not_from_the_token() {
        // A token that claims `role: admin` for a subject the server did not
        // mark as admin must NOT be an admin — the role is the server's call.
        let cfg = AuthConfig::new(Vec::new())
            .with_jwt(JwtConfig::hs256("s3cret", None).expect("verifier"))
            .with_admin("alice");
        let bob = mint_jwt(
            "s3cret",
            serde_json::json!({"exp": exp_in(60), "sub": "bob", "role": "admin", "roles": ["admin"]}),
        );
        assert!(!cfg.identify_bearer(&bob).expect("bob").admin);
        let alice = mint_jwt(
            "s3cret",
            serde_json::json!({"exp": exp_in(60), "sub": "alice"}),
        );
        assert!(cfg.identify_bearer(&alice).expect("alice").admin);
    }

    /// Issue #85: the env-driven path — keys, key→subject mapping and the admin
    /// list. ONE test: `set_var` is process-global and `cargo test` runs tests
    /// in parallel threads (`.dev/AGENTS.md`).
    #[test]
    fn auth_config_from_env_reads_keys_owners_and_admins() {
        let _guard = crate::test_util::env_lock();
        let vars = [ENV_AUTH_KEYS, ENV_AUTH_KEY_OWNERS, ENV_AUTH_ADMINS];
        let saved: Vec<(&str, Option<String>)> =
            vars.iter().map(|v| (*v, std::env::var(v).ok())).collect();
        unsafe {
            // Trailing commas and blanks must be ignored (the env vars are
            // hand-written, and an empty entry must never become a key).
            std::env::set_var(ENV_AUTH_KEYS, "k1, k2,");
            // One well-formed mapping for an existing key, one that also adds
            // a key, one malformed entry that must be ignored.
            std::env::set_var(ENV_AUTH_KEY_OWNERS, "alice=k1, bob=k3,broken");
            std::env::set_var(ENV_AUTH_ADMINS, "bob");
        }

        let cfg = auth_config_from_env();

        assert_eq!(cfg.identify("k1").expect("k1").subject, "alice");
        assert_eq!(
            cfg.identify("k2").expect("k2").subject,
            DEFAULT_KEY_SUBJECT,
            "an unmapped key keeps the shared default subject"
        );
        assert_eq!(cfg.identify("k3").expect("k3").subject, "bob");
        assert!(cfg.identify("broken").is_none(), "malformed entry ignored");
        assert!(cfg.identify("k3").expect("k3").admin, "bob is an admin");
        assert!(!cfg.identify("k1").expect("k1").admin);

        for (name, value) in saved {
            match value {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
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

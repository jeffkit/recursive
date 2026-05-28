# Goal 136 — Authentication: JWT bearer token verification

**Roadmap**: Phase 17.2 — Authentication (part 2/2: JWT verification;
API keys shipped in g135)

**Design principle check**:
- Implemented as: extend `AuthConfig` in `src/http.rs` with an
  optional `JwtConfig` field. Reuse the existing `auth_middleware`;
  add a Bearer-token branch alongside the existing `X-API-Key` branch.
- Verify-only: this goal does **not** mint JWTs. Token issuance is a
  separate concern (user management, login flows) and out of scope.
- ❌ Does NOT modify `agent.rs`, `runtime.rs`, or any non-http code.
- ✅ Adds one Rust dependency: `jsonwebtoken = "9"` (RustCrypto-adjacent,
  widely used, MIT-licensed).

## Why

g135 shipped API key auth. ROADMAP-v4 17.2 also called for JWT.
Without it the server can only trust callers via static shared
secrets (X-API-Key); JWT lets a separate identity provider issue
short-lived tokens with audience/expiry claims, removing the need
for static secrets in client code.

## Scope (do exactly this, no more)

### 1. Cargo.toml

Add under `[dependencies]`:

```toml
jsonwebtoken = "9"
```

Default features are fine — `jsonwebtoken` includes RustCrypto
HMAC support which is what we need for HS256.

### 2. `JwtConfig` struct

In `src/http.rs`, alongside `AuthConfig` (which is around line 104):

```rust
/// JWT bearer token verification config.
///
/// Verify-only: this server validates tokens minted elsewhere; it does
/// not issue them. HS256 (HMAC-SHA256 with a shared secret) is the
/// only supported algorithm in this revision — keeps secret management
/// simple (one env var). RSA/ECDSA can be added later if a deployment
/// needs JWKS-driven key rotation.
#[derive(Clone)]
pub struct JwtConfig {
    /// Decoding key (HMAC secret bytes).
    decoding_key: jsonwebtoken::DecodingKey,
    /// Validation rules: alg=HS256, exp required, optional audience.
    validation: jsonwebtoken::Validation,
}

impl JwtConfig {
    /// Build a config from an HMAC secret. Empty secret = disabled
    /// (parallel to AuthConfig).
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

    /// Verify a token; returns true iff signature + exp + audience all OK.
    pub fn is_valid(&self, token: &str) -> bool {
        jsonwebtoken::decode::<serde_json::Value>(
            token,
            &self.decoding_key,
            &self.validation,
        )
        .is_ok()
    }
}
```

The `serde_json::Value` claim type is a deliberate choice: we don't
need to deserialize claims into a struct for the verify-only flow.
`jsonwebtoken` enforces signature + exp + audience purely from
metadata; the claim payload is opaque to us.

### 3. Extend `AuthConfig` to carry an optional `JwtConfig`

```rust
#[derive(Clone, Default)]
pub struct AuthConfig {
    keys: Arc<Vec<String>>,
    jwt: Option<JwtConfig>,
}

impl AuthConfig {
    // Existing constructors unchanged.

    /// Attach a JWT verifier. Call after `new(...)` to get
    /// API-key-OR-JWT semantics. Without this call, behavior is
    /// X-API-Key only (current g135 behavior).
    pub fn with_jwt(mut self, jwt: JwtConfig) -> Self {
        self.jwt = Some(jwt);
        self
    }

    /// Auth is enabled iff there's at least one accepted credential
    /// shape (key set non-empty, OR JWT verifier present).
    pub fn is_enabled(&self) -> bool {
        !self.keys.is_empty() || self.jwt.is_some()
    }
}
```

### 4. Update `auth_middleware` to also accept `Authorization: Bearer …`

```rust
async fn auth_middleware(
    State(auth): State<AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !auth.is_enabled() {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path == "/health" || path == "/metrics" {
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
        if let Some(authz) = req.headers().get("authorization").and_then(|v| v.to_str().ok()) {
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
```

### 5. Env-driven config

Add to `auth_config_from_env`:

```rust
fn auth_config_from_env() -> AuthConfig {
    // ... existing key parsing ...
    let mut config = AuthConfig::new(keys);
    let jwt_secret = std::env::var("RECURSIVE_HTTP_AUTH_JWT_SECRET").unwrap_or_default();
    let jwt_audience = std::env::var("RECURSIVE_HTTP_AUTH_JWT_AUDIENCE").ok();
    if let Some(jwt) = JwtConfig::hs256(&jwt_secret, jwt_audience) {
        config = config.with_jwt(jwt);
    }
    config
}
```

`RECURSIVE_HTTP_AUTH_JWT_SECRET=<hmac-secret>` enables JWT;
`RECURSIVE_HTTP_AUTH_JWT_AUDIENCE=<aud>` optionally pins audience.
Both empty = JWT disabled (back-compat).

### 6. Tests in `tests/http.rs`

All inside `mod http_tests`, mirroring g135 auth tests. Use a helper
that mints a valid HS256 token at test time (we depend on
`jsonwebtoken` already, so encoding is available too).

- **Test A — `jwt_disabled_legacy_keys_only`**: AuthConfig with keys
  but no JWT. Bearer token in header is ignored; only X-API-Key works.
- **Test B — `jwt_valid_token_accepted`**: AuthConfig with JWT only
  (no API keys). Bearer of a freshly-minted, non-expired token →
  200. Confirms the bearer path works.
- **Test C — `jwt_expired_token_rejected`**: Mint a token with
  `exp = now() - 60`. Server returns 401.
- **Test D — `jwt_wrong_signature_rejected`**: Mint with secret A,
  verify with secret B → 401.
- **Test E — `jwt_audience_mismatch_rejected`**: Server configured
  with `audience = "expected"`, token has `aud = "other"` → 401.
- **Test F — `jwt_audience_match_accepted`**: Same setup with
  matching aud → 200.
- **Test G — `jwt_or_api_key_either_works`**: Both modes configured.
  Request with valid X-API-Key → 200. Request with valid JWT → 200.
  Request with neither → 401.
- **Test H — `jwt_health_metrics_remain_exempt`**: With JWT
  enabled, /health and /metrics still 200 without any auth header.
  (Smoke test that g135's exemption still holds.)

### 7. Documentation

Extend `AuthConfig`'s rustdoc and `JwtConfig`'s rustdoc to document
the env vars: `RECURSIVE_HTTP_AUTH_JWT_SECRET`,
`RECURSIVE_HTTP_AUTH_JWT_AUDIENCE`.

## Acceptance

- `cargo build --features http` green.
- `cargo test --features http --test http` green; new tests pass.
- `cargo test --all-features` green (no regressions in 592).
- `cargo fmt --all -- --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Backward compatibility: server with no
  `RECURSIVE_HTTP_AUTH_JWT_SECRET` env behaves identically to
  post-g135 (X-API-Key only or no auth).
- One new dep (`jsonwebtoken = "9"`) — justified above.
- Files modified: `Cargo.toml` (+1 dep), `src/http.rs` (~80 lines
  added), `tests/http.rs` (~140 lines added). No other source files.

## Notes

- Use `jsonwebtoken::EncodingKey::from_secret` + `encode()` in tests
  to mint tokens deterministically; the same crate is on both sides.
- `Validation::set_required_spec_claims(&["exp"])` is the safety
  default — a valid token without `exp` is rejected. RFC 7519 says
  `exp` is OPTIONAL in spec but for our use it's mandatory.
- Time math in tests: `chrono` is already a transitive dep via
  `jsonwebtoken`, but for `exp` we just need `SystemTime::now() +
  duration` cast to u64 epoch seconds. Don't pull `chrono` directly.
- `.unwrap()` in the tests is fine; in production code, the
  `is_valid -> bool` API hides errors deliberately (a malformed
  token is auth failure, not server error).
- `jsonwebtoken` v9 vs v10: v10 changed some APIs (`Validation::new`
  return type). Pinning to `9` keeps the spec stable; `10` migration
  is a separate dep-bump goal.

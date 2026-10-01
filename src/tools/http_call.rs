//! `http_call`: registry-bound business-API tool (issue #63).
//!
//! The C-end agent form ("load a skill, call the business API, no fs / no
//! shell") needs a way to reach business endpoints without `Bash`+curl.
//! `WebFetch` cannot fill that role (GET only; `url_guard` blocks
//! loopback/RFC1918 by design), and a generic URL-fetch tool would trade a
//! command-injection problem for an SSRF problem (`169.254.169.254`,
//! `localhost:6379`).
//!
//! So this tool inverts the abstraction: **the model never supplies a URL.**
//! The operator registers named endpoints (method / base URL / path template
//! / auth reference) in `<workspace>/.recursive/endpoints.json` (override:
//! `RECURSIVE_ENDPOINTS_FILE`); the model only picks an endpoint *name* and
//! passes params/body. URLs are assembled server-side, which makes SSRF
//! structurally absent from the tool call, and auth values never appear in
//! config or in anything the model can see.
//!
//! Security posture:
//! - unregistered endpoint names are rejected, with an error that lists
//!   endpoint names only (never the internal URLs);
//! - base URLs are validated at load time with `url_guard` semantics:
//!   link-local (IMDS) / metadata / unspecified targets are rejected even
//!   when `allow_private` is set; loopback / RFC1918 / ULA targets require
//!   the per-endpoint `allow_private: true` opt-in (C-end business gateways
//!   are routinely on the internal network — this is the explicit,
//!   auditable way to reach them);
//! - redirects are hard-disabled on the client (`Policy::none()`), closing
//!   the redirect hop that `url_guard`'s docs list as a known gap — a 3xx is
//!   surfaced as an error instead of a second request to an unvalidated
//!   host;
//! - request/transport errors are reported without the request URL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method as HttpMethod};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Host;

use super::Tool;
use crate::tools::tool_kind::ToolKind;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::url_guard::is_private_ip;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 65_536;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const RETRY_DELAY_MS: u64 = 150;
const DEFAULT_AUTH_HEADER: &str = "Authorization";
const DEFAULT_AUTH_PREFIX: &str = "Bearer ";

/// HTTP method allowed on a registered endpoint.
///
/// GET/HEAD are reads; POST/PUT/PATCH/DELETE carry (or clear) business
/// state. The method drives the default `idempotent` value, the tool's
/// side-effect classification, and whether a `body` argument is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            _ => return None,
        })
    }

    fn http(self) -> HttpMethod {
        match self {
            Self::Get => HttpMethod::GET,
            Self::Head => HttpMethod::HEAD,
            Self::Post => HttpMethod::POST,
            Self::Put => HttpMethod::PUT,
            Self::Patch => HttpMethod::PATCH,
            Self::Delete => HttpMethod::DELETE,
        }
    }

    /// Methods that conventionally change server state.
    fn is_mutating(self) -> bool {
        matches!(self, Self::Post | Self::Put | Self::Patch | Self::Delete)
    }

    /// Whether a failed transport attempt may be retried once.
    fn default_idempotent(self) -> bool {
        matches!(self, Self::Get | Self::Head | Self::Put | Self::Delete)
    }

    fn accepts_body(self) -> bool {
        matches!(self, Self::Post | Self::Put | Self::Patch)
    }
}

/// One operator-registered business endpoint. The model never sees
/// `base_url` (or the auth value); it sees only the endpoint *name* in the
/// tool schema's enum.
#[derive(Debug, Clone)]
pub struct EndpointSpec {
    pub name: String,
    pub method: Method,
    pub base_url: url::Url,
    pub path_template: String,
    pub description: String,
    /// Env var holding the auth token. Read at call time; the value is
    /// never logged, returned, or embedded in errors.
    pub auth_env: Option<String>,
    pub auth_header: String,
    pub auth_prefix: String,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    /// Governs the single transport-failure retry.
    pub idempotent: bool,
    /// Opt-in required for loopback / RFC1918 / ULA base URLs (never
    /// sufficient for link-local / metadata targets).
    pub allow_private: bool,
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    method: String,
    base_url: String,
    path: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    auth_env: Option<String>,
    #[serde(default)]
    auth_header: Option<String>,
    #[serde(default)]
    auth_prefix: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_response_bytes: Option<usize>,
    #[serde(default)]
    idempotent: Option<bool>,
    #[serde(default)]
    allow_private: bool,
}

#[derive(Debug, Deserialize)]
struct RawRegistry {
    endpoints: BTreeMap<String, RawEndpoint>,
}

/// Operator-side registry of named endpoints. Keys are the only endpoint
/// identifiers exposed to the model.
#[derive(Debug, Clone, Default)]
pub struct EndpointRegistry {
    endpoints: BTreeMap<String, EndpointSpec>,
}

/// How a base-URL host classifies for the load-time guard.
enum HostClass {
    /// Never reachable, even with `allow_private` (IMDS / metadata /
    /// unspecified / broadcast).
    Blocked,
    /// Reachable only with `allow_private: true` (loopback, RFC1918, ULA,
    /// `localhost` names).
    Private,
    Public,
}

impl EndpointRegistry {
    /// Parse the registry from JSON text, validating every endpoint.
    /// Load-time errors are operator-facing and MAY quote base URLs — they
    /// never reach the model (the model only sees the built tool).
    pub fn from_json(text: &str) -> Result<Self> {
        let raw: RawRegistry = serde_json::from_str(text).map_err(|e| Error::Config {
            message: format!("invalid endpoints config: {e}"),
        })?;
        let mut endpoints = BTreeMap::new();
        for (name, raw) in raw.endpoints {
            let spec = Self::build_endpoint(&name, raw)?;
            endpoints.insert(name, spec);
        }
        Ok(Self { endpoints })
    }

    /// Discover the endpoints config for `workspace`:
    /// `RECURSIVE_ENDPOINTS_FILE` (explicit path override) or
    /// `<workspace>/.recursive/endpoints.json`.
    ///
    /// Returns `None` (silently) when no file exists, and `None` with a
    /// stderr warning when a file exists but is invalid — a malformed
    /// endpoints file must not brick unrelated commands, matching how the
    /// permissions config degrades.
    pub fn discover(workspace: &Path) -> Option<Self> {
        let path: PathBuf = match std::env::var("RECURSIVE_ENDPOINTS_FILE") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => workspace.join(".recursive").join("endpoints.json"),
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return None,
        };
        match Self::from_json(&content) {
            Ok(r) if r.is_empty() => None,
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("http_call: ignoring {}: {e}", path.display());
                None
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn get(&self, name: &str) -> Option<&EndpointSpec> {
        self.endpoints.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.endpoints.keys()
    }

    fn build_endpoint(name: &str, raw: RawEndpoint) -> Result<EndpointSpec> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::Config {
                message: format!("endpoint name '{name}' is invalid: use [a-zA-Z0-9_-] only"),
            });
        }
        let method = Method::parse(&raw.method).ok_or_else(|| Error::Config {
            message: format!(
                "endpoint '{name}': unknown method '{}' (use GET/HEAD/POST/PUT/PATCH/DELETE)",
                raw.method
            ),
        })?;
        let base_url = url::Url::parse(&raw.base_url).map_err(|e| Error::Config {
            message: format!(
                "endpoint '{name}': invalid base_url '{}': {e}",
                raw.base_url
            ),
        })?;
        validate_base_url(&base_url, name, raw.allow_private)?;
        if !raw.path.starts_with('/') {
            return Err(Error::Config {
                message: format!("endpoint '{name}': path '{}' must start with '/'", raw.path),
            });
        }
        // Fail at load time on malformed templates — an unterminated '{'
        // would otherwise silently pass through as a literal path segment.
        validate_path_template(&raw.path, name)?;
        if let Some(bytes) = raw.max_response_bytes {
            if bytes == 0 {
                return Err(Error::Config {
                    message: format!("endpoint '{name}': max_response_bytes must be > 0"),
                });
            }
        }
        Ok(EndpointSpec {
            name: name.to_string(),
            method,
            base_url,
            path_template: raw.path,
            description: raw.description,
            auth_env: raw.auth_env.filter(|s| !s.is_empty()),
            auth_header: raw
                .auth_header
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_AUTH_HEADER.to_string()),
            auth_prefix: raw
                .auth_prefix
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_AUTH_PREFIX.to_string()),
            timeout: Duration::from_millis(raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            max_response_bytes: raw.max_response_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
            idempotent: raw
                .idempotent
                .unwrap_or_else(|| method.default_idempotent()),
            allow_private: raw.allow_private,
        })
    }
}

/// Load-time guard on an operator-configured base URL.
///
/// Same address classes as [`super::url_guard`], but split by severity:
/// link-local / metadata / unspecified are **always** rejected (no business
/// API lives there — this is IMDS-credential-exfiltration defense in
/// depth), while loopback / RFC1918 / ULA need the explicit
/// `allow_private: true` opt-in (C-end business gateways are routinely
/// internal). Public hosts pass.
fn validate_base_url(base_url: &url::Url, name: &str, allow_private: bool) -> Result<()> {
    let reject = |why: String| Error::Config {
        message: format!("endpoint '{name}': base_url '{}': {why}", base_url),
    };
    if base_url.scheme() != "http" && base_url.scheme() != "https" {
        return Err(reject("scheme must be http or https".into()));
    }
    if !base_url.username().is_empty() || base_url.password().is_some() {
        return Err(reject("userinfo in base_url is not allowed".into()));
    }
    if base_url.fragment().is_some() {
        return Err(reject("fragment in base_url is not allowed".into()));
    }
    let host = base_url
        .host()
        .ok_or_else(|| reject("URL has no host".into()))?;
    let class = classify_host(host);
    match class {
        HostClass::Blocked => Err(reject(
            "link-local / metadata / unspecified targets are never allowed in the \
             endpoint registry"
                .into(),
        )),
        HostClass::Private if !allow_private => Err(reject(
            "private/loopback target requires \"allow_private\": true on this endpoint \
             (the opt-in documents that the operator intends an internal gateway)"
                .into(),
        )),
        _ => Ok(()),
    }
}

fn classify_host(host: Host<&str>) -> HostClass {
    match host {
        Host::Ipv4(v4) => classify_ip(std::net::IpAddr::V4(v4)),
        Host::Ipv6(v6) => {
            // IPv4-mapped IPv6 routes to the mapped v4 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                classify_ip(std::net::IpAddr::V4(v4))
            } else {
                classify_ip(std::net::IpAddr::V6(v6))
            }
        }
        Host::Domain(d) => {
            let host = d.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(host.as_str());
            if host == "metadata.google.internal" || host.ends_with(".metadata.google.internal") {
                return HostClass::Blocked;
            }
            if host == "localhost" || host.ends_with(".localhost") {
                return HostClass::Private;
            }
            // Bare-IP spellings in domain form ("2130706433", "127.1") —
            // same lenient parse the OS resolver would apply.
            if let Some(ip) = host
                .parse::<std::net::IpAddr>()
                .ok()
                .or_else(|| super::url_guard::parse_lenient_ipv4(host))
            {
                return classify_ip(ip);
            }
            HostClass::Public
        }
    }
}

fn classify_ip(ip: std::net::IpAddr) -> HostClass {
    match ip {
        std::net::IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_broadcast() || v4.is_link_local() {
                HostClass::Blocked
            } else if is_private_ip(ip) {
                HostClass::Private
            } else {
                HostClass::Public
            }
        }
        std::net::IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            let is_link_local = (seg0 & 0xffc0) == 0xfe80;
            if v6.is_unspecified() || is_link_local {
                HostClass::Blocked
            } else if is_private_ip(ip) {
                HostClass::Private
            } else {
                HostClass::Public
            }
        }
    }
}

/// Extract `{name}` placeholders from a path template, rejecting
/// unterminated or empty placeholders at load time.
fn placeholders(template: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').ok_or_else(|| Error::Config {
            message: format!("path template '{template}' has an unterminated '{{' placeholder"),
        })? + start;
        let name = &rest[start + 1..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::Config {
                message: format!(
                    "path template '{template}' has an invalid placeholder '{{{name}}}' \
                     (use [a-zA-Z0-9_])"
                ),
            });
        }
        out.push(name.to_string());
        rest = &rest[end + 1..];
    }
    Ok(out)
}

fn validate_path_template(template: &str, name: &str) -> Result<()> {
    placeholders(template)
        .map(|_| ())
        .map_err(|e| Error::Config {
            message: format!("endpoint '{name}': {}", e),
        })
}

/// Percent-encode one path-segment value. Unreserved characters pass
/// through; everything else (including `/`, `?`, `#`, `%`, spaces, UTF-8)
/// is %-encoded byte-wise so a param value can never escape its segment.
fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Render a scalar param value the way it appears in a URL.
fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

type AuthResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The `HttpCall` tool: call a *registered* business endpoint by name.
#[derive(Clone)]
pub struct HttpCall {
    registry: EndpointRegistry,
    client: Client,
    /// Test seam: resolve an auth env var without touching process env
    /// (defaults to `std::env::var`).
    #[allow(clippy::type_complexity)]
    auth_resolver: Option<AuthResolver>,
}

impl std::fmt::Debug for HttpCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpCall")
            .field("registry", &self.registry)
            .field("auth_resolver", &self.auth_resolver.is_some())
            .finish()
    }
}

impl HttpCall {
    pub fn new(registry: EndpointRegistry) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(format!("recursive-agent/{}", env!("CARGO_PKG_VERSION")))
            // Redirects are hard-disabled: every hop would leave the
            // load-time-validated base URL, and `url_guard`'s documented
            // known gap is exactly "the lack of a redirect policy". A 3xx
            // is surfaced as an error instead of a second request.
            .redirect(reqwest::redirect::Policy::none())
            // Construction carve-out (Invariant #5 §construction): TLS
            // backend failure means the process cannot do HTTP at all.
            .build();
        #[allow(
            clippy::expect_used,
            reason = "TLS backend unavailable is a fatal startup error"
        )]
        let client = client.expect("reqwest client build: TLS backend unavailable");
        Self {
            registry,
            client,
            auth_resolver: None,
        }
    }

    /// Override the auth env-var resolver (tests inject a closure so they
    /// never mutate process env).
    pub fn with_auth_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.auth_resolver = Some(Arc::new(resolver));
        self
    }

    fn resolve_auth(&self, env_var: &str) -> Option<String> {
        match &self.auth_resolver {
            Some(f) => f(env_var),
            None => std::env::var(env_var).ok().filter(|v| !v.is_empty()),
        }
    }

    /// Assemble the request URL from the endpoint spec and the model's
    /// params: `{placeholder}` segments are substituted (percent-encoded),
    /// leftover scalar params become query pairs.
    fn build_url(spec: &EndpointSpec, params: &BTreeMap<String, Value>) -> Result<url::Url> {
        let needed = placeholders(&spec.path_template).map_err(|e| Error::BadToolArgs {
            name: Self::NAME.to_string(),
            message: e.to_string(),
        })?;
        let mut missing = Vec::new();
        let mut query = Vec::new();
        let mut segments: BTreeMap<String, String> = BTreeMap::new();
        for name in &needed {
            match params.get(name).and_then(scalar_to_string) {
                Some(v) => {
                    segments.insert(name.clone(), encode_segment(&v));
                }
                None => {
                    if params.contains_key(name) {
                        missing.push(format!(
                            "'{name}': placeholder values must be a string / number / boolean"
                        ));
                    } else {
                        missing.push(format!("'{name}'"));
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(Error::BadToolArgs {
                name: Self::NAME.to_string(),
                message: format!(
                    "endpoint '{}' path requires params {}",
                    spec.name,
                    missing.join(", ")
                ),
            });
        }
        for (k, v) in params {
            if !needed.contains(k) {
                match scalar_to_string(v) {
                    Some(s) => query.push((k, s)),
                    None => {
                        return Err(Error::BadToolArgs {
                            name: Self::NAME.to_string(),
                            message: format!(
                                "query param '{k}' must be a string / number / boolean \
                                 (objects belong in `body`)"
                            ),
                        })
                    }
                }
            }
        }
        // Substitute into the template, then set the full path (base_url
        // may itself carry a path prefix).
        let mut path = spec.path_template.clone();
        for (name, value) in &segments {
            path = path.replace(&format!("{{{name}}}"), value);
        }
        let mut url = spec.base_url.clone();
        let base_path = url.path().trim_end_matches('/');
        url.set_path(&format!("{base_path}{path}"));
        for (k, v) in &query {
            url.query_pairs_mut().append_pair(k, v);
        }
        Ok(url)
    }

    async fn send_with_retry(
        &self,
        spec: &EndpointSpec,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            match builder
                .try_clone()
                .ok_or_else(|| Error::Tool {
                    name: Self::NAME.to_string(),
                    call_id: None,
                    message: format!("endpoint '{}': failed to build request", spec.name),
                })?
                .timeout(spec.timeout)
                .send()
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    // Retry once for idempotent endpoints on transport-level
                    // failures (connect refused / timeout) — never for
                    // POST-like endpoints, where a retried request could
                    // double-apply a business action.
                    attempt += 1;
                    if spec.idempotent && attempt == 1 {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    }
                    let kind = if e.is_timeout() {
                        "timed out"
                    } else if e.is_connect() {
                        "connection failed"
                    } else {
                        "request failed"
                    };
                    // Strip the request URL (it embeds the internal base
                    // URL) before rendering the error.
                    let e = e.without_url();
                    return Err(Error::Tool {
                        name: Self::NAME.to_string(),
                        call_id: None,
                        message: format!("endpoint '{}': {kind} ({e})", spec.name),
                    });
                }
            }
        }
    }

    /// Read the response body capped at `max_bytes`; returns
    /// `(body, truncated)`.
    async fn read_capped(resp: &mut reqwest::Response, max_bytes: usize) -> (String, bool) {
        let mut buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = resp.chunk().await.unwrap_or(None) {
            if buf.len() + chunk.len() > max_bytes {
                let take = max_bytes - buf.len();
                buf.extend_from_slice(&chunk[..take]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
            if buf.len() >= max_bytes {
                break;
            }
        }
        (String::from_utf8_lossy(&buf).into_owned(), truncated)
    }

    async fn call(&self, spec: &EndpointSpec, args: &Value) -> Result<String> {
        let empty = BTreeMap::new();
        let params: BTreeMap<String, Value> = match args.get("params") {
            Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Some(_) => {
                return Err(Error::BadToolArgs {
                    name: Self::NAME.to_string(),
                    message: "`params` must be an object".into(),
                })
            }
            None => empty,
        };

        let url = Self::build_url(spec, &params)?;

        let mut builder = self.client.request(spec.method.http(), url);
        if let Some(body) = args.get("body") {
            if !spec.method.accepts_body() {
                return Err(Error::BadToolArgs {
                    name: Self::NAME.to_string(),
                    message: format!(
                        "endpoint '{}' is {} — a JSON body is only accepted for \
                         POST/PUT/PATCH",
                        spec.name,
                        format!("{:?}", spec.method).to_uppercase()
                    ),
                });
            }
            let text = serde_json::to_string(body).map_err(|e| Error::BadToolArgs {
                name: Self::NAME.to_string(),
                message: format!("`body` is not valid JSON: {e}"),
            })?;
            builder = builder
                .header("content-type", "application/json")
                .body(text);
        }
        if let Some(env_var) = &spec.auth_env {
            match self.resolve_auth(env_var) {
                Some(token) => {
                    builder =
                        builder.header(&spec.auth_header, format!("{}{}", spec.auth_prefix, token));
                }
                None => {
                    // Name the env var, never the value (there is no value).
                    return Err(Error::Tool {
                        name: Self::NAME.to_string(),
                        call_id: None,
                        message: format!(
                            "endpoint '{}': auth env var '{env_var}' is not set",
                            spec.name
                        ),
                    });
                }
            }
        }

        let mut resp = self.send_with_retry(spec, builder).await?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(Error::Tool {
                name: Self::NAME.to_string(),
                call_id: None,
                message: format!(
                    "endpoint '{}' returned HTTP {} — redirects are disabled; \
                     fix the endpoint's base_url/path in the endpoints config",
                    spec.name,
                    status.as_u16()
                ),
            });
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let (body, truncated) = Self::read_capped(&mut resp, spec.max_response_bytes).await;
        let mut out = format!("HTTP {}\n", status.as_u16());
        if !content_type.is_empty() {
            out.push_str(&format!("content-type: {content_type}\n"));
        }
        out.push('\n');
        out.push_str(&body);
        if truncated {
            out.push_str(&format!(
                "\n\n[…truncated at {} bytes]",
                spec.max_response_bytes
            ));
        }
        Ok(out)
    }
}

impl HttpCall {
    pub const NAME: &'static str = "HttpCall";
}

#[async_trait]
impl Tool for HttpCall {
    fn spec(&self) -> ToolSpec {
        let mut endpoint_desc = String::new();
        for (i, spec) in self.registry.endpoints.values().enumerate() {
            if i > 0 {
                endpoint_desc.push_str("; ");
            }
            endpoint_desc.push_str(&format!(
                "{} ({}){}",
                spec.name,
                format!("{:?}", spec.method).to_uppercase(),
                if spec.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", spec.description)
                }
            ));
        }
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Call a pre-registered business API endpoint by name. The operator \
                 configures the URL and credentials; you choose the endpoint and pass \
                 `params` (path placeholders and query values) and, for POST/PUT/PATCH \
                 endpoints, a JSON `body`. Arbitrary URLs are not accepted — use WebFetch \
                 for public web pages. Registered endpoints: {endpoint_desc}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "endpoint": {
                        "type": "string",
                        "enum": self.registry.names().collect::<Vec<_>>(),
                        "description": endpoint_desc
                    },
                    "params": {
                        "type": "object",
                        "description": "Path placeholder values ({name}) plus optional query parameters. Values must be strings, numbers or booleans."
                    },
                    "body": {
                        "type": "object",
                        "description": "JSON request body. Only accepted on POST/PUT/PATCH endpoints."
                    }
                },
                "required": ["endpoint"]
            }),
        }
    }

    fn side_effect_class(&self) -> super::audit::ToolSideEffect {
        // A registry that only contains read-only endpoints (GET/HEAD) is
        // safe to classify ReadOnly; any mutating endpoint makes the whole
        // tool External.
        if self
            .registry
            .endpoints
            .values()
            .any(|e| e.method.is_mutating() || !e.idempotent)
        {
            super::audit::ToolSideEffect::External
        } else {
            super::audit::ToolSideEffect::ReadOnly
        }
    }

    fn kind(&self) -> ToolKind {
        if self
            .registry
            .endpoints
            .values()
            .any(|e| e.method.is_mutating() || !e.idempotent)
        {
            ToolKind::Other
        } else {
            ToolKind::Fetch
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let name = args["endpoint"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: Self::NAME.to_string(),
                message: "missing `endpoint`".into(),
            })?;
        let spec = self.registry.get(name).ok_or_else(|| {
            // Names only — the error must not leak internal URLs.
            Error::BadToolArgs {
                name: Self::NAME.to_string(),
                message: format!(
                    "unknown endpoint '{name}'; registered endpoints: {}",
                    self.registry
                        .names()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        })?;
        self.call(spec, &args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn registry_json(base: &str, extra: Value) -> String {
        let mut e = json!({
            "get_order": {
                "method": "GET",
                "base_url": base,
                "path": "/api/orders/{order_id}",
                "description": "查询订单状态",
                "allow_private": true
            }
        });
        if let (Value::Object(map), Value::Object(extra)) = (&mut e, extra) {
            for (k, v) in extra {
                map.insert(k, v);
            }
        }
        json!({ "endpoints": e }).to_string()
    }

    fn loopback_registry(extra: Value) -> EndpointRegistry {
        EndpointRegistry::from_json(&registry_json("http://127.0.0.1:1", extra))
            .expect("registry should build")
    }

    fn spec_with(base: &str, allow_private: bool) -> Result<EndpointSpec> {
        EndpointRegistry::from_json(
            &json!({ "endpoints": { "ep": {
                "method": "GET",
                "base_url": base,
                "path": "/x",
                "allow_private": allow_private
            }}})
            .to_string(),
        )
        .map(|r| r.get("ep").expect("ep").clone())
    }

    // ── config parsing / validation ─────────────────────────────────────

    #[test]
    fn parse_applies_defaults() {
        let reg = EndpointRegistry::from_json(
            &json!({ "endpoints": { "post_ticket": {
                "method": "POST", "base_url": "https://api.example.com", "path": "/t"
            }}})
            .to_string(),
        )
        .expect("ok");
        let e = reg.get("post_ticket").expect("ep");
        assert_eq!(e.method, Method::Post);
        assert!(!e.idempotent, "POST defaults to non-idempotent");
        assert_eq!(e.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(e.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        assert_eq!(e.auth_header, "Authorization");
        assert_eq!(e.auth_prefix, DEFAULT_AUTH_PREFIX);
        assert!(!e.allow_private);
    }

    #[test]
    fn parse_rejects_unknown_method() {
        let err = EndpointRegistry::from_json(
            &json!({ "endpoints": { "e": {
                "method": "TRACE", "base_url": "https://api.example.com", "path": "/t"
            }}})
            .to_string(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown method"), "{err}");
    }

    #[test]
    fn parse_rejects_bad_name_and_path() {
        let bad_name = EndpointRegistry::from_json(
            &json!({ "endpoints": { "bad name!": {
                "method": "GET", "base_url": "https://api.example.com", "path": "/t"
            }}})
            .to_string(),
        )
        .unwrap_err();
        assert!(bad_name.to_string().contains("invalid"), "{bad_name}");

        let bad_path = EndpointRegistry::from_json(
            &json!({ "endpoints": { "e": {
                "method": "GET", "base_url": "https://api.example.com", "path": "t"
            }}})
            .to_string(),
        )
        .unwrap_err();
        assert!(
            bad_path.to_string().contains("must start with"),
            "{bad_path}"
        );

        let unterminated = EndpointRegistry::from_json(
            &json!({ "endpoints": { "e": {
                "method": "GET", "base_url": "https://api.example.com", "path": "/t/{x"
            }}})
            .to_string(),
        )
        .unwrap_err();
        assert!(
            unterminated.to_string().contains("unterminated"),
            "{unterminated}"
        );
    }

    // ── load-time base-URL guard ────────────────────────────────────────

    #[test]
    fn guard_rejects_private_without_opt_in() {
        for base in [
            "http://127.0.0.1:8899",
            "http://10.0.0.1",
            "http://192.168.1.1",
            "http://172.16.0.1",
            "http://localhost:8899",
            "http://2130706433/", // lenient spelling of 127.0.0.1
        ] {
            let err = spec_with(base, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("allow_private"),
                "expected allow_private rejection for {base}: {msg}"
            );
        }
    }

    #[test]
    fn guard_opt_in_allows_private_but_not_link_local() {
        assert!(spec_with("http://127.0.0.1:8899", true).is_ok());
        assert!(spec_with("http://10.1.2.3:8899", true).is_ok());
        assert!(
            spec_with("http://[::1]/", true).is_ok(),
            "v6 loopback is opt-in allowed"
        );
        // IMDS / metadata / unspecified stay blocked even with the opt-in.
        for base in [
            "http://169.254.169.254/latest",
            "http://metadata.google.internal/",
            "http://0.0.0.0/",
            "http://[::ffff:169.254.169.254]/",
        ] {
            let err = spec_with(base, true).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("never allowed"),
                "expected hard rejection for {base}: {msg}"
            );
        }
    }

    #[test]
    fn guard_rejects_userinfo_fragment_and_scheme() {
        for (base, fragment) in [
            ("http://user:pass@api.example.com", "userinfo"),
            ("https://api.example.com/#frag", "fragment"),
            ("ftp://api.example.com", "scheme"),
        ] {
            let err = spec_with(base, false).unwrap_err();
            assert!(
                err.to_string().contains(fragment),
                "expected {fragment} rejection for {base}: {err}"
            );
        }
    }

    #[test]
    fn discover_missing_file_is_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(EndpointRegistry::discover(tmp.path()).is_none());
    }

    #[test]
    fn discover_valid_file_yields_registry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(".recursive");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("endpoints.json"),
            registry_json("http://127.0.0.1:8899", json!({})),
        )
        .expect("write");
        let reg = EndpointRegistry::discover(tmp.path()).expect("registry");
        assert_eq!(reg.len(), 1);
        assert!(reg.get("get_order").is_some());
    }

    // ── schema hygiene ──────────────────────────────────────────────────

    #[test]
    fn spec_never_exposes_urls() {
        let tool = HttpCall::new(loopback_registry(json!({
            "create_ticket": {
                "method": "POST",
                "base_url": "http://127.0.0.1:1",
                "path": "/tickets",
                "allow_private": true
            }
        })));
        let spec_json = serde_json::to_string(&tool.spec()).expect("spec json");
        assert!(spec_json.contains("get_order"));
        assert!(spec_json.contains("create_ticket"));
        assert!(
            !spec_json.contains("127.0.0.1"),
            "spec must not contain the base URL: {spec_json}"
        );
    }

    #[test]
    fn side_effect_class_tracks_registry() {
        let read_only = HttpCall::new(loopback_registry(json!({})));
        assert_eq!(
            read_only.side_effect_class(),
            super::super::audit::ToolSideEffect::ReadOnly
        );
        let mutating = HttpCall::new(loopback_registry(json!({
            "create_ticket": {
                "method": "POST",
                "base_url": "http://127.0.0.1:1",
                "path": "/tickets",
                "allow_private": true
            }
        })));
        assert_eq!(
            mutating.side_effect_class(),
            super::super::audit::ToolSideEffect::External
        );
    }

    // ── execution against a loopback mock (allow_private opt-in) ────────

    /// One-shot raw-HTTP mock: captures the first request (method, path,
    /// headers, body) and replies with a canned response.
    struct Captured {
        request_line: String,
        headers: String,
        body: String,
    }

    fn spawn_mock(
        status_line: String,
        body: String,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Captured>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).ok();
            stream.flush().ok();
            let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
            let mut head_lines = head.split("\r\n");
            let request_line = head_lines.next().unwrap_or_default().to_string();
            let headers = head_lines.collect::<Vec<_>>().join("\r\n");
            Captured {
                request_line,
                headers,
                body: body.to_string(),
            }
        });
        (addr, handle)
    }

    fn tool_at(addr: std::net::SocketAddr) -> HttpCall {
        let reg = EndpointRegistry::from_json(&registry_json(&format!("http://{addr}"), json!({})))
            .expect("registry");
        HttpCall::new(reg)
    }

    #[tokio::test]
    async fn get_hits_registered_endpoint() {
        let (addr, handle) = spawn_mock(
            "HTTP/1.1 200 OK".to_string(),
            r#"{"status":"已发货"}"#.to_string(),
        );
        let tool = tool_at(addr);
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A-1001"}})),
        )
        .await
        .expect("no hang")
        .expect("ok");
        let captured = handle.join().expect("join");
        assert_eq!(captured.request_line, "GET /api/orders/A-1001 HTTP/1.1");
        assert!(out.contains("HTTP 200"), "{out}");
        assert!(out.contains("已发货"), "{out}");
    }

    #[tokio::test]
    async fn post_sends_json_body() {
        let (addr, handle) = spawn_mock(
            "HTTP/1.1 201 Created".to_string(),
            r#"{"id":"t-1"}"#.to_string(),
        );
        let reg = EndpointRegistry::from_json(
            &json!({ "endpoints": { "create_ticket": {
                "method": "POST",
                "base_url": format!("http://{addr}"),
                "path": "/tickets",
                "allow_private": true
            }}})
            .to_string(),
        )
        .expect("registry");
        let tool = HttpCall::new(reg);
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({
                "endpoint": "create_ticket",
                "body": {"order_id": "A-1001", "reason": "damaged"}
            })),
        )
        .await
        .expect("no hang")
        .expect("ok");
        let captured = handle.join().expect("join");
        assert_eq!(captured.request_line, "POST /tickets HTTP/1.1");
        assert!(
            captured
                .headers
                .to_lowercase()
                .contains("content-type: application/json"),
            "{}",
            captured.headers
        );
        assert!(
            captured.body.contains("\"order_id\":\"A-1001\""),
            "{}",
            captured.body
        );
        assert!(out.contains("HTTP 201"), "{out}");
    }

    #[tokio::test]
    async fn extra_params_become_query_pairs() {
        let (addr, handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let tool = tool_at(addr);
        tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({
                "endpoint": "get_order",
                "params": {"order_id": "A-1001", "verbose": true, "lang": "zh"}
            })),
        )
        .await
        .expect("no hang")
        .expect("ok");
        let captured = handle.join().expect("join");
        // BTreeMap iteration order: "lang" < "verbose".
        assert_eq!(
            captured.request_line,
            "GET /api/orders/A-1001?lang=zh&verbose=true HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn path_params_are_percent_encoded() {
        let (addr, handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let tool = tool_at(addr);
        tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({
                "endpoint": "get_order",
                "params": {"order_id": "a b/..?x=1"}
            })),
        )
        .await
        .expect("no hang")
        .expect("ok");
        let captured = handle.join().expect("join");
        assert_eq!(
            captured.request_line, "GET /api/orders/a%20b%2F..%3Fx%3D1 HTTP/1.1",
            "a param value must not escape its path segment: {}",
            captured.request_line
        );
    }

    #[tokio::test]
    async fn unknown_endpoint_error_has_no_url() {
        let (addr, _handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let tool = tool_at(addr);
        let err = tool.execute(json!({"endpoint": "nope"})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown endpoint 'nope'"), "{msg}");
        assert!(msg.contains("get_order"), "{msg}");
        assert!(!msg.contains("127.0.0.1"), "error leaked a URL: {msg}");
    }

    #[tokio::test]
    async fn missing_placeholder_is_rejected() {
        let (addr, _handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let tool = tool_at(addr);
        let err = tool
            .execute(json!({"endpoint": "get_order", "params": {}}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'order_id'"), "{err}");
    }

    #[tokio::test]
    async fn body_on_get_endpoint_is_rejected() {
        let (addr, _handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let tool = tool_at(addr);
        let err = tool
            .execute(json!({"endpoint": "get_order", "params": {"order_id": "A"},
                            "body": {"x": 1}}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("POST/PUT/PATCH"), "{err}");
    }

    #[tokio::test]
    async fn non_2xx_is_data_not_error() {
        let (addr, handle) = spawn_mock(
            "HTTP/1.1 404 Not Found".to_string(),
            r#"{"error":"not found"}"#.to_string(),
        );
        let tool = tool_at(addr);
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "X"}})),
        )
        .await
        .expect("no hang")
        .expect("404 from a reached endpoint is data, not a tool error");
        handle.join().expect("join");
        assert!(out.contains("HTTP 404"), "{out}");
        assert!(out.contains("not found"), "{out}");
    }

    #[tokio::test]
    async fn oversized_response_is_truncated() {
        let (addr, handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "a".repeat(2000));
        let reg = EndpointRegistry::from_json(
            &json!({ "endpoints": { "get_order": {
                "method": "GET",
                "base_url": format!("http://{addr}"),
                "path": "/api/orders/{order_id}",
                "max_response_bytes": 100,
                "allow_private": true
            }}})
            .to_string(),
        )
        .expect("registry");
        let tool = HttpCall::new(reg);
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A"}})),
        )
        .await
        .expect("no hang")
        .expect("ok");
        handle.join().expect("join");
        assert!(out.contains("truncated at 100 bytes"), "{out}");
        assert!(
            out.len() < 400,
            "output should be capped, got {}",
            out.len()
        );
    }

    #[tokio::test]
    async fn redirect_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Consume the request first: closing with unread request data
            // RSTs the client's in-flight send and the test would see a
            // connect/send error instead of the 302.
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(resp.as_bytes()).ok();
            stream.flush().ok();
        });
        let tool = tool_at(addr);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A"}})),
        )
        .await
        .expect("no hang");
        handle.join().expect("join");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("redirects are disabled"), "{msg}");
        assert!(
            !msg.contains("169.254"),
            "error should not advertise the hop: {msg}"
        );
    }

    #[tokio::test]
    async fn transport_error_has_no_url() {
        // Port 1 on loopback: nothing listens; the connect must fail fast
        // and the error must name the endpoint, not the URL.
        let reg = EndpointRegistry::from_json(&registry_json("http://127.0.0.1:1", json!({})))
            .expect("registry");
        let tool = HttpCall::new(reg);
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A"}})),
        )
        .await
        .expect("no hang");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("endpoint 'get_order'"), "{msg}");
        assert!(
            msg.contains("connection failed") || msg.contains("timed out"),
            "{msg}"
        );
        assert!(!msg.contains("http://"), "error leaked a URL: {msg}");
    }

    #[tokio::test]
    async fn auth_env_is_injected_and_missing_env_names_only_the_var() {
        let (addr, handle) = spawn_mock("HTTP/1.1 200 OK".to_string(), "{}".to_string());
        let reg = EndpointRegistry::from_json(
            &json!({ "endpoints": { "get_order": {
                "method": "GET",
                "base_url": format!("http://{addr}"),
                "path": "/api/orders/{order_id}",
                "auth_env": "ORDER_API_TOKEN",
                "allow_private": true
            }}})
            .to_string(),
        )
        .expect("registry");
        let tool = HttpCall::new(reg.clone()).with_auth_resolver(|name| {
            if name == "ORDER_API_TOKEN" {
                Some("sekrit-token-value".to_string())
            } else {
                None
            }
        });
        tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A"}})),
        )
        .await
        .expect("no hang")
        .expect("ok");
        let captured = handle.join().expect("join");
        assert!(
            captured
                .headers
                .to_lowercase()
                .contains("authorization: bearer sekrit-token-value"),
            "token must be injected as a bearer header: {}",
            captured.headers
        );

        // Missing env var: the error names the var, never a value or URL.
        let tool = HttpCall::new(reg).with_auth_resolver(|_| None);
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(json!({"endpoint": "get_order", "params": {"order_id": "A"}})),
        )
        .await
        .expect("no hang")
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ORDER_API_TOKEN"), "{msg}");
        assert!(msg.contains("not set"), "{msg}");
        assert!(!msg.contains("127.0.0.1"), "{msg}");
    }

    // ── helpers ─────────────────────────────────────────────────────────

    #[test]
    fn encode_segment_escapes_everything_unsafe() {
        assert_eq!(encode_segment("A-z_9.~"), "A-z_9.~");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("/"), "%2F");
        assert_eq!(encode_segment("好"), "%E5%A5%BD");
        assert_eq!(encode_segment("%"), "%25");
    }

    #[test]
    fn placeholders_extracts_names() {
        let names = placeholders("/api/orders/{order_id}/items/{sku}").expect("ok");
        assert_eq!(names, vec!["order_id", "sku"]);
        assert!(placeholders("/no/placeholders").expect("ok").is_empty());
    }
}

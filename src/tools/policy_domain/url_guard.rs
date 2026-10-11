//! Shared SSRF guard for outbound-HTTP tools.
//!
//! Extracted from `web_fetch.rs` during Goal 371 so that every tool that makes
//! an outbound HTTP request to a model-supplied URL rejects private/internal/
//! loopback targets **before any socket opens**. Consumers: `WebFetch`,
//! `a2a_call`, `a2a_card`, `a2a_task_check`.
//!
//! Host extraction and normalization were replaced in Goal 377 with the
//! `url` crate's WHATWG parser — the same parsing reqwest applies before
//! connecting — closing the RFC-vs-string mismatch that let userinfo /
//! fragment / query / backslash forms and IPv4-mapped / trailing-dot
//! literals reach private hosts while the hand-rolled splitter saw a
//! "hostname". Remaining known issues (hardcoded `"WebFetch"` name in error
//! messages, the lack of a redirect policy) are still tracked separately.
//!
//! Issue #169 added the **connection-time** half: `validate_url` can only see
//! the literal URL, so a *domain* that resolves to a private address
//! (`http://127.0.0.1.nip.io/`, `http://169.254.169.254.nip.io/`, a DNS
//! rebinding zone) used to reach it unchecked. [`GuardedResolver`] is a
//! `reqwest::dns::Resolve` implementation that vets every DNS answer with
//! [`is_private_ip`] before a socket opens; web tools install it with
//! [`guarded_resolver`]. [`is_private_ip`] also grew the reserved blocks that
//! were missing (CGNAT `100.64.0.0/10`, `0.0.0.0/8`, IPv4-compatible
//! `::a.b.c.d`, NAT64).

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use url::Host;

use crate::error::{Error, Result};

/// Validate URL and block SSRF targets (private/loopback/link-local addresses).
pub(crate) fn validate_url(url: &str) -> Result<String> {
    // Cheap prefix gate keeps today's error message for non-http(s) inputs
    // (web_fetch tests match on "http:// or https://"); `Url::parse` would
    // otherwise report "relative URL without a base" for the same inputs.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::BadToolArgs {
            name: "WebFetch".into(),
            message: "URL must start with http:// or https://".into(),
        });
    }

    let parsed = url::Url::parse(url).map_err(|e| Error::BadToolArgs {
        name: "WebFetch".into(),
        message: format!("invalid URL: {e}"),
    })?;

    // Scheme check stays (belt-and-braces — the prefix gate above already
    // guarantees http/https for anything that parses).
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(Error::BadToolArgs {
            name: "WebFetch".into(),
            message: "URL must start with http:// or https://".into(),
        });
    }

    // Reject embedded credentials: userinfo is a primary obfuscation surface
    // and web tools have no legitimate use for them.
    if !parsed.username().is_empty() {
        return Err(Error::BadToolArgs {
            name: "WebFetch".into(),
            message: "SSRF protection: userinfo in URL is not allowed".into(),
        });
    }

    match parsed.host() {
        Some(Host::Ipv4(v4)) => {
            let ip = IpAddr::V4(v4);
            if is_private_ip(ip) {
                return Err(reject(ip));
            }
        }
        Some(Host::Ipv6(v6)) => {
            // IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is routed by the OS to the
            // mapped v4 address — run the v4 private check so loopback/IMDS
            // mapped forms (`[::ffff:127.0.0.1]`, `[::ffff:169.254.169.254]`)
            // are blocked even though the v6 checks below would pass them.
            if let Some(v4) = v6.to_ipv4_mapped() {
                let ip = IpAddr::V4(v4);
                if is_private_ip(ip) {
                    return Err(reject(ip));
                }
            } else if is_private_ip(IpAddr::V6(v6)) {
                return Err(reject(IpAddr::V6(v6)));
            }
        }
        Some(Host::Domain(d)) => {
            // Normalize: lowercase, and strip ONE trailing dot (a fully
            // qualified host still resolves to the same address).
            let host = d.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(host.as_str());

            // Block well-known SSRF hostnames regardless of capitalisation.
            if host == "localhost"
                || host.ends_with(".localhost")
                || host == "metadata.google.internal"
            {
                return Err(Error::BadToolArgs {
                    name: "WebFetch".into(),
                    message: format!("SSRF protection: host '{host}' is not allowed"),
                });
            }

            // If the host parses as a bare IP address, block private/loopback/
            // link-local ranges.
            if let Ok(ip) = host.parse::<IpAddr>() {
                if is_private_ip(ip) {
                    return Err(reject(ip));
                }
            } else if let Some(ip) = parse_lenient_ipv4(host) {
                // Lenient IPv4 spellings (decimal/hex/short-form) that the OS
                // resolver accepts but `IpAddr::parse` rejects — block the
                // same ranges so `http://2130706433/` can't reach loopback or
                // IMDS. (With the WHATWG parser most of these arrive as
                // `Host::Ipv4`, but keep the fallback for domain-form cases.)
                if is_private_ip(ip) {
                    return Err(reject(ip));
                }
            }
        }
        None => {
            return Err(Error::BadToolArgs {
                name: "WebFetch".into(),
                message: "SSRF protection: URL has no host".into(),
            });
        }
    }

    Ok(url.to_string())
}

/// Returns true for IP addresses that must not be reached via outbound HTTP
/// tools (loopback, private RFC-1918, link-local, and cloud metadata ranges,
/// plus the reserved blocks that alias or embed them).
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()           // 127.0.0.0/8
                || v4.is_private()     // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()  // 169.254.0.0/16 (AWS IMDS et al.)
                || v4.is_broadcast()   // 255.255.255.255
                || v4.is_unspecified() // 0.0.0.0
                // 0.0.0.0/8 "this network" (RFC 1122 §3.2.1.3) — the whole
                // block is non-routable, not just 0.0.0.0 (issue #169).
                || octets[0] == 0
                // 100.64.0.0/10 CGNAT (RFC 6598) — carrier NAT space, never
                // a public destination (issue #169).
                || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible
            // (`::a.b.c.d`, deprecated) addresses are routed to the embedded
            // v4 address — classify them by that address. `to_ipv4` (unlike
            // `to_ipv4_mapped`, the only form the old check handled) covers
            // both layouts (issue #169).
            if let Some(v4) = v6.to_ipv4() {
                return is_private_ip(IpAddr::V4(v4));
            }
            // NAT64 translation prefixes embed (or target) non-global v4
            // space — see `nat64_rejects`.
            if nat64_rejects(&v6) {
                return true;
            }
            v6.is_loopback()           // ::1
                || v6.is_unspecified() // ::
                || v6.is_multicast()
                // fc00::/7 ULA (private)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// True when `v6` belongs to a NAT64 translation prefix that can only reach
/// non-global IPv4 space.
///
/// - `64:ff9b:1::/48` (RFC 8215) is *explicitly* the local-use prefix: it
///   exists to translate non-global IPv4 destinations, so every address in it
///   is rejected outright.
/// - `64:ff9b::/96` (RFC 6052 well-known prefix) is what DNS64 uses to
///   synthesise **global** destinations on IPv6-only networks, so it is not
///   rejected wholesale: only the embedded v4 address decides. That keeps
///   DNS64/NAT64 deployments working for public targets while still refusing
///   `64:ff9b::7f00:1` (127.0.0.1) and `64:ff9b::a00:1` (10.0.0.1).
fn nat64_rejects(v6: &Ipv6Addr) -> bool {
    let s = v6.segments();
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001 {
        return true; // 64:ff9b:1::/48
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        // 64:ff9b::/96 — the v4 address occupies the last 32 bits.
        let v4 = Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        );
        return is_private_ip(IpAddr::V4(v4));
    }
    false
}

/// Build the SSRF rejection error shared by the canonical and lenient IP
/// branches so the error shape stays byte-identical.
fn reject(ip: IpAddr) -> Error {
    Error::BadToolArgs {
        name: "WebFetch".into(),
        message: format!("SSRF protection: IP address '{ip}' is not routable"),
    }
}

/// Parse a lenient IPv4 spelling that the OS resolver accepts but
/// `std::net::IpAddr::parse` rejects, following POSIX `inet_aton` semantics:
///
/// - `2130706433`  → 127.0.0.1 (single 32-bit part)
/// - `0x7f000001`  → 127.0.0.1 (hex)
/// - `127.1`       → 127.0.0.1 (short form: last part holds 24 bits)
/// - `0177.0.0.1`  → 127.0.0.1 (leading-zero octal)
///
/// Returns `None` for any malformed input (including real hostnames), so
/// callers can fall back to treating the host as a DNS name.
///
/// Also consumed by the `HttpCall` endpoint-registry load-time guard
/// (`http_call.rs`), which classifies operator-configured base URLs with
/// the same lenient-parse semantics.
pub(crate) fn parse_lenient_ipv4(s: &str) -> Option<IpAddr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }

    let mut value: u32 = 0;
    for (i, part) in parts.iter().enumerate() {
        let n = parse_lenient_part(part)?;
        if i + 1 == parts.len() {
            // The last part may span the remaining bytes: 1 part → 32 bits,
            // 2 parts → 24 bits, 3 parts → 16 bits, 4 parts → 8 bits.
            let remaining_bytes = 4 - i;
            let max_last = if remaining_bytes >= 4 {
                u32::MAX
            } else {
                (1u32 << (8 * remaining_bytes)) - 1
            };
            if n > max_last {
                return None;
            }
            value |= n;
        } else {
            if n > 0xFF {
                return None;
            }
            value |= n << (8 * (3 - i));
        }
    }

    Some(IpAddr::V4(Ipv4Addr::from(value)))
}

/// Parse one dot-separated part of a lenient IPv4 spelling.
///
/// Radix rules (POSIX, not macOS `getaddrinfo`): `0x`/`0X` prefix → hex;
/// leading `0` with length > 1 → octal; otherwise decimal. A bare `"0"` is
/// decimal zero; `"00"` is octal zero.
fn parse_lenient_part(part: &str) -> Option<u32> {
    if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        if hex.is_empty() {
            return None;
        }
        return u32::from_str_radix(hex, 16).ok();
    }
    let first = part.as_bytes().first()?;
    if !first.is_ascii_digit() {
        return None;
    }
    if part.len() > 1 && part.starts_with('0') {
        return u32::from_str_radix(&part[1..], 8).ok();
    }
    part.parse::<u32>().ok()
}

// ── Issue #169: connection-time DNS guard ──────────────────────────────────
//
// `validate_url` above is the first layer (literal host). A *name* that
// resolves into private space is the second: the resolver below runs inside
// reqwest's connect path, so the check happens after the system resolver
// answered — for the first lookup and for any rebinding re-resolve.

/// Opt-out for the connection-time check: `RECURSIVE_SSRF_ALLOW_PRIVATE_DNS=1`
/// (also `true`/`yes`/`on`, case-insensitive) allows DNS answers that point at
/// private addresses. Needed by deployments that legitimately resolve service
/// names into private space (enterprise proxies, internal gateways). Default
/// is unset ⇒ the guard is **on**.
pub(crate) const ALLOW_PRIVATE_DNS_ENV: &str = "RECURSIVE_SSRF_ALLOW_PRIVATE_DNS";

/// Reject the whole DNS answer when any address is private/reserved.
///
/// Rejecting rather than filtering the private addresses out matters: with a
/// mixed answer (one public, one private) filtering would let the request
/// fall through to another address, so a hostile record could still steer the
/// connection. `allow_private` is the explicit form of
/// [`ALLOW_PRIVATE_DNS_ENV`] so tests never mutate process env.
pub(crate) fn guard_addrs(
    addrs: Vec<SocketAddr>,
    allow_private: bool,
) -> std::io::Result<Vec<SocketAddr>> {
    if allow_private {
        return Ok(addrs);
    }
    for addr in &addrs {
        if is_private_ip(addr.ip()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "SSRF protection: refusing to connect to private address {} resolved from DNS",
                    addr.ip()
                ),
            ));
        }
    }
    Ok(addrs)
}

/// True when the operator set the [`ALLOW_PRIVATE_DNS_ENV`] opt-out.
fn private_dns_opt_out() -> bool {
    parse_opt_out(std::env::var(ALLOW_PRIVATE_DNS_ENV).ok().as_deref())
}

/// Accepted truthy spellings for [`ALLOW_PRIVATE_DNS_ENV`].
fn parse_opt_out(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        }
        None => false,
    }
}

/// Boxed future returned by [`AddrSource::lookup`].
pub(crate) type LookupFuture =
    Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>;

/// Address source for [`GuardedResolver`]: the production implementation is
/// the system resolver, tests substitute a stub so no real DNS is involved.
pub(crate) trait AddrSource: Send + Sync {
    fn lookup(&self, host: &str) -> LookupFuture;
}

/// System resolver (getaddrinfo on tokio's blocking pool).
pub(crate) struct SystemResolver;

impl AddrSource for SystemResolver {
    fn lookup(&self, host: &str) -> LookupFuture {
        let host = host.to_string();
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addrs.collect())
        })
    }
}

/// `reqwest::dns::Resolve` wrapper that refuses private/reserved DNS answers.
///
/// Residual risk (documented, not fixed): when reqwest routes the request
/// through a proxy (`HTTP_PROXY`/`HTTPS_PROXY`), the *proxy* resolves the
/// target name, so this resolver only vets the proxy's own address. That is
/// why [`ALLOW_PRIVATE_DNS_ENV`] exists for proxy deployments, and why the
/// guard is not the only layer (`validate_url` still runs first).
pub(crate) struct GuardedResolver {
    source: Arc<dyn AddrSource>,
    allow_private: bool,
}

impl GuardedResolver {
    /// Production resolver: system DNS, private answers rejected unless
    /// [`ALLOW_PRIVATE_DNS_ENV`] opts out.
    pub(crate) fn from_env() -> Self {
        Self {
            source: Arc::new(SystemResolver),
            allow_private: private_dns_opt_out(),
        }
    }

    /// Test seam: caller supplies the address source and the opt-out flag.
    #[cfg(test)]
    pub(crate) fn with_source(source: Arc<dyn AddrSource>, allow_private: bool) -> Self {
        Self {
            source,
            allow_private,
        }
    }
}

/// Map an IO failure into the boxed error type `reqwest::dns::Resolving`
/// expects.
fn boxed_io_error(e: std::io::Error) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let source = Arc::clone(&self.source);
        let allow_private = self.allow_private;
        let fut: reqwest::dns::Resolving = Box::pin(async move {
            let addrs = source.lookup(&host).await.map_err(boxed_io_error)?;
            let guarded = match guard_addrs(addrs, allow_private) {
                Ok(addrs) => addrs,
                Err(e) => {
                    // reqwest only renders "error sending request" and the
                    // tool error flattens the cause, so log the reason here —
                    // it is the only place that knows both host and address.
                    tracing::warn!(
                        host = %host,
                        error = %e,
                        "SSRF guard refused a DNS answer; set {ALLOW_PRIVATE_DNS_ENV}=1 to allow private targets"
                    );
                    return Err(boxed_io_error(e));
                }
            };
            Ok(Box::new(guarded.into_iter()) as reqwest::dns::Addrs)
        });
        fut
    }
}

/// The production [`GuardedResolver`], ready for
/// `ClientBuilder::dns_resolver`.
pub(crate) fn guarded_resolver() -> Arc<GuardedResolver> {
    Arc::new(GuardedResolver::from_env())
}

// ── Issue #169: environment gate for the wiring pins ───────────────────────

/// Test gate for the wiring pins in `web_fetch.rs` / `a2a.rs`.
///
/// Those pins observe the connection-time guard through the *production*
/// client, so the loopback request must not be intercepted by an ambient proxy.
/// reqwest consults `NO_PROXY` first, then `no_proxy`; when the effective list
/// does not exempt the loopback names, an `HTTP_PROXY` / `HTTPS_PROXY` /
/// `ALL_PROXY` in the environment turns the pin into a **false green**: the
/// guarded arm dies at the unreachable proxy, so `expect_err` plus "no hit on
/// the listener" are satisfied for the wrong reason and deleting the guard
/// changes nothing. `.cargo/config.toml` provides
/// `NO_PROXY=127.0.0.1,localhost,::1` for cargo-invoked processes, but it
/// deliberately does not override a value the developer already exported —
/// hence this loud gate instead of a quietly weakened pin.
///
/// # Panics
///
/// When the effective no-proxy list is unset, or does not exempt both
/// `localhost` and `127.0.0.1`.
#[cfg(test)]
pub(crate) fn assert_loopback_bypasses_proxy() {
    let proxies: Vec<&str> = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .filter(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    .collect();

    // reqwest's precedence: `NO_PROXY` if set, otherwise `no_proxy`.
    let decisive = std::env::var("NO_PROXY")
        .map(|value| ("NO_PROXY", value))
        .ok()
        .or_else(|| {
            std::env::var("no_proxy")
                .map(|value| ("no_proxy", value))
                .ok()
        });
    let (name, value) = match decisive {
        Some(pair) => pair,
        None => panic!(
            "connection_time_guard relies on the loopback names being exempted from any ambient \
             proxy, but neither NO_PROXY nor no_proxy is set. Export \
             `NO_PROXY=127.0.0.1,localhost,::1`, or run the suite through cargo (whose \
             .cargo/config.toml provides it). Proxy env vars currently set: {proxies:?}"
        ),
    };

    let missing = loopback_missing_from_no_proxy(&value);
    assert!(
        missing.is_empty(),
        "connection_time_guard would pass for the wrong reason: {name}={value:?} does not exempt \
         {missing:?}, so the request would be sent to the proxy and fail there instead of being \
         refused by the connection-time SSRF guard (deleting the guard would stay green). Unset \
         {name} or add the loopback names. Proxy env vars currently set: {proxies:?}"
    );
}

/// The loopback names in `value` (a `NO_PROXY`-format list) that are **not**
/// exempted.
#[cfg(test)]
fn loopback_missing_from_no_proxy(value: &str) -> Vec<&'static str> {
    ["localhost", "127.0.0.1"]
        .into_iter()
        .filter(|host| !no_proxy_covers(value, host))
        .collect()
}

/// True when a `NO_PROXY`-format list exempts `host`: comma / whitespace
/// separated, case-insensitive, one leading `.` and the `*` wildcard tolerated.
#[cfg(test)]
fn no_proxy_covers(value: &str, host: &str) -> bool {
    value
        .split([',', ';', ' ', '\t', '\n', '\r'])
        .map(|entry| entry.trim().trim_start_matches('.').to_ascii_lowercase())
        .any(|entry| entry == "*" || entry == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn parse_lenient_ipv4_decimal_single_part() {
        assert_eq!(parse_lenient_ipv4("2130706433"), Some(v4(127, 0, 0, 1)));
        assert_eq!(
            parse_lenient_ipv4("2852039166"),
            Some(v4(169, 254, 169, 254))
        );
        assert_eq!(parse_lenient_ipv4("3232235521"), Some(v4(192, 168, 0, 1)));
        assert_eq!(parse_lenient_ipv4("0"), Some(v4(0, 0, 0, 0)));
        assert_eq!(
            parse_lenient_ipv4("4294967295"),
            Some(v4(255, 255, 255, 255))
        );
    }

    #[test]
    fn parse_lenient_ipv4_hex() {
        assert_eq!(parse_lenient_ipv4("0x7f000001"), Some(v4(127, 0, 0, 1)));
        assert_eq!(
            parse_lenient_ipv4("0xa9fea9fe"),
            Some(v4(169, 254, 169, 254))
        );
        assert_eq!(parse_lenient_ipv4("0X7F000001"), Some(v4(127, 0, 0, 1)));
    }

    #[test]
    fn parse_lenient_ipv4_short_form() {
        assert_eq!(parse_lenient_ipv4("127.1"), Some(v4(127, 0, 0, 1)));
        assert_eq!(parse_lenient_ipv4("127.0.1"), Some(v4(127, 0, 0, 1)));
    }

    #[test]
    fn parse_lenient_ipv4_octal() {
        // POSIX semantics: leading zero means octal (macOS `getaddrinfo`
        // would read `0177` as decimal 177 — we pin glibc/Linux behavior).
        assert_eq!(parse_lenient_ipv4("0177.0.0.1"), Some(v4(127, 0, 0, 1)));
        assert_eq!(parse_lenient_ipv4("00"), Some(v4(0, 0, 0, 0)));
    }

    #[test]
    fn parse_lenient_ipv4_malformed() {
        assert_eq!(parse_lenient_ipv4(""), None);
        assert_eq!(parse_lenient_ipv4("256.1.1.1"), None);
        assert_eq!(parse_lenient_ipv4("1.2.65536"), None);
        assert_eq!(parse_lenient_ipv4("0x"), None);
        assert_eq!(parse_lenient_ipv4("1.2.3.4.5"), None);
        assert_eq!(parse_lenient_ipv4("abc"), None);
        assert_eq!(parse_lenient_ipv4("127.0.0.999"), None);
    }

    #[test]
    fn validate_url_blocks_lenient_ssrf_spellings() {
        // Every row of the bypass table: the OS resolver maps these to
        // private/loopback/link-local addresses that `IpAddr::parse` misses.
        for url in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://127.1/",
            "http://127.0.1/",
            "http://0177.0.0.1/",
            "http://2852039166/latest/meta-data/",
            "http://0xa9fea9fe/",
            "http://3232235521/",
            "http://0/",
            "http://4294967295/",
        ] {
            assert!(validate_url(url).is_err(), "expected {url} to be rejected");
        }
    }

    #[test]
    fn validate_url_still_accepts_hostnames() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("http://example.com:8080/path").is_ok());
    }

    // ── Goal 377: RFC-vs-string parsing mismatch bypasses ───────────────

    /// userinfo: reqwest drops `user@` and connects to 127.0.0.1; the old
    /// hand-rolled splitter saw `user@127.0.0.1` (not an IP) and passed it.
    #[test]
    fn validate_url_blocks_userinfo_bypass() {
        assert!(validate_url("http://user@127.0.0.1/").is_err());
        // Credentials on a public host are also rejected (obfuscation surface).
        assert!(validate_url("http://user:pass@example.com/").is_err());
    }

    /// fragment/query terminators: `#`/`?` end the host; reqwest connects to
    /// 127.0.0.1 while the old splitter saw a single non-IP "hostname".
    #[test]
    fn validate_url_blocks_fragment_query_bypass() {
        assert!(validate_url("http://127.0.0.1#@evil.com/").is_err());
        assert!(validate_url("http://127.0.0.1?x/").is_err());
    }

    /// backslash: WHATWG treats `\` as `/` for special schemes, so reqwest
    /// connects to 127.0.0.1 with path `@evil.com/`.
    #[test]
    fn validate_url_blocks_backslash_bypass() {
        assert!(validate_url("http://127.0.0.1\\@evil.com/").is_err());
    }

    /// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) routes to the v4 address; the old
    /// v6 arm only checked loopback/ULA/link-local v6 ranges and passed.
    #[test]
    fn validate_url_blocks_ipv4_mapped_ipv6() {
        assert!(validate_url("http://[::ffff:127.0.0.1]/").is_err());
        assert!(validate_url("http://[::ffff:169.254.169.254]/").is_err());
        // Public mapped addresses stay allowed.
        assert!(validate_url("http://[::ffff:8.8.8.8]/").is_ok());
        // Deprecated IPv4-compatible (`::a.b.c.d`) literals route to the same
        // v4 address; `is_private_ip` catches them via `to_ipv4` (issue #169).
        assert!(validate_url("http://[::127.0.0.1]/").is_err());
        assert!(validate_url("http://[::169.254.169.254]/").is_err());
    }

    /// Trailing-dot literals resolve to the same address; `localhost.` must
    /// also be blocked (dot-stripped before the hostname check).
    #[test]
    fn validate_url_blocks_trailing_dot_literals() {
        assert!(validate_url("http://127.0.0.1./").is_err());
        assert!(validate_url("http://2130706433./").is_err());
        assert!(validate_url("http://localhost./").is_err());
    }

    /// Port variant still blocked (host check independent of port).
    #[test]
    fn validate_url_blocks_loopback_with_port() {
        assert!(validate_url("http://127.0.0.1:8080/path").is_err());
    }

    /// Non-SSRF URLs keep passing: public hostnames, public v6, query/fragment.
    #[test]
    fn validate_url_still_accepts_public_targets() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("http://example.com:8080/path").is_ok());
        assert!(validate_url("http://[2001:db8::1]/path").is_ok());
        assert!(validate_url("https://example.com/x?y=1#z").is_ok());
        // Trailing-dot public hostname stays allowed.
        assert!(validate_url("http://example.com./").is_ok());
    }

    /// Malformed URLs are rejected up front (the old splitter passed some).
    #[test]
    fn validate_url_rejects_unparseable() {
        assert!(validate_url("http://").is_err());
        assert!(validate_url("http://exa mple.com/").is_err());
    }

    // ── Issue #169: missing reserved ranges ─────────────────────────────

    /// CGNAT (`100.64.0.0/10`) and "this network" (`0.0.0.0/8`, RFC 1122)
    /// are not globally routable: an outbound request that lands there is
    /// reaching carrier-grade NAT / a local service, not the internet.
    #[test]
    fn is_private_ip_covers_cgnat_and_this_network() {
        assert!(is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_private_ip("100.127.255.254".parse().unwrap()));
        assert!(is_private_ip("100.64.0.0".parse().unwrap()));
        assert!(is_private_ip("0.1.2.3".parse().unwrap()));
        assert!(is_private_ip("0.255.255.255".parse().unwrap()));
        // Boundaries: 100.63/100.128 are outside the CGNAT block, and
        // 1.0.0.1 is outside 0.0.0.0/8.
        assert!(!is_private_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_private_ip("100.128.0.0".parse().unwrap()));
        assert!(!is_private_ip("1.0.0.1".parse().unwrap()));
    }

    /// IPv4-compatible IPv6 (`::a.b.c.d`) routes to the embedded v4 address
    /// just like the mapped form; NAT64 (`64:ff9b::/96`) hands the embedded
    /// v4 address to a translator.
    #[test]
    fn is_private_ip_covers_ipv4_compatible_and_nat64() {
        // Compatible (`::a.b.c.d`) — the form the old `to_ipv4_mapped`-only
        // check missed.
        assert!(is_private_ip("::127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::10.0.0.1".parse().unwrap()));
        // Mapped (`::ffff:a.b.c.d`) keeps working.
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_private_ip("64:ff9b::7f00:1".parse().unwrap()));
        assert!(is_private_ip("64:ff9b::a00:1".parse().unwrap()));
        // Public embedded addresses stay routable (DNS64/NAT64 deployments
        // must keep working).
        assert!(!is_private_ip("64:ff9b::808:808".parse().unwrap()));
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
        // Ordinary global v6 is untouched.
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    /// The literal-IP layer (`validate_url`) sees the same ranges so a bare
    /// `http://100.64.0.1/` never reaches the socket.
    #[test]
    fn validate_url_blocks_cgnat_and_this_network() {
        for url in [
            "http://100.64.0.1/",
            "http://100.127.255.254:8080/admin",
            "http://0.1.2.3/",
        ] {
            assert!(validate_url(url).is_err(), "expected {url} to be rejected");
        }
        // Public neighbours still pass.
        assert!(validate_url("http://100.63.0.1/").is_ok());
    }

    // ── Issue #169: connection-time resolver guard ──────────────────────

    fn sa(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 0)
    }

    /// The spec's `resolver_rejects_private_resolution`: every address a DNS
    /// answer could hand back for an internal target is refused.
    #[test]
    fn resolver_rejects_private_resolution() {
        for ip in [
            "169.254.169.254",  // cloud metadata / link-local
            "100.64.0.1",       // CGNAT
            "0.1.2.3",          // 0.0.0.0/8
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "64:ff9b::7f00:1",  // NAT64-embedded loopback
            "127.0.0.1",
            "10.0.0.1",
        ] {
            let err = match guard_addrs(vec![sa(ip)], false) {
                Ok(addrs) => panic!("{ip} must be refused, got {addrs:?}"),
                Err(e) => e,
            };
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(
                err.to_string()
                    .contains("refusing to connect to private address"),
                "{err}"
            );
        }
    }

    /// The spec's `resolver_allows_public`.
    #[test]
    fn resolver_allows_public() {
        let addrs = vec![sa("93.184.216.34"), sa("2606:4700:4700::1111")];
        assert_eq!(guard_addrs(addrs.clone(), false).unwrap(), addrs);
    }

    /// A mixed answer is refused as a whole: keeping the public address would
    /// let the connection fall through to the private one.
    #[test]
    fn resolver_rejects_mixed_answer() {
        assert!(guard_addrs(vec![sa("93.184.216.34"), sa("127.0.0.1")], false).is_err());
    }

    /// The opt-out flag (parsed from the env var) disables the check.
    #[test]
    fn resolver_opt_out_allows_private() {
        assert_eq!(guard_addrs(vec![sa("127.0.0.1")], true).unwrap().len(), 1);
        assert_eq!(ALLOW_PRIVATE_DNS_ENV, "RECURSIVE_SSRF_ALLOW_PRIVATE_DNS");
        assert!(parse_opt_out(Some("1")));
        assert!(parse_opt_out(Some(" TRUE ")));
        assert!(parse_opt_out(Some("yes")));
        assert!(parse_opt_out(Some("on")));
        assert!(!parse_opt_out(Some("0")));
        assert!(!parse_opt_out(Some("")));
        assert!(!parse_opt_out(None));
    }

    /// The `Resolve` impl end to end: a stub source's private answer surfaces
    /// as the guard's `PermissionDenied` (which reqwest reports as a connect
    /// error), a public answer passes through unchanged.
    #[tokio::test]
    async fn guarded_resolver_vets_stub_answers() {
        use reqwest::dns::Resolve;

        struct Stub(Vec<SocketAddr>);
        impl AddrSource for Stub {
            fn lookup(&self, _host: &str) -> LookupFuture {
                let addrs = self.0.clone();
                Box::pin(async move { Ok(addrs) })
            }
        }
        fn resolver(addrs: Vec<SocketAddr>, allow_private: bool) -> GuardedResolver {
            GuardedResolver::with_source(Arc::new(Stub(addrs)), allow_private)
        }

        let name: reqwest::dns::Name = "127.0.0.1.nip.io".parse().unwrap();
        // (`Resolving`'s Ok type is a boxed iterator, so no `unwrap_err()`.)
        let err = match resolver(vec![sa("127.0.0.1")], false).resolve(name).await {
            Ok(_) => panic!("a private DNS answer must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string()
                .contains("refusing to connect to private address 127.0.0.1"),
            "{err}"
        );

        let name: reqwest::dns::Name = "example.com".parse().unwrap();
        let addrs = resolver(vec![sa("93.184.216.34")], false)
            .resolve(name)
            .await
            .unwrap();
        assert_eq!(addrs.collect::<Vec<_>>(), vec![sa("93.184.216.34")]);

        // Opt-out path.
        let name: reqwest::dns::Name = "internal.example".parse().unwrap();
        let addrs = resolver(vec![sa("10.0.0.1")], true)
            .resolve(name)
            .await
            .unwrap();
        assert_eq!(addrs.collect::<Vec<_>>(), vec![sa("10.0.0.1")]);
    }

    /// The system source resolves a name to port-0 `SocketAddr`s (reqwest
    /// substitutes the scheme port). `localhost` keeps this offline.
    #[tokio::test]
    async fn system_resolver_looks_up_localhost() {
        let addrs = SystemResolver.lookup("localhost").await.unwrap();
        assert!(!addrs.is_empty(), "localhost must resolve");
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
        assert!(addrs.iter().all(|a| a.port() == 0), "{addrs:?}");
    }

    /// `nat64_rejects`: the RFC 8215 local-use prefix is rejected wholesale,
    /// the RFC 6052 well-known prefix follows its embedded v4 address.
    #[test]
    fn nat64_prefixes_are_classified() {
        let v6 = |s: &str| s.parse::<Ipv6Addr>().unwrap();
        assert!(nat64_rejects(&v6("64:ff9b::7f00:1")));
        assert!(nat64_rejects(&v6("64:ff9b::a00:1")));
        assert!(nat64_rejects(&v6("64:ff9b:1::1")));
        assert!(!nat64_rejects(&v6("64:ff9b::808:808")));
        assert!(!nat64_rejects(&v6("64:ff9b:2::1")));
        assert!(!nat64_rejects(&v6("2001:db8::1")));
    }

    /// `boxed_io_error` preserves the message the guard produced.
    #[test]
    fn boxed_io_error_keeps_message() {
        let err = boxed_io_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refused",
        ));
        assert_eq!(err.to_string(), "refused");
    }

    /// The production constructor is usable as reqwest's trait object.
    #[test]
    fn guarded_resolver_from_env_is_usable() {
        // Compiles ⇒ `Arc<GuardedResolver>` coerces to the `dyn Resolve`
        // that `ClientBuilder::dns_resolver` accepts.
        let resolver: Arc<dyn reqwest::dns::Resolve> = guarded_resolver();
        drop(resolver);
    }

    /// The env gate's parser (`no_proxy_covers`): all separator/case shapes the
    /// pins may meet, plus the near-misses that must stay "not covered".
    #[test]
    fn no_proxy_covers_tolerates_separators_case_and_wildcard() {
        assert!(no_proxy_covers("127.0.0.1,localhost,::1", "localhost"));
        assert!(no_proxy_covers("127.0.0.1,localhost,::1", "127.0.0.1"));
        assert!(no_proxy_covers("EXAMPLE.com localhost", "localhost"));
        assert!(no_proxy_covers("127.0.0.1\tlocalhost", "127.0.0.1"));
        assert!(no_proxy_covers(".localhost", "localhost"));
        assert!(no_proxy_covers("*", "127.0.0.1"));
        assert!(!no_proxy_covers("example.com", "localhost"));
        assert!(!no_proxy_covers("", "localhost"));
        assert!(!no_proxy_covers("notlocalhost", "localhost"));
        assert!(!no_proxy_covers("127.0.0.10", "127.0.0.1"));
    }

    /// The env gate's decision (`loopback_missing_from_no_proxy`): this is what
    /// turns "the pin would be vacuous here" into a loud failure.
    #[test]
    fn loopback_missing_from_no_proxy_is_reported() {
        assert!(loopback_missing_from_no_proxy("127.0.0.1,localhost,::1").is_empty());
        assert!(loopback_missing_from_no_proxy("*").is_empty());
        assert_eq!(
            loopback_missing_from_no_proxy("example.com"),
            vec!["localhost", "127.0.0.1"]
        );
        assert_eq!(
            loopback_missing_from_no_proxy("127.0.0.1"),
            vec!["localhost"]
        );
    }
}

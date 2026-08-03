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

use std::net::{IpAddr, Ipv4Addr};

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
/// tools (loopback, private RFC-1918, link-local, and cloud metadata ranges).
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()           // 127.0.0.0/8
                || v4.is_private()     // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()  // 169.254.0.0/16 (AWS IMDS et al.)
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
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
fn parse_lenient_ipv4(s: &str) -> Option<IpAddr> {
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
}

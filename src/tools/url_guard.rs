//! Shared SSRF guard for outbound-HTTP tools.
//!
//! Extracted from `web_fetch.rs` during Goal 371 so that every tool that makes
//! an outbound HTTP request to a model-supplied URL rejects private/internal/
//! loopback targets **before any socket opens**. Consumers: `WebFetch`,
//! `a2a_call`, `a2a_card`, `a2a_task_check`.
//!
//! The logic here is deliberately verbatim from the original `web_fetch.rs`
//! implementation — do not "improve" it in place; file a separate goal if a
//! gap is found (e.g. the hardcoded `"WebFetch"` name in error messages, or
//! the lack of a redirect policy — both tracked separately).

use std::net::{IpAddr, Ipv4Addr};

use crate::error::{Error, Result};

/// Validate URL and block SSRF targets (private/loopback/link-local addresses).
pub(crate) fn validate_url(url: &str) -> Result<String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::BadToolArgs {
            name: "WebFetch".into(),
            message: "URL must start with http:// or https://".into(),
        });
    }

    // Extract host from URL to check for SSRF targets.
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|host_port| {
            // Strip port if present; handle IPv6 literals [::1]:8080
            if host_port.starts_with('[') {
                host_port.find(']').map(|i| &host_port[1..i])
            } else {
                Some(host_port.split(':').next().unwrap_or(host_port))
            }
        })
        .unwrap_or("");

    // Block well-known SSRF hostnames regardless of capitalisation.
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower == "metadata.google.internal"
    {
        return Err(Error::BadToolArgs {
            name: "WebFetch".into(),
            message: format!("SSRF protection: host '{host}' is not allowed"),
        });
    }

    // If the host parses as a bare IP address, block private/loopback/link-local ranges.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(reject(ip));
        }
    } else if let Some(ip) = parse_lenient_ipv4(host) {
        // Lenient IPv4 spellings (decimal/hex/short-form) that the OS
        // resolver accepts but `IpAddr::parse` rejects — block the same
        // ranges so `http://2130706433/` can't reach loopback or IMDS.
        if is_private_ip(ip) {
            return Err(reject(ip));
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
}

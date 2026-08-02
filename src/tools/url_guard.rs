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

use std::net::IpAddr;

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
            return Err(Error::BadToolArgs {
                name: "WebFetch".into(),
                message: format!("SSRF protection: IP address '{ip}' is not routable"),
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

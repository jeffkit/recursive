# Manual edit: Goal 377 — close url_guard host-parsing SSRF bypasses

**Date**: 2026-08-03
**Goal**: 377 — `src/tools/url_guard.rs::validate_url` was defeated by RFC-vs-string
parsing mismatch (userinfo / `#` / `?` / `\`), IPv4-mapped IPv6, and trailing-dot
literals. Replace the hand-rolled host extractor with `url::Url` (WHATWG parser —
same parsing reqwest applies) and pin every bypass row in tests.

## What changed

### 1. `src/tools/url_guard.rs` — `validate_url` rewritten around `url::Url`

- **Parse-first**: `url::Url::parse(url)`; on `Err` reject with
  `Error::BadToolArgs` `"invalid URL: {e}"`. A cheap `http://`/`https://` prefix
  gate runs *before* parse so non-http inputs keep today's
  `"URL must start with http:// or https://"` message (web_fetch's
  `web_fetch_rejects_invalid_url` test matches on that string), then the
  `parsed.scheme()` check is kept as belt-and-braces.
- **Userinfo rejected**: `parsed.username()` non-empty →
  `"SSRF protection: userinfo in URL is not allowed"`.
- **Host match** on `parsed.host()`:
  - `Host::Ipv4` → `is_private_ip` (unchanged helper).
  - `Host::Ipv6` → **first** `v6.to_ipv4_mapped()` → run the *v4* private check
    (closes `[::ffff:127.0.0.1]`, `[::ffff:169.254.169.254]`); else the existing
    v6 arm checks.
  - `Host::Domain` → lowercase + strip ONE trailing dot, then hostname block
    (localhost/metadata, dot-stripped so `localhost.` is caught), then
    `IpAddr` parse and `parse_lenient_ipv4` fallback with `is_private_ip`.
  - `None` → `"SSRF protection: URL has no host"`.
- `is_private_ip`, `reject`, `parse_lenient_ipv4` stay **byte-identical**; the
  mapped-v4 delegation lives in the `Host::Ipv6` match arm (permitted by the
  goal's parenthetical), so `is_private_ip` semantics for plain v4/v6 are
  unchanged for other callers.

### 2. `Cargo.toml` — `url = "2"` added to `[dependencies]`

The crate only listed `url` in `[dev-dependencies]`; reqwest already pulls in
url 2.x, so this makes the direct dependency explicit with no version bump.

### 3. Tests — every Why-table row pinned by name in `#[cfg(test)] mod tests`

- `validate_url_blocks_userinfo_bypass` (`user@127.0.0.1`, `user:pass@example.com`)
- `validate_url_blocks_fragment_query_bypass` (`127.0.0.1#@evil.com`, `127.0.0.1?x`)
- `validate_url_blocks_backslash_bypass` (`127.0.0.1\@evil.com`)
- `validate_url_blocks_ipv4_mapped_ipv6` (`[::ffff:127.0.0.1]`, `[::ffff:169.254.169.254]`, plus positive `[::ffff:8.8.8.8]` allowed)
- `validate_url_blocks_trailing_dot_literals` (`127.0.0.1.`, `2130706433.`, `localhost.`)
- `validate_url_blocks_loopback_with_port` (`127.0.0.1:8080/path`)
- `validate_url_still_accepts_public_targets` (`example.com`, `example.com:8080/path`, `[2001:db8::1]/path`, `https://example.com/x?y=1#z`, `example.com.` allowed)
- `validate_url_rejects_unparseable` (`http://`, `http://exa mple.com/`)
- Existing `validate_url_blocks_lenient_ssrf_spellings` / `validate_url_still_accepts_hostnames` / `parse_lenient_ipv4_*` / web_fetch / a2a SSRF tests unchanged and green.

## Empirical notes (url 2.5.8)

- The url crate's WHATWG IPv4 parser already handles trailing-dot and lenient
  spellings: `http://127.0.0.1./`, `http://2130706433./`, `http://2130706433/`,
  `http://0x7f000001/`, `http://127.1/`, `http://0177.0.0.1/` all arrive as
  `Host::Ipv4` (not `Host::Domain` as the goal notes assumed for
  `2130706433`). They are therefore blocked by the Ipv4 arm; the
  `parse_lenient_ipv4` Domain fallback remains as defense-in-depth per the goal
  spec and for url-crate versions that differ.
- `http://127.0.0.1\@evil.com/` parses as host `127.0.0.1` + path `@evil.com/`
  (WHATWG `\` ≈ `/`), so it is blocked by the Ipv4 arm.
- `http://localhost./` is the case that genuinely needs the Domain-arm
  trailing-dot strip (`Host::Domain("localhost.")` → `"localhost"` → blocked).

## Files touched

- `src/tools/url_guard.rs` (validate_url rewrite + tests + module header)
- `Cargo.toml` (`url = "2"` in `[dependencies]`)
- `.dev/journal/manual-20260803-goal377-url-guard-host-parse.md` (this file)

## Tests added

8 new tests in `src/tools/url_guard.rs` (listed above); ~30 assertions covering
every bypass row in the goal's Why table plus positive controls.

## Notes

- Signature unchanged: `validate_url(&str) -> Result<String>`; error variant
  unchanged (`Error::BadToolArgs { name: "WebFetch", .. }`); consumers
  (`web_fetch.rs`, `a2a.rs`) untouched.
- No DNS resolution added (TOCTOU/rebinding out of scope, per goal).
- Quality gates run and green in the worktree: `cargo fmt --all`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --workspace` (all pass; `validate_url` headline test: 20/20).

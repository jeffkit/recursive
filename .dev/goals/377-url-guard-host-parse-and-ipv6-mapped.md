# Goal 377 — Close url_guard host-parsing SSRF bypasses (userinfo/#/?/\, IPv6-mapped, trailing-dot)

**Roadmap**: Tools / security — SSRF guard's hand-rolled host extractor is defeated by
RFC-vs-string parsing mismatch; IPv4-mapped IPv6 and trailing-dot literals bypass IP checks

**Design principle check**:
- Implemented as: a pure parse-and-check refactor of `src/tools/url_guard.rs::validate_url`
  using the `url` crate (already in the dependency tree via reqwest — no new deps), plus
  the existing `is_private_ip` / `parse_lenient_ipv4` helpers unchanged. No network calls,
  no DNS resolution, no kernel/run-loop changes.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. `validate_url` keeps its
  exact signature (`fn validate_url(&str) -> Result<String>`) and error variant
  (`Error::BadToolArgs`); only the host-extraction + host-check logic inside it changes.

## Why (the bypasses, all verified 2026-08-03 by reading the code)

`validate_url` extracts the host with a hand-rolled string split
(`src/tools/url_guard.rs:26-38`): `url.split("://").nth(1)` → `split('/').next()` →
`split(':').next()`. It never stops at `@` (userinfo), `#` (fragment), `?` (query), or
`\` (WHATWG path separator). reqwest, however, parses URLs per RFC-3986/WHATWG. The
mismatch lets these reach private/loopback hosts while the guard sees a "hostname":

| URL | guard's extracted host | guard verdict | reqwest actually connects to |
|---|---|---|---|
| `http://user@127.0.0.1/` | `user@127.0.0.1` (not an IP) | **pass** | `127.0.0.1` (userinfo dropped) |
| `http://127.0.0.1#@evil.com/` | `127.0.0.1#@evil.com` | **pass** | `127.0.0.1` (`#` starts fragment) |
| `http://127.0.0.1?x/` | `127.0.0.1?x` | **pass** | `127.0.0.1` (`?` starts query) |
| `http://127.0.0.1\@evil.com/` | `127.0.0.1\@evil.com` | **pass** | `127.0.0.1` (`\` ≈ `/` in WHATWG) |

Two more literal-form gaps in the same file:

- **IPv4-mapped IPv6** (`:76-90` v6 arm): `[::ffff:127.0.0.1]` / `[::ffff:169.254.169.254]`
  parse as `IpAddr::V6` but the v6 arm only checks `is_loopback()` (true only for `::1`),
  `is_unspecified`, `is_multicast`, ULA `fc00::/7`, link-local `fe80::/10` — the mapped
  address passes and the OS routes it to loopback/IMDS.
- **Trailing-dot** (`:76-90` lenient branch): `http://127.0.0.1./` and
  `http://2130706433./` — `parse_lenient_ipv4` sees an empty trailing part → `None` →
  treated as hostname → the OS resolver maps it to loopback. The exact class the lenient
  parser was added for.

This is the same SSRF-hardening line as Goals 370-372/376; the module header
(`:8-12`) says "file a separate goal if a gap is found".

## Scope (do exactly this, no more)

### 1. Replace the hand-rolled host extraction with `url::Url` parsing

In `src/tools/url_guard.rs::validate_url`:

- `let parsed = url::Url::parse(url)` — on `Err`, reject with the same
  `Error::BadToolArgs` shape (message `"invalid URL: {e}"`).
- Scheme check stays: `parsed.scheme()` must be `http` or `https`, else reject (same
  message as today's scheme branch).
- **Reject userinfo**: if `parsed.username()` is non-empty, reject
  (`"SSRF protection: userinfo in URL is not allowed"`). Web tools have no need for
  embedded credentials, and userinfo is a primary obfuscation surface.
- Match on `parsed.host()`:
  - `Some(Host::Ipv4(v4))` → `is_private_ip(IpAddr::V4(v4))` check (existing helper).
  - `Some(Host::Ipv6(v6))` → **first** `if let Some(v4) = v6.to_ipv4_mapped()` →
    run `is_private_ip(IpAddr::V4(v4))`; **else** the existing v6 arm checks.
  - `Some(Host::Domain(d))` → `d.to_ascii_lowercase()`, **strip a single trailing `.`**,
    then: (a) localhost/metadata hostname checks as today (compare against the
    dot-stripped form too — `localhost.` must also be blocked); (b) try
    `d.parse::<IpAddr>()` then `parse_lenient_ipv4(d)` on the dot-stripped domain and run
    any hit through `is_private_ip` (this closes `127.0.0.1.`, `2130706433.`); (c) real
    hostnames (`example.com`) hit neither parser → allowed, unchanged.
  - `None` (no host) → reject (`"SSRF protection: URL has no host"`).
- Keep the existing `reject(ip)` helper and error message format
  (`"SSRF protection: IP address '{ip}' is not routable"`). Keep the `name: "WebFetch"`
  cosmetic quirk (known issue, out of scope).
- `parse_lenient_ipv4` and `is_private_ip` stay byte-identical except the v6 arm may
  delegate to the v4 check via `to_ipv4_mapped()`.

### 2. Tests — extend `#[cfg(test)] mod tests` in `src/tools/url_guard.rs`

Every bypass row from the Why table must be pinned **by name**:

- `validate_url("http://user@127.0.0.1/").is_err()`
- `validate_url("http://127.0.0.1#@evil.com/").is_err()`
- `validate_url("http://127.0.0.1?x/").is_err()`
- `validate_url("http://127.0.0.1\\@evil.com/").is_err()`
- `validate_url("http://[::ffff:127.0.0.1]/").is_err()`
- `validate_url("http://[::ffff:169.254.169.254]/").is_err()`
- `validate_url("http://127.0.0.1./").is_err()`
- `validate_url("http://2130706433./").is_err()`
- `validate_url("http://localhost./").is_err()`
- `validate_url("http://127.0.0.1:8080/path").is_err()` (port variant still blocked)
- Still accepted: `http://example.com`, `http://example.com:8080/path`,
  `http://[2001:db8::1]/path` (public v6), `https://example.com/x?y=1#z`.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — kernel/run-loop invariants.
- `src/tools/web_fetch.rs`, `src/tools/a2a.rs`, `src/tools/mod.rs` — consumers stay as-is
  (`validate_url`'s signature doesn't change). Do NOT add `Policy::none()` here — that's a
  separate goal (378).
- `is_private_ip` semantics for plain v4/v6 (except the delegated v4-mapped arm), the
  scheme check shape, `parse_lenient_ipv4` internals.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Headline test by name: `cargo test --manifest-path Cargo.toml validate_url` — all
  bypass rows green.
- Grep: `rg "to_ipv4_mapped|Url::parse" src/tools/url_guard.rs` ≥ 2 hits (impl + tests).

## Notes for the agent (traps)

- **`url` crate is already a dependency** (reqwest pulls it in) — add it to
  `Cargo.toml` only if the workspace crate doesn't already list it; prefer `url::Url`
  from wherever reqwest gets it. No version bump needed.
- **`Url::parse` normalizes**: `http://user@127.0.0.1/` parses fine (userinfo in
  `parsed.username()`); `http://127.0.0.1\@evil.com/` yields host `127.0.0.1` with path
  `@evil.com/` (WHATWG). Trust `parsed.host()`, not the raw string.
- **`Host::Ipv6` gives you `Ipv6Addr` directly** — no string round-trip. For mapped
  forms use `ipv6.to_ipv4_mapped()` (Rust std, returns `Option<Ipv4Addr>`).
- **Trailing dot**: `Url::parse("http://127.0.0.1./")` yields `Host::Domain("127.0.0.1.")`
  — that is why the Domain branch must strip ONE trailing dot before the IP/lenient
  checks. `example.com.` must stay allowed (strip → `example.com` → not an IP → pass).
- **Keep the lenient parser reachable**: `http://2130706433/` parses as
  `Host::Domain("2130706433")` (url crate does not treat it as an IP literal) — so the
  Domain branch's `parse_lenient_ipv4` fallback is what still blocks Goal-376 spellings.
  Do not regress the existing `validate_url_blocks_lenient_ssrf_spellings` test.
- **Do not add DNS resolution** — resolving hostnames to check their A/AAAA records is
  explicitly out of scope for this goal (TOCTOU/rebinding is a separate decision).
- **Error shape**: keep `Error::BadToolArgs { name: "WebFetch" }` and the
  `"SSRF protection: ..."` message prefix so web_fetch's existing error-match tests stay
  green. Only the message body may change for the new reject cases.
- **cargo-fmt + clippy are enforced gates** — run `cargo fmt --all` and clippy before
  finishing. Do not leave `#[allow]`s behind.
- **Journal**: write `.dev/journal/manual-<date>-goal377-url-guard-host-parse.md`
  (Date / Goal / Files touched / Tests added / Notes) per repo convention.

# Manual edit: goal-376 — Close lenient-IP SSRF bypass in url_guard

**Date**: 2026-08-03
**Goal**: Goal 376 — Close lenient-IP SSRF bypass in url_guard (decimal/hex/short-form IPv4)

## What changed

All changes confined to `src/tools/url_guard.rs` (the shared SSRF guard). No
kernel / run-loop / tool-consumer / e2e changes; `validate_url` keeps its exact
signature and return shape.

### 1. `parse_lenient_ipv4(s: &str) -> Option<IpAddr>` (new, private)

Hand-rolled POSIX `inet_aton`-semantics parser for spellings the OS resolver
accepts but `std::net::IpAddr::parse` rejects:
- Splits on `.` into 1–4 parts; empty/over-4 parts → `None`.
- Per-part radix via `parse_lenient_part`: `0x`/`0X` → hex; leading `0` with
  length > 1 → octal; else decimal. `"0"` is decimal zero, `"00"` is octal zero
  (both → `0.0.0.0`).
- Packing: non-last parts ≤ 255 (8 bits); last part may span remaining bits
  (1 part → 32, 2 parts → 24, 3 parts → 16, 4 parts → 8). Overflow → `None`.
- Packs via `Ipv4Addr::from(u32)` (big-endian). Pure string parsing — no DNS,
  no sockets, no new deps.

**Deliberate POSIX-not-macOS semantics**: leading `0` means octal (`0177.0.0.1`
→ `127.0.0.1`), even though macOS `getaddrinfo` would read `0177` as decimal
177. Pinned in unit test so glibc/Linux (prod + e2e container) is the covered
resolver.

### 2. `validate_url` lenient fallback

At the canonical `host.parse::<IpAddr>()` branch: when the canonical parse
fails, `parse_lenient_ipv4(host)` feeds the same `is_private_ip` check. Error
shape is byte-identical to the canonical branch — shared `fn reject(ip: IpAddr)
-> Error` helper keeps variant (`Error::BadToolArgs`), `name` (`"WebFetch"`),
and message format (`"SSRF protection: IP address '{ip}' is not routable"`)
in one place. Non-IP hostnames (`example.com`, `example.com:8080`) still pass —
lenient parse returns `None` and the guard proceeds as before.

Did NOT touch: `is_private_ip` internals, the localhost/metadata hostname
branch, the scheme check, the hardcoded `"WebFetch"` name (tracked separately),
or any consumer (`web_fetch.rs`, `a2a.rs`, `tools/mod.rs`).

### 3. Tests (new `#[cfg(test)] mod tests` in `url_guard.rs` — it had none)

- `parse_lenient_ipv4_*` unit tests: decimal single-part (`2130706433` →
  127.0.0.1, `2852039166` → 169.254.169.254, `3232235521` → 192.168.0.1,
  `0` → 0.0.0.0, `4294967295` → 255.255.255.255), hex (`0x7f000001`,
  `0xa9fea9fe`, uppercase `0X`), short-form (`127.1`, `127.0.1`), octal
  (`0177.0.0.1` → 127.0.0.1, `00` → 0.0.0.0), malformed (`""`, `256.1.1.1`,
  `1.2.65536`, `0x`, `1.2.3.4.5`, `abc`, `127.0.0.999` → `None`).
- `validate_url_blocks_lenient_ssrf_spellings`: every row of the Why table
  rejected end-to-end, incl. `http://2852039166/latest/meta-data/` (AWS IMDS).
- `validate_url_still_accepts_hostnames`: `http://example.com` and
  `http://example.com:8080/path` still `Ok`.

## Files touched

- `src/tools/url_guard.rs` — import (`Ipv4Addr`), lenient fallback in
  `validate_url`, new `reject` / `parse_lenient_ipv4` / `parse_lenient_part`
  helpers, new `#[cfg(test)] mod tests`.

## Tests added

7 new tests in `src/tools/url_guard.rs` (listed above).

## Verification

- `cargo test --manifest-path Cargo.toml parse_lenient_ipv4` → 5/5 green (headline gate).
- `cargo test --manifest-path Cargo.toml url_guard` → 7/7 green.
- `cargo test --workspace` → green (2218 lib + all integration suites; 0 failed).
- `cargo build --workspace` → green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all -- --check` → clean.
- Grep acceptance: `rg -c "parse_lenient_ipv4" src/tools/url_guard.rs` → 26
  (≥3 required); `rg "2130706433|2852039166|0x7f000001|127\.1"
  src/tools/url_guard.rs` → hits inside `#[cfg(test)]` (bypass spellings pinned
  by name). The pre-existing 13 dead-code warnings in `src/team.rs`
  (default-feature build, from goal-373's `pub(crate) mod team` flip) are
  unrelated to this change.

## Notes

- **Security-edge decisions** (kept deliberately conservative):
  - Leading `+`/`-` are rejected at the first-byte digit check, so `+127` /
    `-1` → `None` (allowed as hostname, matching inet_aton which refuses them
    as numeric and fails DNS resolution anyway — no new hole).
  - Strict octal: a part like `"08"` (leading 0, invalid octal digit 8) →
    `None`, not silently coerced. glibc's `inet_aton` would read `08` octal-
    leniently as 8 → `0.0.0.8`, which `is_private_ip` does NOT block anyway
    (not loopback/private/link-local), so returning `None` for `"08"` creates
    no private-network bypass. Not in the goal's required case list; noted here
    for the record.
  - Trailing-dot `example.com.` → 5 parts / empty part → `None` → allowed, same
    as before (documented in goal as intended).
- **No new dependencies** (invariant #6): hand-rolled parser, ~40 lines, std
  only (`u32::from_str_radix`, `Ipv4Addr::from`).
- **Invariant #5**: no `unwrap()`/`expect()` in the new non-test code — the
  parser returns `Option` throughout.

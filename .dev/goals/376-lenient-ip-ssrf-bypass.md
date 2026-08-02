# Goal 376 — Close lenient-IP SSRF bypass in url_guard (decimal/hex/short-form IPv4)

**Roadmap**: Tools / security — SSRF guard lets lenient-IP spellings reach private hosts

**Design principle check**:
- Implemented as: a pure string-level IPv4 parse helper (`parse_lenient_ipv4`) in
  `src/tools/url_guard.rs` + a branch in `validate_url` that feeds it through the existing
  `is_private_ip` check. No new deps, no network calls, no DNS resolution.
- ❌ Does NOT touch the agent kernel, run loop, or any invariant. `validate_url` keeps its
  exact signature and return shape; only its private decision logic grows.
- No new deps (hand-rolled parser, ~40 lines).

## Why (the bypass, with live evidence)

`src/tools/url_guard.rs:validate_url` blocks SSRF targets by parsing `host` with
`std::net::IpAddr` (`:54-61`) and rejecting private/loopback/link-local addresses via
`is_private_ip` (`:68-83`). **`IpAddr::parse` only accepts canonical dotted-quad IPv4 and
RFC-4291 IPv6** — but the OS resolver (`getaddrinfo`/`inet_aton` semantics) accepts
several *lenient* spellings that silently resolve to the same private addresses:

| URL host | resolves to (verified via `socket.getaddrinfo`, 2026-08-03) | `IpAddr::parse` |
|---|---|---|
| `2130706433` | `127.0.0.1` (loopback) | FAILS → treated as hostname → **bypass** |
| `0x7f000001` | `127.0.0.1` (loopback) | FAILS → **bypass** |
| `127.1` | `127.0.0.1` (loopback) | FAILS → **bypass** |
| `127.0.1` | `127.0.0.1` (loopback) | FAILS → **bypass** |
| `2852039166` / `0xa9fea9fe` | `169.254.169.254` (**AWS IMDS**) | FAILS → **bypass** |
| `3232235521` | `192.168.0.1` (RFC-1918) | FAILS → **bypass** |
| `0` | `0.0.0.0` (unspecified) | FAILS → **bypass** |
| `4294967295` | `255.255.255.255` (broadcast) | FAILS → **bypass** |

So today `http://2130706433/` and `http://0x7f000001/` sail past the guard and hit
`127.0.0.1`, and `http://2852039166/latest/meta-data/` hits the AWS IMDS endpoint that
`http://169.254.169.254/latest/meta-data/` is explicitly blocked from (`web_fetch.rs:378`).
The guard's existing tests (`web_fetch.rs:369-382`, `validate_url_blocks_ssrf_targets`)
only cover canonical spellings.

This is the same SSRF-hardening line as Goals 370-372; `url_guard.rs:8-12` explicitly
says "file a separate goal if a gap is found".

## Scope (do exactly this, no more)

### 1. Add `fn parse_lenient_ipv4(s: &str) -> Option<IpAddr>` in `src/tools/url_guard.rs`

Hand-rolled POSIX `inet_aton`-semantics parser (no `std` help exists for this). Rules:

- Split `s` on `.` into 1-4 parts.
- Each part is a number in one of three radices: `0x`/`0X` prefix → hex; leading `0` (and
  length > 1) → octal; otherwise decimal. Empty parts → `None`.
- **glibc semantics, NOT macOS `getaddrinfo`**: leading `0` means octal (`0177` = 127).
  macOS's resolver skips octal interpretation (its `0177.0.0.1` = `177.0.0.1`) — implement
  POSIX semantics regardless, so Linux (prod + e2e container) is closed.
- Part-value packing (POSIX): the *last* part may hold more than 8 bits —
  1 part → up to 32 bits; 2 parts → second up to 24 bits; 3 parts → third up to 16 bits;
  4 parts → each ≤ 255. Earlier parts must be ≤ 255 in every form.
- Pack into `Ipv4Addr::from(u32)` (big-endian). Handle overflow (value > 2^32-1 → `None`).
- Return `Some(IpAddr::V4(..))` on success, `None` on any malformed input.

### 2. Wire it into `validate_url`

At `src/tools/url_guard.rs:54-61`, where the canonical `host.parse::<IpAddr>()` branch
lives: when the canonical parse **fails**, fall back to
`parse_lenient_ipv4(host)` and run the result through the same `is_private_ip` check,
returning the same `Error::BadToolArgs { name: "WebFetch".into(), .. }` error shape with
the message format used by the existing branch (`"SSRF protection: IP address '{ip}' is
not routable"` — reuse the *parsed* IP in the message). Non-IP hostnames (e.g.
`example.com`) must continue to pass unchanged — `parse_lenient_ipv4` returns `None` and
the guard proceeds as today.

Do NOT touch `is_private_ip`, the localhost/metadata hostname branch (`:46-52`), or the
scheme check. The hardcoded `name: "WebFetch"` in error messages is a known cosmetic
issue tracked separately (per the module's own comment) — leave it.

### 3. Add a test module to `src/tools/url_guard.rs` (it currently has none)

`#[cfg(test)] mod tests` in the same file, covering:

- `parse_lenient_ipv4` unit cases: `"2130706433"` → 127.0.0.1; `"0x7f000001"` → 127.0.0.1;
  `"127.1"` → 127.0.0.1; `"127.0.1"` → 127.0.0.1; `"0177.0.0.1"` → 127.0.0.1 (octal!);
  `"2852039166"` → 169.254.169.254; `"0xa9fea9fe"` → 169.254.169.254; `"3232235521"` →
  192.168.0.1; `"0"` → 0.0.0.0; `"4294967295"` → 255.255.255.255; malformed: `""`,
  `"256.1.1.1"` (4-part >255), `"1.2.3"` (3-part third > 0xFFFF: `"1.2.65536"`), `"0x"`,
  `"1.2.3.4.5"`, `"abc"`, `"127.0.0.999"` → `None`.
- `validate_url` end-to-end rejections (mirror the style of
  `web_fetch.rs::validate_url_blocks_ssrf_targets`): every row in the Why table → `is_err()`,
  including `http://2852039166/latest/meta-data/`.
- `validate_url` still accepts `http://example.com`, `http://example.com:8080/path` →
  `is_ok()`.

### 4. (Only if the compiler complains) extend the `use` list in `url_guard.rs`

The file currently imports `std::net::IpAddr`; the new code needs `Ipv4Addr` — add it to
the existing import. Nothing else.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — kernel/run-loop invariants.
- `src/tools/web_fetch.rs`, `src/tools/a2a.rs`, `src/tools/mod.rs` — consumers stay as-is;
  `validate_url`'s signature doesn't change, so nothing else needs editing. Do NOT "fix"
  the `name: "WebFetch"` cosmetic issue.
- `is_private_ip` internals, the hostname-based localhost/metadata branch, the scheme
  check in `validate_url`.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Grep: `rg "parse_lenient_ipv4" src/tools/url_guard.rs` returns ≥3 hits (fn def + the
  validate_url call + a test).
- Grep: `rg "2130706433|2852039166|0x7f000001|127\.1" src/tools/url_guard.rs` returns hits
  inside `#[cfg(test)]` (the bypass spellings are pinned by name, not just shape).
- Headline test passes by name:
  `cargo test --manifest-path Cargo.toml parse_lenient_ipv4` — all unit cases green.

## Notes for the agent (traps)

- **This goal exists because a security gate had a hole — the fix must not introduce
  another one.** Radix parsing order matters: try `0x`/`0X` hex first, then leading-`0`
  octal, then decimal. `"0"` is decimal zero (single digit, not octal), and `"00"` is
  octal zero — both must give `0.0.0.0` (which `is_private_ip` rejects as unspecified).
- **POSIX octal semantics, not macOS behavior.** Do NOT "verify" octal against your local
  resolver — macOS `getaddrinfo("0177.0.0.1")` returns `177.0.0.1` (it ignores octal), but
  glibc returns `127.0.0.1`. The guard must use POSIX rules so Linux (the e2e container and
  prod) is covered. Pin `0177.0.0.1` → `127.0.0.1` in the unit test regardless.
- **No DNS, no sockets.** `parse_lenient_ipv4` is pure string parsing. Do not resolve
  hostnames; do not use `ToSocketAddrs`; do not add any crate. If the host has a trailing
  dot (`example.com.`), it's a hostname — lenient parse fails → allowed, same as today.
- **The last part is special.** `"1.2.3"` with third part `65536` is invalid (3-part form
  caps the last at 16 bits), but `"127.1"` (24-bit last) and `"2130706433"` (32-bit single)
  are valid. Re-read the packing rules in Scope §1 before writing tests.
- **Keep the error shape byte-identical to the canonical branch.** Same variant
  (`Error::BadToolArgs`), same `name`, same message format string — only the IP in the
  message text comes from the lenient parse. Reuse a small shared `fn reject(host: &str,
  ip: IpAddr) -> Error` if it reads cleaner; the goal is behavior, not structure.
- **`validate_url` must still accept real hostnames with ports.** `http://example.com:8080`
  → host extraction gives `example.com` → lenient parse `None` → allowed.
- **cargo-fmt + clippy are enforced gates** — run `cargo fmt --all` and clippy before
  finishing. Do not leave `#[allow]`s behind.
- **Journal**: write `.dev/journal/manual-<date>-goal376-url-guard-lenient-ip.md` (Date /
  Goal / Files touched / Tests added / Notes) per repo convention.

---
type: Architecture
title: Web Tools — web_fetch, web_search
description: HTTP fetch and web search tools. Feature-gated by web_fetch and web_search Cargo features. Not available in sandboxed runs.
tags: [tools, web, feature-gated]
timestamp: 2026-07-09T15:00:00Z
---

# Web Tools

Both tools are **feature-gated** and not available in all builds.

## web_fetch (`WebFetch`)

- **Source**: `src/tools/web_fetch.rs`
- **Feature flag**: `web_fetch`
- **Args**: `url`
- **Returns**: Text content of the page (HTML stripped to readable text)

### SSRF guard (web_fetch, and the A2A tools)

Outbound requests are checked in two layers (issue #169):

1. **Literal URL** — `url_guard::validate_url` rejects loopback / private /
   link-local / cloud-metadata hosts before a request is built, including
   `user:pass@` userinfo tricks, trailing-dot FQDNs and lenient
   `2130706433`-style IPv4 spellings. Redirects are never followed.
2. **Resolved address** — the HTTP client installs `url_guard::guarded_resolver`,
   a `reqwest::dns::Resolve` that refuses the whole DNS answer as soon as *any*
   address is private or reserved (`10/8`, `172.16/12`, `192.168/16`, `127/8`,
   `169.254/16`, CGNAT `100.64/10`, `0.0.0.0/8`, IPv4-mapped/compatible IPv6,
   NAT64, ULA, link-local). This is the layer that stops
   `http://127.0.0.1.nip.io/`, `http://169.254.169.254.nip.io/latest/meta-data/`
   and DNS rebinding from reaching internal services.

A refusal is logged at `warn` (`SSRF guard refused a DNS answer`); the tool error
itself only says `request failed: error sending request`, because reqwest does
not surface the cause.

**Opt-out (default off — the guard is on).** Set
`RECURSIVE_SSRF_ALLOW_PRIVATE_DNS=1` (`true` / `yes` / `on`, case-insensitive)
in the agent process environment to let names resolve into private space. Use it
only when the deployment legitimately reaches internal hosts (an internal
gateway, a sidecar, a corporate resolver): it disables layer 2 for *every*
outbound web and A2A request in that process.

**Known limitation (proxy).** With `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`
set, reqwest hands the request to the proxy and the **proxy** resolves the target
name, so layer 2 never runs for that request — an internal target can be reached
through the proxy. This predates the guard (there was none) and is tracked as a
follow-up: reject a non-loopback proxy by default, or pre-resolve and vet the
target name in proxy mode. `NO_PROXY` entries keep the affected hosts on the
direct path.

## web_search (`WebSearch`)

- **Source**: `src/tools/web_search.rs`
- **Feature flag**: `web_search`
- **Args**: `query`, optional `num_results`
- **Returns**: Ranked results with title, url, snippet

### Providers

When `RECURSIVE_WEB_SEARCH_PROVIDER` + `RECURSIVE_WEB_SEARCH_API_KEY` (or
`~/.recursive/config.toml` `[search]`) are set, results come from the chosen
API backend: `brave` | `tavily` | `serper` | `bocha` | `bing` (Bing Web Search
API, not HTML scrape).

### Zero-config fallback (no API key)

1. **DuckDuckGo HTML** scrape (`html.duckduckgo.com`)
2. **Bing HTML** scrape (`www.bing.com/search`) if DDG is challenged / empty
3. **Jina AI Search** (`s.jina.ai`) Markdown fallback if both scrapes fail

Optional: `RECURSIVE_WEB_SEARCH_JINA_KEY` raises the Jina quota.

## Usage Note

AGENTS.md: "Use sparingly; most goals don't need it." Web tools are unavailable
in the headless self-improve loop when running without the feature flags.

## Related Concepts

- [Tools Overview](index.md)

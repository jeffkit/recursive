# Goal 37 — Web Fetch tool

**Roadmap**: 2.2 — Web Fetch (High / S)

**Design principle check**:
- Implemented as: **new Tool** `src/tools/web_fetch.rs`. Registered
  via the existing `ToolBox` mechanism.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

Agents frequently need to consult docs, GitHub READMEs, Stack
Overflow, etc. Today that requires the user to copy-paste content
into the conversation. Every competitor has a fetch tool. Without
it, our agent is blind to the internet.

## Scope

Touches: new `src/tools/web_fetch.rs`, `src/tools/mod.rs` (re-export),
`src/main.rs` (registration).

### 1. New tool `web_fetch`

Parameters:
- `url: string` (required) — must start with `http://` or `https://`.
- `max_bytes: int` (optional, default 65536) — cap on body read.

Behavior:
- HTTP GET via `reqwest` (already a dep).
- Headers: `User-Agent: recursive-agent/<version>`.
- `.timeout(Duration::from_secs(15)).connect_timeout(Duration::from_secs(5))`
  — MANDATORY per AGENTS.md section 5.
- Read up to `max_bytes`; if body exceeds, truncate and append
  `\n\n[…truncated at <max_bytes> bytes; total body was N bytes]`.
- If `Content-Type: text/html`, do a best-effort markdown conversion
  (strip script/style, collapse whitespace, preserve links as
  `[text](url)`). The `html2text` crate has done this for years —
  add it as a dep if needed (small, well-vetted). Or do the minimal
  manual stripping in ≤80 LOC; preference for no new deps unless
  necessary.
- Return: a string with the (possibly markdown-converted) body.
- Errors: invalid URL → error string. Non-2xx → error string with
  status code and short body excerpt. Network timeout → error string.

### 2. Tests in `src/tools/web_fetch.rs`

- **Test A**: mock TCP server returns a small `text/plain` response;
  `web_fetch` returns the body. Use explicit `.timeout()` +
  `.connect_timeout()` on the test client too.
- **Test B**: mock server returns 404 → tool returns an error
  string containing "404".
- **Test C**: body exceeds `max_bytes` → tool returns truncated
  body with the truncation marker.

(If you add `html2text`, add a 4th test for HTML → markdown.)

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- Reuse the mock TCP server pattern from g32 (streaming-sse) tests
  if it lands first, or model after `src/llm/openai.rs` tests.
- The HTML → markdown conversion is the trickiest part. If a clean
  ≤80 LOC manual implementation works (regex-based tag stripping +
  link preservation), prefer that over a new dep. If it gets ugly,
  add `html2text` (no async, tiny, MIT-licensed).
- Do NOT follow redirects manually — `reqwest` does by default
  (max 10), which is fine. Don't disable it.
- URL validation: just check the prefix. Don't parse with `url`
  crate (not yet a dep, not worth pulling in for this).
- Use `apply_patch`. `.to_string()` over `.into()` in tests.

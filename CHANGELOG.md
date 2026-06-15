# Changelog

## 0.2.0 (unreleased)

- **BREAKING (security)**: HTTP server now refuses requests with 503 when
  no auth is configured (SEC-003 / Goal 277). Operators must set
  `RECURSIVE_HTTP_AUTH_KEYS` or `RECURSIVE_HTTP_AUTH_JWT_SECRET`.
  For local dev only, `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` restores the
  old pass-through behavior. Do NOT use this escape hatch in production.
- **BREAKING (security)**: `run_skill_script` no longer wraps script
  execution in `sh -c`. Args are parsed with `shell-words` and passed as
  discrete argv elements to a direct `exec` of the script. Shell injection
  via args is no longer possible. Skills that relied on `sh -c` globbing
  (e.g. `args: "*"`) will now see literal `*` — update scripts to expand
  globs internally (e.g. with `for f in "$@"; do ...; done`). Goal 283.
- Skill system v2 (refs, scripts, params, injection modes, composition)
- MCP HTTP+SSE transport
- MCP resources and prompts support
- Feature flags (mcp, web_fetch, anthropic)
- Structured error types
- 5 runnable examples
- 367+ tests

## 0.1.0 (initial release)

- Minimal ReAct agent loop
- OpenAI-compatible LLM provider
- Filesystem tools (read, write, list, patch)
- Shell tool with sandboxing and timeout
- Mock provider for offline testing
- CLI: run, repl, tools commands
- Hook system for lifecycle observation
- Transcript compaction
- MCP stdio transport
- Skill system v1

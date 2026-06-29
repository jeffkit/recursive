# Changelog

## 0.7.0

The "workspace split" release — 805 commits since 0.6.0. Highlights:

### Breaking
- **Workspace restructure**: TUI and CLI physically migrated into separate
  workspace crates (`recursive-cli`, `recursive-tui`, `recursive-agent`).
  The published `recursive` binary now lives in `recursive-cli`; the root
  crate is the library. Embedders depending on the old in-tree layout
  must update path deps.
- **Deleted deprecated types**: `Agent`, `StepEvent`, `AgentOutcome`
  removed (use `RunCore` / `AgentEvent`).
- **HTTP server security**: refuses 503 when no auth is configured
  (`RECURSIVE_HTTP_AUTH_KEYS` / `RECURSIVE_HTTP_AUTH_JWT_SECRET`).
  `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` for local dev only.
- **`run_skill_script`** no longer wraps in `sh -c`; args are parsed with
  `shell-words` and exec'd directly (no shell injection).

### Providers & pricing
- Remote provider catalog with 7-day TTL cache
  (`recursive providers update|list|status`, `RECURSIVE_PROVIDERS_URL`,
  `RECURSIVE_PROVIDERS_AUTO_REFRESH`). `pricing_for` now resolves from
  the effective catalog (remote cache > bundled > `providers.d`).
- Dual-protocol `anthropic_api_base` in presets (OpenAI + Anthropic on
  one provider).

### LLM
- Anthropic `stream_with_search`: multi-round tool search across
  streaming calls.
- OpenAI provider software-layer ToolSearch fallback.
- Live reasoning streaming; reasoning tokens counted in cost total.

### Tools & skills
- `WebSearch` tool with multi-provider support + Jina zero-config fallback.
- `Glob`; tool names aligned with fake-cc conventions.
- Skill-hub: `find_skills` / `install_skill` tools.
- Partial-read guard for `StrReplace` (goal 261).

### HTTP API & sessions
- `recursive http` subcommand with graceful shutdown.
- Route-level auth bypass; HTTP session TTL reaper +
  `Config.subagent_max_depth`.
- Type-safe `SessionStatus` enum; `schema_version` on `SessionMeta`;
  auto-fill session `name` from first prompt.
- Native session-id resume (`recursive resume <id>` / `--from-file`)
  replaces transcript-replay resume.

### Multi-agent
- Coordinator mode + team/task tools; inter-worker messaging; parallel
  dispatch; `role_name` in `spawn_worker`.

### TUI
- Bottom-panel API + `CommandInteract` mode (in-layout slot replaces
  overlay popups); per-turn cache-hit rate; Claude-Code-style startup
  banner.

### Self-improve loop
- `--reviewer-agent` (claude support); `--allow-tools` flag; multi-round
  revision loop; reviewer with Read/Glob access.

### Internals
- Architecture review fixes (P0–P3); `session.rs` / `tools/mod.rs` split;
  unified `atomic_write`; configurable stuck-detection window/threshold.

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

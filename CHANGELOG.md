# Changelog

## Unreleased

- feat(http): wire the S3 transcript backend into `recursive http` (#92). With
  the `cloud-runtime` feature compiled in, `RECURSIVE_S3_BUCKET` now selects
  `S3StorageBackend` for transcripts, memory and per-session metadata instead of
  logging "recognized but not yet wired" and silently staying on local disk, so
  `GET /sessions/:id` cold-loads a session torn down on a sibling replica that
  shares the bucket. Selection is a pure, unit-tested function
  (`storage::select_http_storage`); the decision is made once at startup via
  `storage::http_storage_backend`. The bundled Dockerfile gained a `FEATURES`
  build arg (default `http`), and `docker-compose.yml` builds with
  `http,cloud-runtime`, closing the gap where the "full cloud stack" compose
  built a binary that could not use the configured bucket. Persistence timing is
  unchanged (teardown only: DELETE / idle eviction / graceful shutdown).
  Redis session hot-state remains **not** consumed by `recursive http`:
  `RedisSessionStore` is available through the library API, but the kernel does
  not checkpoint per turn, so injecting it would be a no-op — the server logs a
  precise note instead, and the cloud docs now state the sticky-session
  requirement for multi-replica deployments.

- feat(cost): four-dimensional mid-run budgets (#94). `max_budget_usd` and
  `thinking_budget` were stored-but-dead fields: nothing in the agent loop read
  either, and the HTTP schema still advertised them ("Agent stops after any turn
  that would exceed this limit") to integrators. `RunCore` now carries a
  `CostBudget` guard that compares the turn's accumulated spend after every
  completed step and ends the turn with `FinishReason::BudgetExceeded` (data, not
  an error — transcript kept) before the next LLM call is issued; models with no
  pricing entry degrade to a token ceiling at a pessimistic $10/M blend, so an
  unpriced gateway model still cannot run away. `thinking_budget` now reaches the
  wire as Anthropic `thinking = {type:"enabled",budget_tokens:n}` (temperature
  dropped and `max_tokens` raised above the budget, as the Messages API requires),
  on both the streaming and non-streaming paths. `max_steps` / `wall_timeout_secs`
  stop defaulting to `0` = silently unlimited: they now default to 200 steps /
  3600 s per turn (both still revertible with an explicit `0` — the escape hatch
  long-running `recursive loop` / batch flows rely on). New env knobs
  `RECURSIVE_MAX_BUDGET_USD` (`--max-budget-usd` now reads it too) and
  `RECURSIVE_THINKING_BUDGET`. HTTP `POST /run` and `POST /sessions` accept
  per-request `max_budget_usd` / `thinking_budget`; a per-session thinking budget
  builds a provider for that request (provider construction moved into
  `recursive::llm::build_llm_provider`), and both survive a cold load
  (`SessionMeta.overrides`). Semantics are per turn, not per session — the request
  and flag docs now say so.

- fix(http): rate-limit key no longer trusts a client-forged `X-Forwarded-For`
  (#107). The old `extract_client_key` took the **leftmost** XFF entry
  unconditionally, so a direct-connected client could mint a fresh full token
  bucket per request by rotating the header. XFF is now trusted only when the
  operator declares the proxy hop count via
  `RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES` (default `0` = never trust; the client
  address is then the `trusted_proxies`-th XFF entry counting from the
  **right**, discarding the untrusted leftmost chain). A chain shorter than the
  configured hop count fails closed to the socket IP rather than trusting a
  client-supplied entry, and that socket-IP fallback is now actually wired: the
  server make-service installs `ConnectInfo<SocketAddr>`
  (`into_make_service_with_connect_info`) — plain `axum::serve` silently omits
  it, so without this the fallback degraded to one shared `ip:unknown` bucket.
  Multiple `X-Forwarded-For` header *fields* are flattened before indexing, so a
  proxy that emits its own field cannot leave the client-controlled entry
  selectable. Requests carrying a non-empty `x-api-key` are keyed on the hash of
  that header regardless of any XFF they send — note the limiter runs *before*
  authentication, so the header only partitions buckets and is not an
  authorization decision; a client rotating a bogus `x-api-key` can therefore
  still mint fresh buckets (pre-existing limitation, tracked separately). The
  bucket map is also reclaimed now (SEC-011): `RECURSIVE_RATE_LIMIT_MAX_BUCKETS`
  (default 10 000) is a burst guard that evicts an idle bucket when a new key
  arrives at the cap, and the reaper's periodic `prune()` drops every bucket
  that has refilled to capacity. Both judge idleness from the clock
  (`tokens + elapsed × refill_rate`), not from the stored counter — a drained
  bucket always stores `capacity − 1`, so a counter-only predicate could never
  fire and the map grew without bound under a unique-key flood. Eviction prefers
  header-derived (`xff:`) keys over socket (`ip:`) and authenticated
  (`apikey:`) ones, so a unique-key flood cannot crowd real clients out. The cap
  is a burst guard, not a hard ceiling — a bucket used within the refill window
  is never evicted, so the map settles around `cap + (new keys/s) × (refill
  window)`, and every key untouched for that window is reclaimed. Multi-replica
  shared-state limiting remains a separate (cloud-runtime) work item — buckets
  are still per-process.
- feat(sandbox): 会话级环境绑定 + 能力注入 + 后台任务随环境销毁 (#31)：`ToolRegistry`
  持有会话的 `BackgroundJobManager`（`Clone` 共享、fork 新建），`run_background` 在容器档
  经共享 transport 在沙箱内执行（解除宿主执行的排除）；新增幂等
  `AgentRuntime::destroy_environment` / `ToolTransport::destroy`，会话删除、空闲驱逐、
  优雅关停及一次性 run（含 `/agui`）路径均回收环境；`session_tool_registry()` 改为
  async/Result 形态，容器创建失败按会话返回 503 而非进程退出；非本地档向系统提示注入
  `<environment>` 段（网络/持久化/工具链等能力），本地档提示保持逐字节不变。
- feat(sandbox): ContainerEnvironment 容器档执行环境与选择入口 (#30)：`RECURSIVE_SANDBOX=container`
  或 `--sandbox container` 时，所有 I/O 工具（Read/Write/Edit/Glob/Grep/Bash 等）通过
  `ContainerTransport` 在沙箱容器内执行，HTTP 服务为每会话构建独立容器 registry；非
  `cloud-runtime` 构建下 container/microvm 档显式 exit(2) 拒绝降级，宿主执行工具
  （run_background 等）在容器档不再注册，消除"容器跑命令、宿主执行"绕过。
- Fixed unknown-model cost reporting (#42): an unpriced model no longer fakes
  `$0.0000` — `.meta.json` writes `cost_usd: null` instead of `0.0`, and the
  CLI cost line prints `cost: unknown (no pricing for <model>)`.
- `config show` no longer labels a preset's default model as `model=` (#43):
  the line now reads `default_model=`, so it can't be mistaken for the effective
  model (which `provider.model` / `RECURSIVE_MODEL` may override). New
  integration test in `crates/recursive-cli/tests/config_show.rs`.
- Fixed publishable-crate version drift (#44): recursive-tui and the agui-*
  crates had lagged at 0.8.2 while recursive-agent/cli shipped 0.8.3. All six
  publishable crates are now aligned at 0.8.3, internal path-dependency version
  reqs are pinned exactly (`=0.8.3`), and a new `tests/lockstep.rs` plus
  `.dev/scripts/check-lockstep.sh` (supports a `vX.Y.Z` tag argument) enforce
  lockstep so drift fails loudly. `release.yml` wiring of the script is a
  follow-up.
- TUI: status-bar pricing no longer re-reads the provider catalog (disk IO +
  TOML/JSON parsing) on every render frame — `App` caches
  `(model_name, pricing)` per model (`pricing_for_model`,
  `pricing_lookup_count()`), the render path reads the cache only (issue #41).
- HTTP `/metrics` gains density & queue gauges (#19): `recursive_runs_in_flight`
  (runs currently holding an admission permit, RAII-tracked via `RunPermit`'s
  `Drop` so failed acquisitions never count) and
  `recursive_transcript_bytes_total` (estimated transcript size across live
  sessions; busy sessions skipped and reported by
  `recursive_transcript_bytes_skipped`). `AdmissionGate` now takes the
  in-flight counter in both constructors.
- Documentation: new `docs/llm-gateway-compat.md` collecting the implicit
  wire-protocol constraints behind issues #15/#16/#17 (new-api/one-api and
  Bedrock gateway traps), linked from the README Docs section.

## 0.8.2

Release-infra hardening release; no product code changes.

### Release channels
- **crates.io publish chain repaired**: the channel had silently stalled at
  0.6.0 — `cargo publish -p recursive-agent` verified against stale registry
  copies of its workspace siblings and the failure was swallowed by
  `continue-on-error`. All publishable crates now version-lockstep (agui-*,
  recursive-agent, recursive-tui, recursive-cli), internal deps carry version
  reqs, and the release job publishes the whole workspace in dependency order
  with index-propagation retries. Failures are loud now.
- **Homebrew tap automated**: `release.yml` bumps `jeffkit/homebrew-tap`
  after every release (deploy-key auth, idempotent). The tap had lagged at
  0.7.0 through two releases.
- Docker image build unchanged; GitHub Releases unchanged.

### Maintenance (0.8.1 post-release fixes, shipped here)
- Gate repairs: `cargo fmt` + 2 clippy `needless_borrow` in
  `src/http/handlers.rs`; h2 0.4.14→0.4.19 (RUSTSEC-2026-0258) and
  rustls 0.23.40→0.23.45 (RUSTSEC-2026-0285) upgraded; unused `serde`
  dependency dropped from recursive-tui.
- Test isolation: the TUI session-listing test holds `env_lock` and filters
  foreign sessions; all six session-writing backend tests pin
  `PinnedRecursiveHome` so test runs no longer write sessions into the real
  user data dir.

## 0.8.1

172 commits since 0.8.0. Highlights:

### Features
- **Agent Client Protocol (ACP)**: `recursive acp` server implementing
  P0–P7 of ACP v1 — stdio JSON-RPC loop, session lifecycle, permission
  bridge, editor fs + MCP multi-transport (new `recursive::acp` module).
- **OTLP trace exporter**: feature-gated `otel` OpenTelemetry export in
  the CLI (Goal 325).
- **Sub-agents on by default**: backgroundable continued conversations,
  coordinator briefing methodology, and a multi-sub-agent E2E regression
  suite with aimock record/replay.
- **Compaction upgrade**: circuit breaker, opt-in Microcompactor, and
  post-compaction re-injection of the plan, todo list, recently-read
  files, and invoked skills after cross-turn compaction.
- **TUI select & copy**: selectable/copyable agent output (Goal 349);
  `/compact-before` and `/compact-after` commands (Goal 342); context
  gauge reads the effective provider catalog.
- **Config**: `AGENTS.md` is authoritative, `CLAUDE.md` is fallback only.

### Architecture & reliability
- `#![deny(clippy::unwrap_used, expect_used)]` rolled out workspace-wide
  across all 7 crates — the shipping `recursive` binary no longer carries
  production `.unwrap()`/`.expect()` in session-resume / control paths
  (Invariant #5, Goal 354).
- `src/compact.rs` promoted to `src/compact/` module; unified compaction
  threshold decision (Goal 330); CompactionRunner refactor with
  boundary-preserving validation (Goal 341/347).
- Recompaction-in-chain telemetry (Goal 338) and cache hit/miss metrics
  on `CompactionBoundary` (Goal 336).
- Edit staleness check decoupled from the full-content cache (Goal 348).

### Bug Fixes
- Mid-stream `Error::Cancelled` now persists the transcript (Invariant #7,
  Goal 353).
- Replaced unsound unmaintained `serde_yml 0.0.12` (RUSTSEC-2025-0068)
  with `serde_yaml_ng` (Goal 355).
- Cross-turn compaction sizes by `last_prompt_tokens`, not the accumulated
  sum; degenerate emergency compaction without hook events is rejected.
- TUI: pasted CR/CRLF normalized to real newlines; ESC double-press
  interrupt; thinking duration no longer accumulates across thoughts
  (Goal 352); `/clear` stops an active event-driven loop.
- SshTransport host key verification hardened (Goal 349).

### Dev & E2E
- Flow watchdog detects hung `recursive` processes (Goal 346); in-flight
  work preserved on flow kill (Goal 345).
- E2E record/replay unified onto a single MCP path with self-healing
  aimock mode.
- Flowcast 0.6 executor migration; fail-fast quality-gate preflight;
  `/loop` supervise shipped as a loadable skill with agent-controlled stop.

## 0.8.0

192 commits since 0.7.0. Highlights:

### Features
- **AG-UI interrupt/resume** (Pattern 2 HITL): pause agent runs for human
  approval and resume from the same session state.
- **CLI Claude Code JSON alignment**: `--output-format json` with
  bidirectional control channel for programmatic embedding.
- **SDK `query()` alignment**: subprocess CLI turn results match the
  control-channel protocol.
- **`config.toml`**: file-based config for search, stuck-detection, and
  limits sections (alongside existing env vars).
- **WebSearch zero-config fallback**: DuckDuckGo/Bing HTML scrape when no
  API key is configured.
- **TUI loop driver**: event-driven `/loop` with enforced `max_turns` cap;
  slash menu loads real skills from disk.

### Architecture & reliability
- Run-inner refactor: extracted step helpers from `RunCore::run_inner`
  (check_shutdown, enforce_transcript_budget, drain_mailbox, etc.).
- `SessionLifecycle` + documented lock hierarchy in runtime.
- Hard step cap (P3-1) and monotonic sequence numbers (P3-2) for
  multi-agent runs.
- Self-improve flow: agentic fix-loop replaces hard rollback; optional
  `--hitl ilink` backend for human-in-the-loop gates.
- Eliminated 6 flaky test failures; expanded mutation-test baseline
  across core, TUI, and tools.

### TUI & platform
- Windows CI fixes for recursive-tui tests; symlink handling in skill
  command tests.
- Mutation-test debt cleared across command_menu, markdown, modal, chat,
  completion, and related modules.

### Dev & E2E
- ArgusAI 0.14.x adoption with hardened E2E gate and MCP run path.
- Clippy lints surfaced to the self-improve agent for in-run fixes.

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

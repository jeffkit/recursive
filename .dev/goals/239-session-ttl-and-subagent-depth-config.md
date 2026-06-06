# Goal 239 — HTTP session TTL eviction + sub-agent depth limit from Config

**Roadmap**: Arch-review bugfixes (part 3/3)

**Design principle check**:
- Implemented as: TTL reaper task in HTTP server + depth limit read at startup
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Two resource/security issues from the architecture review:

1. **Unbounded HTTP session store**: `AppState.sessions` is a plain
   `HashMap` with no eviction, TTL, or size cap. Each session holds a
   full `AgentRuntime` (transcript, checkpoint state, etc.). Under load
   this exhausts heap memory.

2. **Sub-agent depth limit via env var**: `RECURSIVE_SUBAGENT_MAX_DEPTH`
   is read from the environment at call time in `src/tools/sub_agent.rs`.
   A child process can unset/override it to remove the limit. It should
   be read once at startup and frozen in `Config`.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add `subagent_max_depth` field

Add a field to `Config`:

```rust
/// Maximum sub-agent nesting depth. Default 2.
/// Overrides RECURSIVE_SUBAGENT_MAX_DEPTH env var at startup.
pub subagent_max_depth: usize,
```

Load it from `RECURSIVE_SUBAGENT_MAX_DEPTH` env var (parse as usize,
default 2) in the same `Config::from_env()` / builder pattern already
used by other fields.

### 2. `src/tools/sub_agent.rs` — use `Config.subagent_max_depth`

Currently the tool reads `std::env::var("RECURSIVE_SUBAGENT_MAX_DEPTH")`
at call time. Change it to read the value from the `Config` that is
passed to the tool at construction time (check how other tools in
`src/tools/` receive config — likely via a field on the struct or passed
in `build_standard_tools`).

If `SubAgent` doesn't currently hold a reference to `Config`, add
`max_depth: usize` as a plain field, and set it from
`config.subagent_max_depth` when the tool is constructed in
`build_standard_tools` (in `src/tools/mod.rs`).

### 3. `src/http/mod.rs` or `src/http/handlers.rs` — session TTL reaper

Add a configurable `session_ttl_secs: u64` to `AppState` (default 1800
= 30 minutes). When a session receives a message, update a
`last_active: Arc<AtomicU64>` (unix timestamp) stored in `SessionState`.

Spawn a background task in `build_router` / server startup that wakes
every 60 seconds, scans `sessions`, and removes sessions whose
`last_active` is older than `session_ttl_secs`. Before removing each
session, call `runtime.close(None).await` to fire `SessionEnd`.

Alternatively, if `SessionState` already has a timestamp field, reuse it.
Read `src/http/mod.rs` SessionState definition before adding fields.

Minimal implementation notes:
- `last_active` update: in `send_message` handler, after acquiring the
  session lock, update `last_active` to `SystemTime::now()` as unix secs.
- Reaper interval: 60 seconds is fine.
- TTL default: 1800 seconds (30 min). Read from `RECURSIVE_SESSION_TTL_SECS`
  env var at startup; store in `AppState`.
- When TTL = 0, disable the reaper (opt-out for local dev / tests).

### 4. Tests

- Add a unit test for `Config` that verifies `subagent_max_depth` parses
  from the env var.
- In `tests/http.rs` or inline `#[cfg(test)]`: add a test that creates a
  session, verifies it appears in list, simulates TTL expiry (or test the
  reaper function directly with a TTL of 0 seconds and a past timestamp),
  and verifies it's removed.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `Config.subagent_max_depth` exists and is read from env var at startup
- `SubAgent` tool uses `Config.subagent_max_depth` not runtime env lookup
- HTTP session TTL reaper exists and calls `runtime.close()` on eviction

## Notes for the agent

- Read `src/config.rs` to understand how other env vars are parsed.
- Read `src/tools/sub_agent.rs` to find the current env var read and the
  struct definition.
- Read `src/http/mod.rs` to find `SessionState` before adding fields.
- Read `src/tools/mod.rs` `build_standard_tools` to find where SubAgent
  is constructed.
- Use `apply_patch` / surgical edits only. Do NOT rewrite whole files.
- Keep the reaper task simple — a `tokio::spawn` loop with
  `tokio::time::sleep(Duration::from_secs(60))` is fine.
- **DO NOT modify** `src/llm/`, `src/run_core.rs`, `src/compact.rs`,
  `src/session.rs`, `src/runtime.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.

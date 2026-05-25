# Goal 50 — Docker Sandbox Backend

**Roadmap**: 4.1 — Docker Sandbox Backend

**Design principle check**:
- Implemented as: new module `src/sandbox.rs` providing a `DockerSandbox`
  wrapper that sits between `Agent` and `ToolRegistry`. The agent loop
  is unchanged.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

The current path-based sandboxing (`resolve_within`) prevents filesystem
escapes but doesn't protect against arbitrary network access, system calls,
or resource exhaustion from `run_shell`. Docker sandboxing gives true
process isolation — the agent's tools execute inside a container with
controlled mounts, no network (optional), and resource limits.

## Scope (do exactly this, no more)

### 1. `src/sandbox.rs` — new module

Implement `DockerSandbox`:

```rust
pub struct DockerSandbox {
    container_id: String,
    workspace_mount: PathBuf, // host path mounted as /workspace in container
    image: String,            // docker image to use (default: "ubuntu:22.04")
    network: bool,            // enable network access (default: false)
}
```

Public API:
- `DockerSandbox::new(workspace: &Path, image: &str, network: bool) -> Result<Self>`
  - Calls `docker create` with:
    - `-v <workspace>:/workspace`
    - `--workdir /workspace`
    - `--network none` (if network=false)
    - `--rm` (auto-remove on stop)
    - Image + `sleep infinity` as entrypoint (keeps container alive)
  - Calls `docker start <id>`
  - Returns the container ID

- `DockerSandbox::exec(&self, command: &str, timeout: Duration) -> Result<(String, i32)>`
  - Calls `docker exec <id> sh -c "<command>"` with timeout
  - Returns (stdout+stderr combined, exit code)
  - On timeout: `docker exec` kill + return timeout error

- `DockerSandbox::stop(&self) -> Result<()>`
  - Calls `docker stop <id>` + `docker rm -f <id>` (belt + suspenders)

- Implement `Drop` for `DockerSandbox` → calls `stop()` best-effort

### 2. `DockerToolRegistry` — wrapper

A `ToolRegistry`-like wrapper that intercepts `run_shell` calls and routes
them through `DockerSandbox::exec()` instead of local `tokio::process`.
Other tools (`read_file`, `write_file`, etc.) still operate on the host
filesystem through the mount — they work unchanged because the workspace
path is the same.

```rust
pub struct DockerToolRegistry {
    inner: ToolRegistry,
    sandbox: DockerSandbox,
}
```

Implements: `execute(&self, name, args) -> Result<String>`:
- If `name == "run_shell"`: extract `command` from args, run via
  `sandbox.exec(command, timeout)`
- Else: delegate to `inner.execute(name, args)`

### 3. `src/main.rs` — CLI flag

Add `--docker` flag (or env `RECURSIVE_DOCKER=1`):
- When set, wraps the normal `ToolRegistry` in `DockerToolRegistry`
- Default image: `RECURSIVE_DOCKER_IMAGE` env or `"ubuntu:22.04"`
- Default network: `RECURSIVE_DOCKER_NETWORK` env or `false`

### 4. `src/lib.rs` — re-exports

Export `DockerSandbox`, `DockerToolRegistry`.

### 5. Tests

**Important**: Docker tests must be gated behind `#[cfg(feature = "docker-tests")]`
or a runtime check (`docker info` succeeds), because CI and self-improve
worktrees may not have Docker available.

- Test: `DockerSandbox::new` creates a running container (check `docker ps`)
- Test: `exec("echo hello")` returns "hello\n"
- Test: `exec("exit 42")` returns exit code 42
- Test: `exec` with timeout kills hung command
- Test: `Drop` cleans up container (no leaked containers after scope exit)
- Test: `DockerToolRegistry` routes `run_shell` through docker,
  other tools through inner registry

## Acceptance

- `cargo test` green (docker tests skipped if docker unavailable)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `recursive run --docker "echo hello from docker"` works end-to-end
  (if docker available)
- Container is cleaned up on normal exit AND on panic/budget-exceeded
- No new dependencies (use `tokio::process::Command` for docker CLI calls)

## Notes for the agent

- Use `tokio::process::Command` to call `docker` CLI — don't add a Docker
  library dependency. The CLI is simpler and more portable.
- For `docker create` output parsing: it prints the container ID on stdout.
  Trim whitespace.
- The timeout on `exec` should use `tokio::time::timeout` wrapping the
  child process wait.
- Test functions that need Docker should check `docker info` at the start
  and `return Ok(())` (skip) if it fails. Don't use `#[ignore]`.
- Keep the `DockerToolRegistry` minimal — it's a thin proxy. The complex
  part is container lifecycle in `DockerSandbox`.
- For the `Drop` impl: spawn a blocking `std::process::Command` (not async)
  since Drop can't be async. Use `try` and ignore failures.

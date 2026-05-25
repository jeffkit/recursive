# Goal 53 — Tool Transport Abstraction

**Roadmap**: 4.1-redesign — Tool Transport (replaces Docker Sandbox)

**Design principle check**:
- Implemented as: new trait `ToolTransport` in `src/tools/mod.rs` and a
  new `src/tools/transport.rs` module. Tools call through the transport
  layer instead of directly using `tokio::process` / `tokio::fs`.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

The original 4.1 "Docker Sandbox" was too rigid — it wrapped Docker
specifically. The better abstraction is a **transport layer** that tools
execute through. This allows:
- Local execution (current default, zero overhead)
- Docker exec (container sandbox)
- SSH (remote machine)
- Cloud sandbox APIs (future)

This follows Claude Code's "tool alias" pattern where read/write/shell
operations can be routed to any backend.

## Scope (do exactly this, no more)

### 1. `src/tools/transport.rs` — new module

```rust
/// Abstraction over where/how tool operations execute.
#[async_trait]
pub trait ToolTransport: Send + Sync {
    /// Execute a shell command and return (stdout+stderr, exit_code).
    async fn exec_shell(&self, command: &str, cwd: &Path, timeout: Duration)
        -> Result<(String, i32)>;

    /// Read a file's contents.
    async fn read_file(&self, path: &Path) -> Result<String>;

    /// Write contents to a file (create parent dirs as needed).
    async fn write_file(&self, path: &Path, content: &str) -> Result<()>;

    /// List directory entries.
    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>>;

    /// Check if a path exists.
    async fn exists(&self, path: &Path) -> Result<bool>;
}

/// Default local transport — current behavior, just wraps tokio::fs and
/// tokio::process.
pub struct LocalTransport;
```

Implement `LocalTransport` with the exact same logic currently inside
the individual tools (extract from `fs.rs`, `shell.rs`).

### 2. `src/tools/mod.rs` — ToolRegistry gets a transport

Add an `Arc<dyn ToolTransport>` field to `ToolRegistry`:
```rust
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    transport: Arc<dyn ToolTransport>,
}
```

Default to `LocalTransport`. Add a builder method:
```rust
pub fn with_transport(mut self, transport: Arc<dyn ToolTransport>) -> Self
```

### 3. Refactor existing tools to use transport

`ReadFile`, `WriteFile`, `ListDir`, `RunShell` should accept a reference
to the transport and call through it instead of using `tokio::fs` /
`tokio::process` directly.

**Important**: This is a refactor. External behavior MUST be identical.
All existing tests must pass unchanged.

### 4. Tests

- Test: `LocalTransport::exec_shell` runs a command and returns output
- Test: `LocalTransport::read_file` / `write_file` round-trip
- Test: `ToolRegistry::with_transport` sets the transport
- Test: A mock transport can intercept tool calls (proves the abstraction
  works)

## Acceptance

- `cargo test` green — ALL existing tests pass (this is a refactor)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `LocalTransport` is the default — zero behavior change for users
- No new dependencies
- The transport is injectable — library consumers can provide custom
  transports (Docker, SSH, etc.) without modifying the kernel

## Notes for the agent

- Read `src/tools/fs.rs` and `src/tools/shell.rs` to understand the
  current direct I/O calls. Extract them into `LocalTransport` methods.
- The `resolve_within` sandboxing stays — it's applied BEFORE calling
  the transport. Transport sees already-validated paths.
- Keep `workspace: PathBuf` on each tool struct — the transport is about
  HOW to execute, the workspace is about WHERE (path validation is still
  tool-side).
- This is a pure plumbing refactor. If you find yourself changing test
  assertions or tool behavior, you're doing it wrong. Stop and re-read.

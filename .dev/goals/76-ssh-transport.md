# Goal 76 — Tool Transport: SSH Adapter

**Roadmap**: Phase 8.2 — Tool Transport SSH adapter

**Design principle check**:
- Implemented as: new transport variant in `src/tools/transport.rs`.
  Does not modify agent loop.
- Does NOT modify `agent.rs`.

## Why

The Tool Transport abstraction (from g53) enables running tools on
remote machines. An SSH adapter is the most practical first remote
transport — it enables the agent to operate on remote servers without
needing a custom daemon.

## Scope (do exactly this, no more)

### 1. `src/tools/transport.rs` — add SSH transport

Extend the existing transport abstraction with an SSH variant:

```rust
pub struct SshTransport {
    /// SSH connection string: user@host or user@host:port
    host: String,
    /// Path to private key (optional, defaults to ssh-agent)
    key_path: Option<PathBuf>,
    /// Remote workspace directory
    remote_workspace: PathBuf,
}
```

The SSH transport should:
- Execute shell commands via `ssh user@host 'command'`
- Transfer files via `scp` or by piping content over ssh
- Map tool operations to their SSH equivalents:
  - `read_file(path)` → `ssh host 'cat path'`
  - `write_file(path, content)` → `ssh host 'cat > path' <<< content`
  - `list_dir(path)` → `ssh host 'ls -1 path'`
  - `run_shell(command)` → `ssh host 'cd workspace && command'`

### 2. Implementation via `tokio::process::Command`

Use the system's `ssh` binary (no Rust SSH library needed):

```rust
impl SshTransport {
    async fn ssh_exec(&self, command: &str) -> Result<String> {
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg(&self.host);
        if let Some(ref key) = self.key_path {
            cmd.arg("-i").arg(key);
        }
        cmd.arg(command);
        // capture output, apply timeout
    }
}
```

### 3. Config support

Add SSH transport configuration via environment or config:
```
RECURSIVE_SSH_HOST=user@remote-server
RECURSIVE_SSH_KEY=/path/to/key
RECURSIVE_SSH_WORKSPACE=/home/user/project
```

### 4. Integration with existing Tool trait

The SSH transport wraps the existing `LocalTransport` pattern:
- Tools still implement `Tool` trait as before
- The transport layer intercepts `run_shell`, `read_file`, etc.
  and routes them over SSH instead of locally

### 5. Tests

- Test: SSH command construction is correct (verify command args)
- Test: timeout handling works
- Test: error when ssh binary not found
- Test: host parsing (user@host, user@host:port)
- Test: key path is passed correctly

**Note**: Tests should NOT require an actual SSH server. Test command
construction and parsing only. Mark any live tests as `#[ignore]`.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- SSH transport compiles and unit tests pass
- No actual SSH connections required for tests
- Existing local transport unaffected

## Notes for the agent

- Read `src/tools/transport.rs` for the existing transport abstraction.
- The key design: transport is a LAYER between the agent and tool
  execution. Tools don't know about transport — they just execute.
  The transport decides HOW to execute (local vs SSH).
- Use `tokio::process::Command` for spawning ssh (same pattern as
  `run_shell` tool).
- Don't add an SSH crate dependency. Use the system `ssh` binary.
  This is simpler, more reliable, and handles auth (keys, agent) for free.
- For `write_file` over SSH: `echo 'content' | ssh host 'cat > path'`
  or use heredoc. Be careful with shell escaping.
- Consider `StrictHostKeyChecking=no` and `BatchMode=yes` SSH options
  for non-interactive use.

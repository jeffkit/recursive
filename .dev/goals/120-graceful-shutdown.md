# Goal 120 — Graceful shutdown with signal handling

**Roadmap**: Phase 17.4 — Graceful shutdown + in-flight request draining

**Design principle check**:
- Implemented as: tokio signal handler + CancellationToken in runner.rs
- Agent loop checks cancellation between steps
- ❌ Does NOT change agent.rs main loop logic (only adds a check)

## Why

Currently `Ctrl-C` abruptly kills the process, potentially losing the
current session state. Graceful shutdown catches SIGINT/SIGTERM, lets
the current step finish, saves the session, and exits cleanly.

## Scope (do exactly this, no more)

### 1. Add shutdown signal handling in src/runner.rs

```rust
use tokio::signal;
use tokio_util::sync::CancellationToken;

/// Create a CancellationToken that triggers on SIGINT or SIGTERM.
pub fn shutdown_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        let ctrl_c = signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        
        tokio::select! {
            _ = ctrl_c => {},
            #[cfg(unix)]
            _ = sigterm.recv() => {},
        }
        tracing::info!("shutdown signal received, finishing current step...");
        t.cancel();
    });
    token
}
```

### 2. Wire into main.rs run command

```rust
let shutdown = recursive::runner::shutdown_signal();
// Pass to agent builder or check after run
```

### 3. Check cancellation in agent step loop

In `src/agent.rs`, at the top of the step loop (after incrementing step),
add a cancellation check. The Agent needs to accept an optional
CancellationToken:

Actually — to keep agent.rs minimal, add the check in `AgentRunner`
or in main.rs's event handling. The simplest approach:

After `agent.run()` returns, if the shutdown token is cancelled,
save the session and exit cleanly. The current step will have completed
naturally since the LLM call is not interrupted.

For the HTTP server: in the `Http` command handler, use the token with
`axum::serve(...).with_graceful_shutdown(token.cancelled())`.

### 4. Tests

- **Test A**: `shutdown_signal()` returns a valid CancellationToken
- **Test B**: Token cancels on simulated signal (test helper)

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` clean.
- Pressing Ctrl-C during a run finishes the current step and exits 0.
- Files modified: `src/runner.rs` (~30 lines), `src/main.rs` (~10 lines)
- **Add `tokio-util` to Cargo.toml** if not already present (for CancellationToken).

## Notes for the agent

- Check if `tokio-util` is already in Cargo.toml. If not, add it with
  `features = ["rt"]`.
- The HTTP server (if feature=http) already uses axum. Look for
  `axum::serve` or `.serve()` to add graceful shutdown.
- Do NOT modify agent.rs. The graceful shutdown is external — it lets
  the current agent step complete, then exits.
- For the CLI `run` command: after `agent.run()` returns, check if
  shutdown was triggered. If so, print a message and ensure session
  is finalized.
- Keep it simple: the first version just catches the signal and
  lets the current operation finish. No mid-step interruption.

# Goal 349 — Harden SshTransport host key verification (remove hardcoded StrictHostKeyChecking=no)

**Roadmap**: Post-Phase — Security hardening

**Design principle check**:
- Implemented as: transport-option change in `src/tools/transport.rs` — secure by default, explicit opt-in for the insecure mode with a startup warning
- ✅ Does NOT branch inside `run_core.rs::RunCore::run_inner`'s main loop
- ✅ Follows least-privilege: no silent MITM-able default

## Why

`SshTransport::build_ssh_command` (`src/tools/transport.rs` ~line 157-158) hardcodes:

```rust
cmd.arg("-o").arg("StrictHostKeyChecking=no");
cmd.arg("-o").arg("UserKnownHostsFile=/dev/null");
```

This **completely disables SSH host key verification** — anyone who can intercept the connection (network MITM, DNS spoofing, ARP spoofing) can impersonate the remote host and execute arbitrary commands in the agent's context. Since the transport is the execution primitive for a sandboxed `Bash`, this is a remote-code-execution-in-agent's-sandbox vector if it is ever wired to a provider.

History: flagged as **P3** in the 2026-06-06 architecture review (`docs/review/00-summary.md`, "SshTransport 硬编码 StrictHostKeyChecking=no"). Still unfixed as of this goal.

Current risk is mitigated only by the fact that **SshTransport has no production callers** (verified by grep: no `SshTransport` reference outside `transport.rs` and its tests). It is dead code — but a landmine: the moment someone wires it into a docker/ssh sandbox provider, the insecure default becomes live.

## Scope (do exactly this, no more)

### 1. `src/tools/transport.rs` — remove the insecure defaults

In `build_ssh_command`:
- Remove `-o StrictHostKeyChecking=no` and `-o UserKnownHostsFile=/dev/null` from the unconditional args.
- Keep `BatchMode=yes` and `ConnectTimeout`.

### 2. Add an explicit, warned opt-in for ephemeral hosts

Add a builder field + method on `SshTransport` (default `false`):

```rust
/// When `true`, skip strict host key verification for ephemeral / throwaway
/// hosts. MITM-unsafe: only for trusted networks or disposable sandboxes.
/// Default `false` (secure).
insecure_host_key_checking: bool,
```

Method:

```rust
/// Opt in to skipping host-key verification (MITM-unsafe).
/// Emits a `tracing::warn!` so the choice is visible in logs.
pub fn with_insecure_host_key_checking(mut self, on: bool) -> Self { ... }
```

When `true`, emit `tracing::warn!` at call time (in `build_ssh_command`) and append **one of**:
- `-o StrictHostKeyChecking=accept-new` (preferred — first-use trust, still detects later key changes), **or**
- if the goal's acceptance requires byte-compat with the old behavior, `-o StrictHostKeyChecking=no` plus `-o UserKnownHostsFile=/dev/null` — but ONLY under the opt-in flag.

Decision: use `accept-new` as the opt-in default unless a test in the repo demonstrates that `accept-new` is insufficient for a real supported use case. `accept-new` still blocks MITM on subsequent connections, which is strictly better than `no`.

### 3. Tests in `src/tools/transport.rs`

Add unit tests for `build_ssh_command` (the method is `fn`, not `async`, so plain `#[test]` works — verify how the current tests construct `SshTransport`):

- `default_ssh_command_does_not_disable_host_key_checking` — assert the produced `Command`'s args do NOT contain `StrictHostKeyChecking=no` and do NOT contain `UserKnownHostsFile=/dev/null`; DO contain `BatchMode=yes`.
- `insecure_opt_in_enables_host_key_checking_bypass` — with `with_insecure_host_key_checking(true)`, assert args contain the opted-in option (`accept-new` or `no`, whichever the implementation chose).
- `insecure_opt_in_defaults_to_false` — fresh `SshTransport` has `insecure_host_key_checking == false`.

Note: `Command` args can be inspected via `cmd.get_args()` (std) — check how existing tests in the file assert command shape, if any, and reuse that pattern.

## Files NOT to touch

- `src/tools/shell.rs`, `src/tools/docker_*.rs`, `src/tools/e2b_provider.rs` — unrelated sandbox paths
- `src/tools/transport.rs`'s `LocalTransport` / `ToolTransport` trait — unchanged
- Any other transport implementation

## Acceptance

- `cargo test --workspace` green (new tests + all existing)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Default `SshTransport` never emits `StrictHostKeyChecking=no` / `UserKnownHostsFile=/dev/null` unless the caller explicitly opted in via `with_insecure_host_key_checking(true)`
- Opt-in emits a `tracing::warn!` (visible in logs)

## Notes for the agent

- **Read first**: `src/tools/transport.rs` — the `SshTransport` struct, `build_ssh_command`, and the existing `#[cfg(test)] mod tests`.
- **SshTransport is currently dead code** (no production caller). Do not try to wire it anywhere; the goal is only to make the default safe so a future caller cannot silently adopt the MITM-able config.
- The `with_key` method already exists for identity files — that is orthogonal to host key verification (a key authenticates the client, not the server).
- If you find that `accept-new` breaks an existing test that relied on the old insecure default, that test is asserting the bug — update the test, don't re-add the insecure default.

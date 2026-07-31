# Manual edit: harden SshTransport host key verification

**Date**: 2026-07-31
**Goal**: 349 — Harden SshTransport host key verification (remove hardcoded
  StrictHostKeyChecking=no). Security hardening (P3 from the 2026-06-06
  architecture review, `docs/review/00-summary.md`).

**Files touched**:
  - `src/tools/transport.rs`
    - `SshTransport` struct: added `insecure_host_key_checking: bool` field
      (default `false`), documented as MITM-unsafe, secure-by-default.
    - `SshTransport::new`: initializes the new field to `false`.
    - New builder method `with_insecure_host_key_checking(self, on: bool) ->
      Self` — explicit opt-in; doc comment documents the MITM risk.
    - `build_ssh_command`: removed the unconditional
      `-o StrictHostKeyChecking=no` and `-o UserKnownHostsFile=/dev/null`
      args. Kept `BatchMode=yes` and `ConnectTimeout=<secs>`.
      When `insecure_host_key_checking` is true, emits
      `tracing::warn!(host = %self.host, "...")` and appends
      `-o StrictHostKeyChecking=accept-new` (first-use trust — still detects
      later key changes; strictly better than `no`).

**Tests added** (in `src/tools/transport.rs`, `#[cfg(test)] mod tests`):
  - `default_ssh_command_does_not_disable_host_key_checking` — default command
    args do NOT contain `StrictHostKeyChecking=no` or
    `UserKnownHostsFile=/dev/null`; DO contain `BatchMode=yes`.
  - `insecure_opt_in_enables_host_key_checking_bypass` — with
    `with_insecure_host_key_checking(true)`, args contain
    `StrictHostKeyChecking=accept-new` and still no `no`/`/dev/null` variants.
  - `insecure_opt_in_defaults_to_false` — fresh transport defaults the field
    to `false`; builder toggles true/false correctly.
  - Updated the two tests that asserted the old buggy behavior:
    `ssh_build_command_basic` (removed `StrictHostKeyChecking=no` assertion;
    now asserts the default does NOT disable host key checking) and
    `ssh_build_command_known_hosts` (renamed
    `ssh_build_command_known_hosts_secure_by_default`; now asserts the default
    does NOT append `UserKnownHostsFile=/dev/null`).

**Verification**:
  - `cargo test --lib tools::transport`: 36 passed, 0 failed (incl. 3 new).
  - `cargo test --workspace`: all test binaries green (incl. `invariants`, 35
    passed).

**Follow-up — fix the pre-existing `runtime_stays_manageable` invariant
failure (flow test-gate round 1)**:
  - The initial `cargo test --workspace` run failed on the pre-existing
    invariant `loop_size_orthogonality::runtime_stays_manageable`
    (`src/runtime.rs` was 4044 lines vs limit 3700; grown by legitimate
    feature landings after Goal 340 set the limit at 3672). The flow's test
    gate demanded a green tree, so the runtime file was split into child
    modules (no behavior change):
  - `src/runtime/checkpoint.rs` (NEW): `CheckpointState` moved out of
    `runtime.rs`; fields made `pub(crate)` because `runtime.rs` (parent
    module) reads/writes them directly (~40 lines saved).
  - `src/runtime/builder.rs` (NEW): `AgentRuntimeBuilder` (struct + Debug +
    Default + impl) moved out of `runtime.rs` verbatim (~340 lines saved).
  - `src/runtime.rs`: added `mod builder;` + `pub use builder::AgentRuntimeBuilder;`
    (public API path `recursive::runtime::AgentRuntimeBuilder` unchanged via
    the re-export — `lib.rs` `pub use runtime::{AgentRuntime, AgentRuntimeBuilder, ...}`
    still resolves) and `mod checkpoint;` + `pub(crate) use checkpoint::CheckpointState;`.
    Removed the two moved blocks; pruned 7 imports that were only used by the
    builder (`NullSink`, `HookRegistry`, `AgentKernelBuilder`,
    `EnterPlanModeTool`, `RequestPlanModeTool`, `ToolRegistry`, `AtomicUsize`)
    and added the two needed by the tests module (`HookRegistry`, `ToolRegistry`)
    directly to `mod tests`. Final size: 3677 lines (limit 3700).
  - `CheckpointState`'s `disabled()`/`enabled()` are now `pub(crate)`; all
    call sites in `runtime.rs` are unchanged. `SessionLifecycle` stays in
    `runtime.rs` (builder accesses it via `super::` — allowed since
    `builder` is a descendant module).
  - No tests were added for the extraction itself (pure code movement);
    the existing 2171 lib tests cover the moved code. The `agent-presence`
    gate is satisfied by the transport.rs test additions in this same run.
  - Re-verified after the fix: `cargo test --workspace` green, clippy clean,
    fmt clean.
  - `cargo fmt --all -- --check`: clean.

**Notes**:
  - `SshTransport` remains dead code with no production callers (verified:
    no reference outside `transport.rs` and its tests). This change makes the
    default safe so a future caller cannot silently adopt the MITM-able config.
  - Chose `StrictHostKeyChecking=accept-new` for the opt-in (per goal
    decision): blocks MITM on subsequent connections, strictly better than
    `no`. No existing test demonstrated `accept-new` to be insufficient for a
    supported use case.
  - `with_key` is orthogonal (client auth, not server verification) —
    untouched.
  - `tracing` is an existing direct dependency (Cargo.toml line 89); no new
    deps added.

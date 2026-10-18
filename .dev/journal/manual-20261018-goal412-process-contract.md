# Goal 412 — 进程与任务所有权/取消契约（#198 / #188）

- **Date**: 2026-10-18
- **Goal**: recursive#198（owner 拍板契约方向）/ #188（收口对象）：进程组 + 取消句柄 + destroy 清算 + 取消可观测
- **Judgement cases closed**: #164（输出超管道缓冲即死锁）、#165（超时只杀 sh）、#177（destroy 不清宿主后台任务）

## Site audit (Scope 1)

`grep -rn "Command::new|\.spawn()|start_kill|killpg|\.kill()" src crates/*/src` — three classes:

### Process creation points (`spawn` / `output`)
| site | class | disposition |
|---|---|---|
| `src/tasks/run_background.rs:466` (`/bin/sh -c`, host tier) | long-running | **converted** → `process::spawn_owned` (group leader + kill_on_drop) |
| `src/tools/transport_layer/transport.rs` `LocalTransport::exec_shell` | long-running (every `Bash`) | **converted** → `spawn_owned` + `TaskRegistration` (Exec, scope = cwd) |
| `src/tools/transport_layer/transport.rs` `ssh_exec` / `SshTransport::exec_shell` | long-running local `ssh` client | **converted** → `spawn_grouped` + `terminate_group` |
| `src/mcp.rs:590` `McpClient::spawn_stdio` | long-running server | **converted** → `spawn_grouped`; `kill_gracefully` now signals the group |
| `src/tools/execution/run_code/runner.rs:272` node runtime | long-running | **converted** → `attach_group` + `kill_group` on timeout |
| `src/hooks/external.rs:826,870` hook children | short-lived, but timeout arm leaks | **unchanged** — `wait_with_output` consumes the child, so the timeout arm cannot signal it without restructuring that loop; documented in `src/process.rs` module docs |
| `src/tools/checkpoint.rs`, `src/checkpoint.rs`, `src/deliverables/*`, `src/rewind.rs`, `src/http/agui.rs`, `crates/recursive-cli/src/cli/control.rs` (`git … .output()`) | one-shot, reaped inside `output()` | unchanged |
| `src/tools/transport_layer/container_transport.rs`, e2b | remote, no local child | unchanged (owned by `ToolTransport::destroy`) |
| `src/acp/session.rs:181-235`, `src/session/lifecycle.rs:129` | *kill* helpers for ACP/session children | unchanged (§#200 experimental; ACP wiring explicitly out of scope) |

### Termination points
- `run_background_job` timeout (`start_kill` only → #165) — **replaced** by `process::terminate_group` (killpg + start_kill + wait).
- `LocalTransport::exec_shell` timeout — same replacement; a new `cancelled()` arm returns `Interrupted`.
- `SshTransport` timeouts — `terminate_group`.
- `mcp::kill_gracefully` — SIGTERM/SIGKILL now go to the process group; the ad-hoc `kill -9 <pid>` subprocess call is gone.

### Environment-destroy points
- `AgentRuntime::destroy_environment` — **was** `bg_manager().clear()` (table reset, processes leaked: #177). Now `run_background::reclaim_all` (cancel → wait for the reaping → drop table → loud report) then `transport.destroy()`. Companion *non-process* resources (containers, remote workspaces, temp dirs) remain owned by each transport's `destroy()`, which runs immediately after.

## Files touched
- `src/process.rs` (new): process-group spawn/termination + `CancelHandle` / `CancelRegistry` / `TaskRegistration` + `recursive::tasks` events.
- `src/lib.rs`: register `pub mod process`.
- `src/tasks/run_background.rs`: concurrent output readers (#164), group termination (#165), per-job cancel handle, `JobState::Cancelled`, `reclaim_all` / `ReclaimReport`.
- `src/tools/transport_layer/transport.rs`: LocalTransport exec cancel + group kill; SSH group spawn/kill.
- `src/mcp.rs`: stdio server as group leader; group-aware graceful kill.
- `src/tools/execution/run_code/runner.rs`: group spawn + group kill on timeout.
- `src/runtime.rs`: `destroy_environment` = liquidation (process-table part only; no transcript/compaction fields touched).
- `src/http/environment_binding_tests.rs`: destroy test now seeds a *real* background process and asserts it is gone (#177).
- `Cargo.toml`: `libc` target widening `cfg(target_os = "linux")` → `cfg(unix)`.

## Tests added
- `test_process_contract_process_group` (`src/process.rs`)
- `test_process_contract_cancel_handle` (`src/tasks/run_background.rs`)
- `test_process_contract_destroy_reclaim` (`src/tasks/run_background.rs`)
- `test_process_contract_cancel_observable` (`src/process.rs`)
- `cancel_registry_routes_by_key_and_scope`, `task_registration_deregisters_on_drop`, `global_scope_cancel_reaches_registered_task` (`src/process.rs`)
- `background_output_beyond_pipe_buffer_does_not_deadlock` (#164 regression, `run_background.rs`)
- `background_timeout_kills_descendants` (#165 regression, `run_background.rs`)
- `reclaim_reports_jobs_that_never_reap` (fail-loud counting)
- `local_transport_exec_shell_is_cancellable_by_scope` (`transport.rs`)
- Each test's doc comment names the mutation that turns it red (Scope 6 negative controls).

## Notes
- **Windows boundary**: process groups are POSIX. `attach_group` is a no-op, `signal_group` returns `false`, and `terminate_group` keeps the pre-existing `Child::start_kill` path — unix behaviour is unchanged except that timeouts/cancels now reach the whole group.
- **Dependency justification (invariant #6)**: no crate added. `libc` was already a direct dependency for `prctl` on Linux and is already in `Cargo.lock` for every unix (tokio/reqwest pull it); `killpg` needs no Linux-specific API, so its target cfg widened from `linux` to `unix`. `scripts/check-new-deps.sh` sees an unchanged `libc\t0.2` entry.
- **Tenancy**: cancel scopes are opaque strings supplied by the edge (job-table id, workspace root). `TurnContext`/kernel still carry no identity (owner ruling 2026-10-18).
- **Not wired**: ACP/MCP *functional* cancel routing (#200 experimental). The interface is reachable: `process::cancel_task(key)` / `process::cancel_scope(scope)` against `process::cancel_tasks()`.

---
type: Architecture
title: Cancellation & Process Ownership — the Four Contracts
description: Process groups + killpg, cancel handles in a process-wide task table, destroy = liquidation, observable cancellation — the ownership/cancellation contracts across the five execution channels, with authoritative symbols, regressions and mutation negative controls (issue #198 / #188).
tags: [cancellation, process-groups, killpg, destroy, background-jobs, acp, mcp, issue-198, issue-188]
timestamp: 2026-10-24T00:00:00Z
---

# Cancellation & Process Ownership — the four contracts (issue #198)

Source: `src/process.rs` (module doc — the contract text), `src/tasks/run_background.rs`, `src/tools/transport_layer/transport.rs`, `src/mcp.rs`, `src/tools/execution/run_code/runner.rs`, `src/acp/server.rs`, `src/runtime.rs`.

Ending a task is a **guaranteed operation**: every subprocess is spawned into
a process group someone owns, every long-running piece of work is addressable
in a task table, environment teardown is liquidation (not a table reset), and
every cancellation is observable. That direction was ratified by the owner for
#198; the implementation landed with Goal 412 (`75749136`) and the #179 ACP
fix (`4abc3a81`). The authoritative semantics live in the code —
`src/process.rs`'s module doc is the contract text and the
`test_process_contract_*` tests are the executable statement. This page is the
map that ties each contract to its symbols, regressions and negative controls.
If code and page ever disagree, code plus tests win: file an issue, do not
patch either side casually.

**Form difference vs the plaita precedent** (`DESIGN-cancellation-and-lease.md`,
the four-section template this page follows): that document catalogued *open*
breakpoints and designed their fixes. Here the breakpoint list is a **closed
mapping** — all four breakpoints (#164 / #165 / #177 / #179) are already
fixed on `main`, each with regressions and mutation negative controls (§2).
There is no pending design in this page; what remains is documentation of the
landed state plus the owner's ruling on contract 2's two deliberately
half-landed edges (§4.1).

## 1. Current map — channel by channel

Five channels carry long-running work, plus the destroy chain. The precise
cancel semantics differ per channel — this is the part most worth reading
before consuming any of them:

- **Background jobs** (`run_background`): the job handle is immediate.
  Insertion registers the job in the task table keyed
  `background-job:<scope>/<id>` (`spawn_keyed`, `run_background.rs:211`);
  `cancel(id)` / `task_stop` fire the handle, the job loop's
  `token.cancelled()` arm reaps the whole group and records
  `JobState::Cancelled` (`run_background.rs:959-981`). Host tier: the command
  is a group-leader `spawn_owned` (`run_background.rs:683`); reader tasks are
  spawned *before* the waiter is polled and drained with a bound on every
  exit path (`run_background_job`, `run_background.rs:931-1002`). Sandbox
  tier (transport-backed jobs): the job selects the exec future against
  `handle.cancelled()` (`run_background.rs:643-652`) — dropping the exec
  future is how a sandbox execution stops early (local tier reaps via
  `kill_on_drop`; container tier's `destroy()` removes the environment).
- **Foreground exec — local**: `LocalTransport::exec_shell` registers the
  exec in the task table (scope = the workspace root,
  `transport.rs:927-930`), spawns via `spawn_owned` (`:936`), and waits on a
  three-arm select: done / timeout / cancelled (`:958-962`). The timeout arm
  and the cancelled arm both `terminate_group` (`:972`, `:982`). The cancel
  arm is reachable through `process::cancel_scope`, but has **no production
  caller today** (§4.1) — the cancel paths that actually fire are the timeout
  arm and future-drop `kill_on_drop`.
- **Foreground exec — SSH**: the local `ssh` client is the group leader
  (`transport.rs:542`, `:719`); a timeout terminates the group (`:566`,
  `:740`), which drops the connection — the remote command has no local child
  to own.
- **MCP**: server-level, a stdio server is spawned as a group leader
  (`mcp.rs:643`) and `kill_gracefully` signals the group (SIGTERM → grace →
  `kill_group`, `mcp.rs:2611+`; a graceful leader exit is *not* proof the
  tree is gone — launchers like `npx` leave the real server as a child).
  Call-level, each `tools/call` is bounded by `tool_timeout` (#181,
  `mcp.rs:385`; the handshake/discovery `read_timeout` is a separate
  transport-wide backstop, `:609-617`). No per-call task-table registration
  exists (§4.1).
- **ACP prompt turns**: cooperative at the step boundary — see §4.2 before
  consuming. In one line: `session/cancel` fires the session's current-turn
  token; the token ends the turn between steps or mid-LLM-stream, and the
  in-flight tool call runs to its own completion or timeout.
- **`run_code` runtime**: the node runtime is attached to its own group at
  spawn (`attach_group`, `runner.rs:754`) and the timeout arm SIGKILLs the
  group (`kill_group`, `runner.rs:948`).
- **Destroy chain**: `AgentRuntime::destroy_environment`
  (`runtime.rs:1426-1460`) = `reclaim_all` (cancel every job → bounded reap →
  drop table, `run_background.rs:457-490`) → `ReclaimReport` → WARN/info →
  `transport().destroy()`. Idempotent, runs on every teardown path (HTTP
  DELETE / idle eviction / graceful shutdown). Contract 3 below.

## 2. Breakpoint list → closed mapping

**All four breakpoints are closed.** Unlike the plaita precedent — where this
section lists what cannot yet be cancelled — every entry below is fixed on
`main`, each with its mechanism, landing commit, regression and mutation
negative control. Read this as provenance, not as a to-do list.

| Breakpoint | Mechanism that closed it | Landing | Regression | Negative control (mutation → red) |
|---|---|---|---|---|
| #164 output beyond the pipe buffer deadlocked `run_background` | reader tasks spawned before the waiter is polled; bounded drain on every exit path | Goal 412 `75749136` | `background_output_beyond_pipe_buffer_does_not_deadlock` (`run_background.rs:1854`) | poll `child.wait()` without the concurrent readers → the test wedges on a full pipe |
| #165 timeout killed `sh` only; descendants survived | every child is a process-group leader; one termination point `terminate_group` (killpg → `start_kill` → reap) | Goal 412 `75749136` | `background_timeout_kills_descendants` (`run_background.rs:1903`), `test_process_contract_process_group` (`process.rs:631`) | `start_kill` instead of `terminate_group` (or drop the `kill_group` call) → descendant survives |
| #177 `destroy_environment` reset the job table; host processes kept running | destroy = liquidation: `reclaim_all` → `ReclaimReport` → `transport().destroy()` | Goal 412 `75749136` | `test_process_contract_destroy_reclaim` (`run_background.rs:1785`), `environment_binding_destroy_drains_background_jobs` (seeds a real process) | `reclaim_all` clears without cancelling → still-alive assertion red; reporting `unreclaimed: 0` unconditionally → red |
| #179 ACP `session/cancel` could not interrupt a running turn | read loop decoupled from turns: channelised writer (mpsc + writer task), turn tasks, per-session cancel registry, fresh token per turn → `set_interrupt_token`; notification form (no id) dispatched before early exit | `4abc3a81` (cherry-pick of B2 `5d4f335f`) | `notification_cancel_interrupts_running_turn`, `request_form_cancel_still_works`, `cancel_does_not_affect_next_turn`, `cancel_for_other_session_is_noop`, `cancel_token_fresh_after_each_turn` (+4 `session_cancel_*` shape tests) | cancel reaches only the addressed session (`cancel_for_other_session_is_noop`); the token is fresh each turn (`cancel_token_fresh_after_each_turn`) |

## 3. The four contracts

### Contract 1 — process groups: one spawn entry, one kill entry

Every local long-running child is spawned as the **leader of its own process
group** (`spawn_grouped` / `spawn_owned`, `src/process.rs:500-511` —
`process_group(0)` plus `kill_on_drop`), and every timeout/cancel ends
through the single termination point `terminate_group`: `killpg` the group,
`start_kill` as belt-and-braces, then reap (`src/process.rs:569-575`). Group
signalling is WARNING-loud on any failure except the normal post-exit `ESRCH`
race (`signal_group`, `src/process.rs:520-551`). Windows: process groups are
POSIX — `attach_group` is a no-op, `signal_group` returns `false`, and
`terminate_group` keeps the pre-existing `Child::start_kill` path; the
contract introduces no new Windows behaviour (`src/process.rs:25-27`).

Regression: `test_process_contract_process_group`. Negative control: drop the
`kill_group` call in `terminate_group` → the "descendant survived" assertion
goes red (`src/process.rs:663`).

### Contract 2 — the task table: cancellation is data routed to an addressable handle

Long-running work registers in the process-wide table `cancel_tasks()`
(`src/process.rs:358`) as a `TaskRegistration`, producing a `CancelHandle`
(`src/process.rs:137-465`); a cancel channel resolves the handle by key or
scope instead of threading a flag through the call stack. Registration tracks
the task's *lifetime*: dropping the registration deregisters, so a dropped
exec future or a cleared job row cannot leave a stale routable entry.
Landing points: background jobs keyed `background-job:<scope>/<id>`
(`run_background.rs:211`); foreground exec scoped to the workspace root
(`transport.rs:929`); ACP turn tokens registered per session
(`TurnControl`, `server.rs:179`) and wired into the kernel via
`set_interrupt_token` (`runtime.rs:1490`). Consumers today: the job loop's
cancel arm, the exec select's cancelled arm (`transport.rs:961`), and the ACP
step-boundary probe. **Two edges are deliberately half-landed — §4.1.**

Regressions: `test_process_contract_cancel_handle` +
`cancel_registry_routes_by_key_and_scope` +
`global_scope_cancel_reaches_registered_task` +
`local_transport_exec_shell_is_cancellable_by_scope` +
`task_registration_deregisters_on_drop` + the ACP cancel five. Negative
controls: skip `token.cancel()` in `cancel_with` → registry test red
(`src/process.rs:669-671`); drop the waiter's `token.cancelled()` arm →
`test_process_contract_cancel_handle` red (`run_background.rs:1717-1722`);
drop the exec registration → `local_transport_exec_shell_is_cancellable_by_scope`
red (`transport.rs:1290-1292`).

### Contract 3 — destroy = liquidation, fail-loud

「destroy = 清算非清表」. `destroy_environment` (`src/runtime.rs:1426-1460`) is
idempotent (once-guard) and runs on every teardown path: `reclaim_all`
cancels every running job through its handle, waits a bounded grace
(`DEFAULT_RECLAIM_GRACE`) for each job loop to reap its own group, drops the
table, and returns `ReclaimReport { cancelled, unreclaimed }`
(`run_background.rs:442-490`). `unreclaimed > 0` is a **WARNING** on
`recursive::tasks` — the comment cites the owner ruling 2026-10-18 「任何降级
必须可观测」 (`runtime.rs:1443-1451`); a clean sweep logs info. Then
`transport().destroy()` owns the non-process companion resources (containers,
remote workspaces, temp dirs).

The trade-off is deliberate: liquidation failure surfaces as a loud WARNING
plus the report, **not** as a failed destroy — destroy is an idempotent
terminal operation, and an error return would leave the environment
half-torn-down on every retry. What must never happen is a silent leak
looking like success (#177).

Regressions: `test_process_contract_destroy_reclaim` +
`reclaim_reports_jobs_that_never_reap` +
`environment_binding_destroy_drains_background_jobs`. Negative controls: make
`reclaim_all` clear the table without cancelling → the still-alive assertion
goes red (`run_background.rs:1781-1783`); report `unreclaimed: 0`
unconditionally → red (`run_background.rs:1831-1832`).

### Contract 4 — cancellation is observable

Every cancellation lands a structured record on `CANCEL_TARGET =
"recursive::tasks"`: `CancelEvent { key, kind, scope, reason, stage, at_ms }`
(`src/process.rs:102-115`) — who, when, what, and how far the task had got.
`RUST_LOG=recursive::tasks=warn` shows each event (`cancel_with`,
`src/process.rs:208-217`); an in-process bounded ring (`MAX_CANCEL_EVENTS =
64`) serves tests and consumers via `CancelRegistry::events()`. `stage` is
frozen at cancel time — `note_stage` ignores writes after cancellation
(`src/process.rs:176-184`). `CancelReason::Explicit / Scope / Destroy`
(`src/process.rs:81-88`) lets a consumer tell an operator stop from a scope
sweep from teardown liquidation. Downstream observability: destroy's
WARN/info (contract 3), the `ReclaimReport`, ACP's `stopReason: "cancelled"`
(`server.rs:1648-1651`), and pairable transcripts — a mid-stream cancel
persists the partial reply (Invariant #7, `finish_interrupted`,
`run_core.rs:1149`, #168's `has_persistable_interrupt` predicate), and a
still-unanswered tool call is answered by the resume orphan policy
([sessions.md](sessions.md)).

Regression: `test_process_contract_cancel_observable`. Negative control
(implicit, proven): delete the `tracing::warn!` in `cancel_with` → the
`logs_contain` pins go red.

### Contract × regression × negative control

| Contract | Authoritative landing | Regression | Negative control |
|---|---|---|---|
| 1 process groups + killpg | `src/process.rs` (Goal 412 `75749136`): `spawn_grouped`/`spawn_owned` + `terminate_group`; windows = `start_kill` fallback, no new behaviour | `test_process_contract_process_group` + the channel timeout regressions | drop `kill_group` in `terminate_group` → descendant-survived red (`process.rs:663`) |
| 2 task table + cancel handles | `CancelRegistry`/`TaskRegistration` (`process.rs:137-465`); job-keyed registration (`run_background.rs:211`), exec scope registration (`transport.rs:929`), ACP turn-token registry (`server.rs:179`) | `test_process_contract_cancel_handle`, `cancel_registry_routes_by_key_and_scope`, `global_scope_cancel_reaches_registered_task`, `local_transport_exec_shell_is_cancellable_by_scope`, `task_registration_deregisters_on_drop`, ACP cancel five | skip `token.cancel()` → registry red; drop the waiter's cancel arm → handle red; drop the exec registration → scope red |
| 3 destroy = liquidation | `runtime.rs:1426-1460` + `reclaim_all`/`ReclaimReport` (`run_background.rs:442-490`) | `test_process_contract_destroy_reclaim`, `reclaim_reports_jobs_that_never_reap`, `environment_binding_destroy_drains_background_jobs` | clear-without-cancel → still-alive red; an unconditional `unreclaimed: 0` → red |
| 4 observable cancellation | `CANCEL_TARGET="recursive::tasks"` + `CancelEvent` fields (`process.rs:43`, `:102-115`) + destroy WARN/info + `stopReason: "cancelled"` | `test_process_contract_cancel_observable` | implicit: delete the emit → the `logs_contain` pins red |

## 4. Remaining path — owner ruling (2026-10-24) and honest boundaries

### 4.1 Contract 2's two half-landed edges: the boundary is ratified, zero code

The owner ratified option (a) — **documentation ratifies the boundary; no
speculative wiring** (2026-10-24; the rejected option (b) was a minimal
`mcp.rs` registration with no production caller to justify it):

- **The exec scope handle is a reserved, reachable interface.** The exec
  cancel path in production is the timeout arm plus future-drop
  `kill_on_drop`. The scope handle (`process::cancel_scope` /
  `process::cancel_task` against the registration's scope/key) is fully
  reachable — exercised by `local_transport_exec_shell_is_cancellable_by_scope`
  and `global_scope_cancel_reaches_registered_task` — but has **zero
  production callers** (verified repo-wide 2026-10-24; only those tests call
  it). Under #200's channel tiering this is a recorded status quo, not a
  gap: a future consumer (a per-scope HTTP DELETE, an operator stop, destroy
  wiring) resolves its handle here when consumer-side evidence exists.
- **MCP = server-level group kill + call-level `tool_timeout`.** A stdio
  server dies as a whole group (`kill_gracefully`, `mcp.rs:2611`); an
  individual tool call is bounded by `tool_timeout` (#181). There is no
  per-call task-table registration: the `CancelKind::McpServer` variant is
  declared (`src/process.rs:65`) but has **zero construction sites**
  repo-wide. Disposed by #200 / Goal 427 (ratified option (a)): the variant
  is **kept as an experimental-reserved, deliberately unwired arm** — a
  recorded status quo, not a gap; wire it (`spawn_keyed`, keyed by server
  name) only when an MCP-host consumer appears — see
  [channels.md](channels.md) §10.

### 4.2 ACP cancel is cooperative at the step boundary — read this before consuming it

A `session/cancel` fires the addressed session's current-turn token
(`TurnControl::cancel_session`, `server.rs:208`; the notification form
without an id targets the most recently prompted session). The token has
exactly two consumption points: the **between-step probe** `check_shutdown`
(`run_core.rs:934`) — the current step's tool call runs to completion or to
its own timeout, then the loop ends with `FinishReason::Cancelled` instead of
making the next LLM call — and the **in-flight LLM stream**, which receives
the token (`run_core.rs:1260`) so a mid-stream cancel becomes
`Error::Cancelled` → `make_cancelled_outcome` (`run_core.rs:1082`) with the
partial transcript persisted. The turn task itself is never aborted
(`server.rs:1626-1676`). **Do not read this as "cancel immediately kills the
in-flight tool"**: a `Bash` running when the cancel lands finishes naturally
or hits its own timeout; its process group is owned by the exec/background
contracts (§1), not by the ACP token.

### 4.3 Audited and deliberately unchanged (not missed)

The Goal 412 site audit covered every spawn / termination / destroy point;
these were left alone **on purpose** (journal
`.dev/journal/manual-20261018-goal412-process-contract.md` and the
`src/process.rs` module doc):

- **One-shot `Command::output()` calls** (checkpoint, deliverables, rewind,
  AG-UI, CLI control): spawned and reaped inside std — no handle to leak.
- **External hook children** (`src/hooks/external.rs:826`, `:870`):
  short-lived, but the timeout arm cannot signal them — `wait_with_output`
  consumes the child handle, so reaching the group would require
  restructuring that loop. Documented in the module doc; revisit only with a
  real incident.
- **Container / e2b / SSH-*remote* commands** (`container_transport.rs`,
  e2b): the remote side has no local child to own; the environment is owned
  by `ToolTransport::destroy`, which the destroy chain calls (contract 3).
- **ACP/session child *kill* helpers** (`src/acp/session.rs:181-235`,
  `src/session/lifecycle.rs:129`): #200 experimental surface; functional ACP
  wiring beyond the step-boundary token is out of scope for the contract.

### 4.4 Honest boundaries

- **Tenancy is deliberately absent** (owner ruling 2026-10-18
  「运行时租户透明」): cancel scopes are opaque strings supplied at the edge (a job-table
  id, the workspace root); the kernel and `TurnContext` carry no identity
  field — that is design, not a gap. Session ownership / tenant keys belong
  to #146/#152 (deferred); this page deliberately does not absorb them.
- **Production-channel tiering is #200**, an open issue: which channels get
  functional cancel wiring, and what the downgrade surface looks like, is
  decided there — it is also the basis for §4.1's ratifications. #179 already
  crossed the experimental line for ACP; that is noted here, not
  re-litigated.
- **Windows**: everything above is POSIX-shaped; the Windows fallback
  (`start_kill`, no groups) is the documented contract, and the #224 policy
  (`cfg_attr(windows, ignore)` per assertion, each stating its mechanism and
  the condition for lifting it) applies to the tests.
- This page describes landed code as of goal 425 (2026-10-24, baseline
  `584f2f63`). It is a map, not a specification to re-implement against:
  `src/process.rs` and the `test_process_contract_*` tests remain the single
  authority.

## Related Concepts

- [Execution Environments](execution-environments.md) — the sandbox tiers the channels execute in; teardown order
- [Shell Tool](tools/shell.md) — the `Bash` tool whose exec this contract owns
- [Sessions](sessions.md) — teardown paths (DELETE / idle eviction / graceful shutdown) and the resume orphan policy
- [Context Compaction](compaction.md) — the sibling contract page for transcript semantics

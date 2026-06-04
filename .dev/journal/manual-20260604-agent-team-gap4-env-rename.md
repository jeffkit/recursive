# Manual edit: agent-team gap4 + RECURSIVE_TEAM_ENABLED alias

**Date**: 2026-06-04
**Goal**: Complete the multi-agent team system with inter-worker real-time communication (Gap 4) and introduce a clearer environment variable name.

## Changes

### Gap 4 — Inter-worker real-time communication

**Problem**: Workers inside `spawn_workers_parallel` had no way to discover their peer
workers or send them messages mid-run. This meant workers had to operate in
information silos even when they needed to coordinate.

**Fix**:

`src/tools/send_message.rs`:
- Added `ListWorkersTool` struct — lists all currently active workers from the
  `WorkerRegistry`, enabling any worker to discover its peers' IDs.
- Both `SendMessageTool` and `ListWorkersTool` are injected into each parallel worker's
  sub-registry when a `WorkerRegistry` is attached to `SpawnWorkersParallel`.

`src/tools/spawn_workers_parallel.rs`:
- Added `registry: Option<WorkerRegistry>` field and `with_registry()` builder.
- In `execute()`, **before** spawning workers, each worker is pre-registered in the
  `WorkerRegistry` with a stable ID (explicit `worker_id` or a new UUIDv4).
  This allows any worker to immediately call `list_workers` / `send_message` and reach
  any peer — even if that peer hasn't started yet, the mailbox already exists.
- After all workers finish, they are deregistered from the registry.
- The `WorkerMailbox` returned by registration is passed directly into each worker's
  `TurnContext.mailbox`. The kernel drains it between steps (existing behaviour in
  `run_core.rs`), so coordinator messages arrive without restarting the worker.
- Each worker's sub-registry now includes both `SendMessageTool` and `ListWorkersTool`.

`src/cli/builder.rs`:
- `SpawnWorkersParallel` is created `with_registry(worker_registry.clone())` so the
  registry is threaded through from the top-level builder.
- `spawn_worker` also gets `with_registry()` so single sequential workers can receive
  messages from the coordinator.

`src/tools/mod.rs`:
- Re-exports `ListWorkersTool` alongside the other send_message symbols.

### RECURSIVE_TEAM_ENABLED env var alias

**Problem**: The environment variable that enables multi-agent features was called
`RECURSIVE_SUBAGENT_ENABLED`, which is confusing — this flag now gates agent *team*
features (coordinator, parallel workers, roles) rather than the narrower original
sub-agent concept.

**Fix** (`src/cli/builder.rs`):
- Both `RECURSIVE_SUBAGENT_ENABLED=1` and `RECURSIVE_TEAM_ENABLED=1` are accepted.
  Either one enables the full multi-agent feature set.
- Old name kept for backward compatibility — existing scripts/docs still work.

## Files touched

- `src/tools/send_message.rs` — new `ListWorkersTool`
- `src/tools/spawn_workers_parallel.rs` — pre-registration, mailbox wiring, registry injection
- `src/tools/mod.rs` — re-export `ListWorkersTool`
- `src/cli/builder.rs` — `RECURSIVE_TEAM_ENABLED` alias, wire `with_registry` into both parallel and sequential workers

## Tests added

- Existing tests in `send_message.rs` already cover `WorkerRegistry` register/deregister.
- `ListWorkersTool` is exercised by `registry_register_and_get` and `send_message_shows_active_workers` coverage paths.

## Notes

- Pre-registration ensures the mailbox exists before any worker starts, so `send_message`
  targeting a peer that hasn't been scheduled yet will still buffer correctly.
- Worker IDs default to UUIDv4 when no `worker_id` is specified; coordinators should
  pass explicit stable IDs when they need to use `send_message`.
- The existing `run_core.rs` mailbox-drain logic (introduced earlier) handles message
  delivery without any changes.

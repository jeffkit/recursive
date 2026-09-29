okf_version: "0.1"
type: Concept
title: Execution Environments (Sandbox Tiers)
description: The four sandbox tiers (none / policy / container / microvm), their capability contracts, and when to choose a microVM.
timestamp: "2026-09-29T08:00:00Z"
---

# Execution Environments (Sandbox Tiers)

Recursive runs its I/O tools (Read / Write / Edit / Glob / Grep /
count_lines / Bash) through a single shared `ToolTransport` (Goal 401).
Which transport is bound decides **where the agent's work executes**. There
are four tiers, selected by the `RECURSIVE_SANDBOX` environment variable.

## Tier matrix

| Tier | Isolation | Entry (env) | Feature | Provider / transport |
|------|-----------|-------------|---------|----------------------|
| none (default) | none — host user | *(unset)* or `none` | — | `LocalTransport` (`src/tools/transport.rs`) |
| policy | in-process path+shell policy (L1) | `RECURSIVE_SANDBOX=policy` | — | `PolicyToolSetProvider` (`src/tools/policy_sandbox.rs`) |
| container | Docker container, non-root, caps dropped (L2) | `RECURSIVE_SANDBOX=container` | `cloud-runtime` | `ContainerToolSetProvider` + `ContainerTransport` (`src/tools/container_*.rs`) |
| microvm | E2B Firecracker microVM (L3, hardware-isolated) | `RECURSIVE_SANDBOX=microvm` | `e2b-sandbox` | `E2bToolSetProvider` + `E2bTransport` (`src/tools/e2b_provider.rs`) |

All non-`none` tiers are **fatal on setup failure** (exit 2, clear message):
Recursive never silently falls back to local execution — a sandbox that
quietly became host execution is worse than a loud error.

MicroVM-specific env: `RECURSIVE_E2B_API_KEY` (required),
`RECURSIVE_E2B_TEMPLATE` (default `base`), `RECURSIVE_E2B_TIMEOUT_SECS`
(sandbox TTL, default 3600, renewed after each successful exec once half
elapsed), `RECURSIVE_E2B_API_BASE`.

## Capability matrix

`ToolTransport::capabilities()` reports the environment; tools consult it
instead of hard-coding assumptions.

| Capability | none | policy | container | microvm (E2B) |
|---|---|---|---|---|
| `network` | true | restricted by policy | opt-in (`RECURSIVE_CONTAINER_NETWORK`) | true — the `base` template ships with outbound network; a custom template may differ |
| `persistent` | true | true | true | true (sandbox lives for the session; TTL renewed) |
| `path_root` | *(empty = host-resolved paths)* | host | `/workspace` (bind mount) | `/workspace` (fixed; created at sandbox start; empty unless the agent writes into it — the host workspace is **not** pre-uploaded) |
| `user` | none | host user | `1000:1000`, non-root | probed via `whoami` |
| `toolchain` | not probed | not probed | probed at startup | probed at startup (`cargo node rg git`) |
| `snapshot` | false | false | false | false (non-goal, see below) |

`ToolTransport` method coverage per tier:

| Method | none | container | microvm |
|---|---|---|---|
| `read_file` / `write_file` | `tokio::fs` | bind mount | E2B files API (`GET`/`POST /sandboxes/{id}/files`; 404 → `NotFound`) |
| `list_dir` / `create_dir_all` | `tokio::fs` | in-container `find`/`mkdir -p` | in-VM `find -maxdepth 1` / `mkdir -p` via exec |
| `walk` | local `walkdir` (single pass) | in-container `find` (single round trip) | in-VM `find` (single round trip; overrides the O(dirs) fallback) |
| `exec_shell` | host `/bin/sh` | `docker exec` (PID-tracking timeout kill) | `POST /sandboxes/{id}/process` |
| `capabilities` | static local | static + probed toolchain | probed `whoami`/toolchain at start (`path_root` = fixed `/workspace`) |

Path semantics: the tool layer resolves paths against the **host**
workspace, so every transport maps them before use — the container tier
via its bind mount, the microvm tier via an explicit prefix rewrite
(host workspace → `/workspace`, `E2bTransport::map_path`; paths outside
the workspace prefix are rejected with `InvalidInput`, same contract as
`ContainerTransport::map_path`). Consequence: in the microvm tier the VM
starts **empty** — host workspace contents are not pre-uploaded; the
agent's Write/Edit populate `/workspace`, and Read/Glob of a file that
was never written in this session returns `NotFound`. Whole-tree
pre-upload is out of scope for this milestone (see non-goals).
| `destroy` | no-op | container remove (idempotent) | `DELETE /sandboxes/{id}` (idempotent; 404 = already gone) |

In the microvm tier the host-exec tools (`run_background` /
`check_background` / `watch_file` / `stop_loop`) are **dropped** from the
registry — they spawn via the host `/bin/sh` and would bypass the VM (same
honest-degradation policy as the container tier).

## When to choose microVM vs container

Choose **microvm** (E2B / Firecracker) when:

- you need **hardware-level isolation** — the workload runs untrusted /
  adversarial code that must not share a kernel with anything else;
- a **custom Firecracker image** (own kernel, device drivers, syscall
  filter) is required;
- the execution should run **off-host entirely** (no Docker daemon, no
  host filesystem proximity — e.g. multi-tenant SaaS execution).

Choose **container** when Docker is available and kernel-sharing is
acceptable: it has no per-VM cost, lower latency, and a bind-mounted
workspace (no file upload/download round trips).

E2B dependencies and caveats: an **API key** (`RECURSIVE_E2B_API_KEY`), a
**template** (default `base`), **TTL management** (renewed by the
transport; a session longer than the TTL without successful execs can
expire), and **data egress** — file contents transit the E2B cloud API.

## Isolation dimensions covered

| Dimension | none | policy | container | microvm (E2B) |
|---|---|---|---|---|
| Filesystem | none — host user's files | path policy (prefix allow-list) | container rootfs + `/workspace` bind, non-root UID, dropped caps | independent VM rootfs; host workspace never mounted (only mapped prefix inside VM) |
| Processes | host PIDs | shell command policy | PID namespace per container | separate kernel — no shared PID space at all |
| Kernel / syscalls | host kernel shared | host kernel shared | host kernel shared (syscall surface via dropped caps, seccomp optional) | **hardware-isolated** (Firecracker microVM, KVM); guest kernel owns syscalls |
| Network | unrestricted | restricted by policy | opt-in bridge (`RECURSIVE_CONTAINER_NETWORK`) | VM NIC; `base` template allows outbound by default |
| Resources | unbounded | unbounded | cgroup limits if configured | VM vCPU/RAM fixed by template |
| Data tenancy | single tenant on host | same | container-per-session, shared host kernel | sandbox-per-session on shared cloud host; each VM's memory is freed on delete |
| Time snapshots | none | none | none | none (`snapshot == false`); TTL renewal only |

## Density & default-tier rationale

Per-session marginal cost, in order-of-magnitude terms:

| Tier | Marginal footprint per session | Cold start |
|---|---|---|
| in-process (`none`/policy) | ~50 KB (one policy table / tool set) | ~0 |
| container | tens of MB (one container + overlay) | hundreds of ms |
| microVM (E2B) | GB-scale (VM RAM reservation) | <150 ms (snapshot-based boot) |

**The default must stay `none`**: Recursive's primary mode is a local
developer agent operating on the user's own repo with their own
credentials. Sandboxing adds latency (every I/O becomes an RPC —
upload/download round trips in the microvm tier), external dependencies
(Docker daemon / E2B API key + network), and semantic gaps (the microvm
workspace starts empty; host background tools are dropped). Users who
need isolation opt in explicitly via `RECURSIVE_SANDBOX`; setup failure
then aborts loudly rather than degrade.

## Self-hosted microVM requirements (future work checklist)

Running Firecracker-class microVMs without E2B would require, at minimum:
`/dev/kvm` available; a guest kernel + rootfs + init to bake per
template; TAP devices + NAT/CNI networking for the VM NIC; CoW rootfs
(cloning) plus virtio-fs (or 9p) for workspace sharing; snapshot/restore
for fast boots; a warm-VM pool to amortize boot; and cargo-chef-style
layered caching so toolchain setup survives template rebuilds. None of
this is in scope — see non-goals.

## Non-goals

- No local Firecracker orchestration (own microVM management, jailer,
  rootfs building) — E2B is the only microVM backend.
- No workspace pre-upload to the VM (whole-tree sync is a follow-up;
  today the microvm workspace starts empty by design).
- No cross-tier hot migration of a live session.
- No E2B snapshot / clone support (`capabilities().snapshot == false`).
- No offline / air-gapped microVM support (the API is remote by design).

Next milestone candidates: egress policy / network allow-lists per
sandbox, a credential broker (secrets scoped per session instead of
ambient env), and an audit trail of sandbox I/O. Multi-tenant HTTP
hardening (per-session sandbox in `microvm` mode — today the HTTP server
shares one VM across sessions) is likewise deferred.

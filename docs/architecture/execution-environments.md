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

> **⚠️ Egress warning (microvm tier).** The default E2B `base` template
> **allows outbound network**. Because E2B is a managed service, the host
> cannot toggle the VM NIC — `RECURSIVE_SANDBOX_NETWORK=on` is only a
> host-side startup acknowledgment gate (the provider refuses to start
> without it), **not network isolation**. For untrusted or adversarial
> workloads, build a custom no-egress template and point
> `RECURSIVE_E2B_TEMPLATE` at it. Untrusted workloads must also not run in
> the `none`/`policy` tiers (see Threat Model below).

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

> **⚠️ Egress warning (microvm tier).** The default E2B `base` template
> **allows outbound network**. Because E2B is a managed service, the host
> cannot toggle the VM NIC — `RECURSIVE_SANDBOX_NETWORK=on` is only a
> host-side startup acknowledgment gate (the provider refuses to start
> without it), **not network isolation**. For untrusted or adversarial
> workloads, build a custom no-egress template and point
> `RECURSIVE_E2B_TEMPLATE` at it. Untrusted workloads must also not run in
> the `none`/`policy` tiers (see Threat Model below).

## Capability matrix

`ToolTransport::capabilities()` reports the environment; tools consult it
instead of hard-coding assumptions.

| Capability | none | policy | container | microvm (E2B) |
|---|---|---|---|---|
| `network` | true | restricted by policy | opt-in (`RECURSIVE_SANDBOX_NETWORK`) | opt-in via `RECURSIVE_SANDBOX_NETWORK` (host-side startup gate; the `base` template itself has outbound — use a custom no-egress template for untrusted workloads) |
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
| Network | unrestricted | restricted by policy | opt-in bridge (`RECURSIVE_SANDBOX_NETWORK`) | VM NIC; the `base` template allows outbound — gated behind the host-side `RECURSIVE_SANDBOX_NETWORK=on` startup acknowledgment; a custom template owns its own egress decision |
| Resources | unbounded | unbounded | cgroup limits if configured | VM vCPU/RAM fixed by template |
| Data tenancy | single tenant on host | same | container-per-session, shared host kernel | sandbox-per-session on shared cloud host; each VM's memory is freed on delete |
| Time snapshots | none | none | none | none (`snapshot == false`); TTL renewal only |

## Threat Model

Per tier, **who does this tier defend against — and who does it not?**

- **`none`**: defends against nobody. The agent runs with the full
  authority of the host user. Only suitable for trusted code **and**
  trusted input (a prompt-injected command deletes real files).
- **`policy`**: defends against the agent's *mistakes* (wrong-path
  deletes, disallowed commands) via path/command allow-lists. Does **not**
  defend against malicious code: same kernel, same user, no syscall
  restriction.
- **`container`**: defends against code inside the container escaping to
  the ordinary container boundary (non-root UID, dropped capabilities,
  no-new-privs, default `net none`). Does **not** defend against kernel
  privilege escalation (the host kernel is shared) or outbound abuse once
  `RECURSIVE_SANDBOX_NETWORK=on` opts in.
- **`microvm`**: defends against **arbitrary malicious code inside the
  guest** (hardware isolation, independent kernel). Does **not** defend
  against: ① trust in the E2B cloud / supply chain (file contents and
  commands transit the E2B API); ② the `base` template's outbound network
  (an exfiltration channel — the host-side gate only acknowledges it); ③
  the host-side tool channels listed below.

### Host-side tools vs the sandbox boundary

The sandbox tiers only cover the tools that execute through
`ToolTransport`:

**Inside the boundary (execute via the transport):**
`Read` / `Write` / `Edit` / `Glob` / `Grep` / `count_lines` / `Bash`
(run_shell).

**Outside the boundary (execute on the host — the outbound/entry surface
of a prompt injection):**

- `web_fetch` / `web_search` — arbitrary outbound URL / search query
  (`web_search` is enabled by default via the `web_search` cargo feature).
- `install_skill` / `load_skill` — write/read the host skill directory →
  a persistent injection surface for later sessions.
- `memory` (`remember`/`recall`/`forget`/`update_fact`) — host files.
- `episodic_recall` — host session store.
- `checkpoint` — host git.
- `a2a` / `client_fs` — host-side agent / client channels.
- MCP tools — out-of-process, host network.
- `task_*` / `team_*` — coordinator-mode dispatch.
- `estimate_tokens` — reads host files directly (`tokio::fs::read_to_string`,
  `src/tools/estimate_tokens.rs`) without going through the shared
  transport: a host-side read channel in the container/microvm tiers (its
  `path` argument can point at host files that were never written inside
  the sandbox).

Precedent for honest degradation: under the microvm tier, host-executing
tools such as `run_background` / `watch_file` are **dropped** from the
tool set (see the tier notes above). The remaining host tools listed
above are currently **not** dropped — this is a known accepted risk, see
Non-goals and the next-milestone candidates below.

### Environment-variable invariant

Sandboxed tiers (`policy` / `container` / `microvm`) do **not** inherit
host environment variables in shell exec. `RunShell::execute`
(`src/tools/shell.rs`) starts `env_pairs` as an empty `Vec` and only
appends pairs the tool call explicitly passes in its `env` argument;
`ContainerTransport::exec_shell` (`src/tools/container_transport.rs`)
and `E2bTransport::exec_shell` (`src/tools/e2b_provider.rs`) build the
in-sandbox environment solely from that slice (an explicit `K=V`
prefix). Host credentials such as `RECURSIVE_E2B_API_KEY` therefore
never enter the sandbox. The `none` tier (`LocalTransport`)
**intentionally** inherits the host environment — that is the local
tier's design, not a defect. Regression test:
`tests/issue51_sandbox_env_inheritance.rs`.

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
ambient env), an audit trail of sandbox I/O, and bringing the host-side
tools (web_fetch / memory / MCP / skills) inside the sandbox boundary or
dropping them per tier — the second known gap beyond egress policy (see
Threat Model). Multi-tenant HTTP
hardening (per-session sandbox in `microvm` mode — today the HTTP server
shares one VM across sessions) is likewise deferred.

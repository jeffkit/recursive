# Sandbox Modes

Recursive supports multiple levels of tool execution isolation.

## Overview

| Mode | Isolation | Use case |
|---|---|---|
| `local` | Host process | Development, trusted workloads |
| `policy` | Path + command restrictions | Moderate isolation via allowlists |
| `docker` | Docker container (L2) | Untrusted code, multi-user |
| `e2b` | E2B microVM (L3) | Full VM isolation, cloud execution |

Set the mode via:

```bash
export RECURSIVE_SANDBOX_MODE=docker
```

## local (default)

Tools run directly in the host process. The filesystem sandbox is enforced via `resolve_within`, which rejects path escapes.

```bash
RECURSIVE_SANDBOX_MODE=local
RECURSIVE_WORKSPACE=/path/to/project
```

## policy

Adds an allowlist/denylist layer on top of `local`. Configure permitted paths and shell commands.

```bash
RECURSIVE_SANDBOX_MODE=policy
```

## docker

Each `run_shell` invocation runs inside a fresh Docker container. The workspace is mounted read-write; all other host paths are excluded.

```bash
RECURSIVE_SANDBOX_MODE=docker
```

Requires Docker daemon running on the host.

## e2b

Each agent run gets its own E2B microVM — full Linux sandbox with network access. Best isolation, suitable for untrusted code execution.

```bash
RECURSIVE_SANDBOX_MODE=e2b
RECURSIVE_E2B_API_KEY=your-e2b-api-key
RECURSIVE_E2B_TEMPLATE=base
RECURSIVE_E2B_TIMEOUT_SECS=300
```

Requires an [E2B](https://e2b.dev) account.

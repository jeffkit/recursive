# Deployment Overview

Recursive can be deployed in several configurations:

| Mode | Use case |
|---|---|
| **Local** | Development, single-user, laptop |
| **Docker (single container)** | Small team, self-hosted |
| **Cloud (Redis + S3)** | Multi-user, production, horizontal scaling |

## Local vs cloud feature comparison

| Concern | Local (default) | Cloud (`cloud-runtime` feature) |
|---|---|---|
| Transcript persistence | Local JSONL (`~/.recursive/...`) | S3 |
| Session hot-state | In-memory | Redis |
| Tool execution | Host shell | Docker (L2) or E2B microVM (L3) |
| Horizontal scaling | Single process | Stateless HTTP pods + shared Redis/S3 |
| Resume across restarts | Via `--session` flag | Automatic via storage restore |

## Navigation

- [Docker](./docker) — single-container and compose setups
- [Cloud (Redis + S3)](./cloud) — production deployment
- [Sandbox Modes](./sandbox) — local, policy, docker, e2b

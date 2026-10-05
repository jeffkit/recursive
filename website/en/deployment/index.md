# Deployment Overview

Recursive can be deployed in several configurations:

| Mode | Use case |
|---|---|
| **Local** | Development, single-user, laptop |
| **Docker (single container)** | Small team, self-hosted |
| **Cloud (S3)** | Multi-user, production, horizontal scaling (sticky sessions) |

## Local vs cloud feature comparison

| Concern | Local (default) | Cloud (`cloud-runtime` feature) |
|---|---|---|
| Transcript persistence | Local JSONL (`~/.recursive/...`) | S3 via `S3StorageBackend` when `RECURSIVE_S3_BUCKET` is set; appended per turn, cold-loaded on restart |
| Session hot-state | In-memory (`NoopSessionStore`), owned by the process | Redis via `RedisSessionStore` is library-API only — `recursive http` does not consume it (direction closed) |
| Tool execution | Host shell | Docker (L2) or E2B microVM (L3) |
| Horizontal scaling | Single process | Shared S3 makes transcripts durable and cold-loadable from any replica; in-flight sessions still live on one pod, so route with sticky sessions |
| Resume across restarts | Via `--session` flag | HTTP `GET /sessions/:id` cold-loads from the storage backend |

## Navigation

- [Docker](./docker) — single-container and compose setups
- [Cloud (S3)](./cloud) — production deployment
- [Sandbox Modes](./sandbox) — local, policy, docker, e2b

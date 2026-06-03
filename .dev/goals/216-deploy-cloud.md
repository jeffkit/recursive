# Goal 216 — Docker Compose + .env.example + Cloud Deployment Guide

**Roadmap**: Phase 19 — Ecosystem & Distribution (deployment)
**依赖**: Goal 138 (Dockerfile, 已合并)

**Design principle check**:
- 新增 `docker-compose.yml`、`scripts/localstack-init.sh`、`.env.example`
- 修改 `README.md`，新增 Cloud Deployment Guide 章节
- ❌ 不修改 `src/` 任何 Rust 源码
- ❌ 不新增 Cargo 依赖

## Why

Goal 138 已经提供了 Dockerfile 用于单机容器化部署，但缺少：

1. **多服务编排**：用户希望本地起 Redis（hot session store）+ S3（cold blob store）+ LocalStack（AWS mock）+ Recursive 容器，一条 `docker compose up` 全部拉起。
2. **环境变量集中管理**：所有 env var 在 `.env.example` 里集中说明，开发者直接 `cp .env.example .env` 即可。
3. **部署文档**：`README.md` 需要一个可复制的 Cloud Deployment 章节，覆盖 AWS / GCP / 自托管 K8s 路径。

## Scope

### 1. `docker-compose.yml`（新增）

4 个 service：

| Service | Image | Port | 说明 |
|---------|-------|------|------|
| `recursive` | 本地 build | 8080 | 主服务 |
| `redis` | `redis:7-alpine` | 6379 | Session hot store |
| `minio` | `minio/minio` | 9000/9001 | S3-compatible blob store |
| `localstack` | `localstack/localstack` | 4566 | AWS mock（可选 profile） |

`recursive` service 通过 `depends_on` 等待 redis/minio ready，并通过 `env_file: .env` 加载配置。

### 2. `.env.example`（新增）

完整 ENV var 参考表，分三段：

- **LLM**：`RECURSIVE_API_KEY`、`RECURSIVE_API_BASE`、`RECURSIVE_MODEL`
- **HTTP**：`RECURSIVE_HTTP_PORT`、`RECURSIVE_HTTP_HOST`、`RECURSIVE_AUTH_*`
- **Storage**：`RECURSIVE_REDIS_URL`、`RECURSIVE_S3_BUCKET`、`RECURSIVE_S3_ENDPOINT`、`RECURSIVE_SANDBOX_*`

每行带 inline 注释，说明用途和合法值。

### 3. `scripts/localstack-init.sh`（新增）

启动时创建 LocalStack S3 bucket（`recursive-prod`）和 SSM 参数。给 `docker-compose.yml` 的 `localstack` service 用作 entrypoint。

### 4. `README.md` — Cloud Deployment Guide 章节

新增章节，覆盖：

- **Local Docker Compose**：`docker compose up -d` 起步
- **AWS ECS / Fargate**：task definition 模板、IAM role、SSM 参数引用
- **Self-hosted Kubernetes**：Deployment + Service + Ingress YAML 片段
- **GCP Cloud Run**：env var 配置、Secret Manager 引用
- Local-vs-cloud 配置 cheatsheet

## 验收标准

- `docker compose config` 验证 yml 合法
- `shellcheck scripts/localstack-init.sh` 无错误
- `.env.example` 覆盖 `src/config.rs` / `src/config_file.rs` 中所有可识别 ENV var
- README Cloud Deployment 章节的所有命令可直接复制运行
- ❌ 不破坏现有 `cargo test --workspace`

## Notes

- 这是开发/部署基础设施，不影响 product code 的运行时行为
- 与 Goal 138（Dockerfile）互补：本 goal 解决"多服务编排"，Goal 138 解决"单容器打包"
- 远程引用 `origin/feat/phase19-sdk-ecosystem` 上有重复工作（commit `dbc1b4e`），但已通过 cherry-pick 进入 main，本 goal 是独立路径

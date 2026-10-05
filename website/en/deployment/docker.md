# Docker Deployment

## Single container

```bash
docker build -t recursive:dev --target runtime .
docker run -p 3000:3000 \
  -e RECURSIVE_API_KEY="$OPENAI_API_KEY" \
  -e RECURSIVE_API_BASE="https://api.openai.com/v1" \
  -e RECURSIVE_MODEL="gpt-4o-mini" \
  recursive:dev
```

The image defaults to `recursive http --addr 0.0.0.0:3000` and exposes `/health` for probes.

## Docker Compose (S3)

```bash
cp .env.example .env    # fill in RECURSIVE_API_KEY
docker compose up
```

The bundled `docker-compose.yml` spins up:
- **recursive** — the HTTP API server (port 3000), built with
  `--build-arg FEATURES=http,cloud-runtime`
- **localstack** — S3-compatible transcript persistence (for local development)

Redis is intentionally not provisioned — `recursive http` does not consume a
`RedisSessionStore` (see `cloud.md`).

## Environment variables

Set these in your `.env` or Docker run command:

```bash
RECURSIVE_API_KEY=sk-...
RECURSIVE_API_BASE=https://api.openai.com/v1
RECURSIVE_MODEL=gpt-4o-mini
RECURSIVE_HTTP_ADDR=0.0.0.0:3000
```

For cloud storage, build the image with the `cloud-runtime` feature and set:

```bash
RECURSIVE_S3_BUCKET=my-recursive-bucket
AWS_ACCESS_KEY_ID=...
AWS_SECRET_ACCESS_KEY=...
AWS_DEFAULT_REGION=us-east-1
```

`RECURSIVE_S3_BUCKET` selects the S3 transcript backend, which the runtime
appends to per turn. `RECURSIVE_REDIS_URL` is not consumed by `recursive http`
and is ignored (a note is logged).

## Health probe

```bash
curl http://localhost:3000/health
# → "ok"
```

Use this as the Docker health check:

```yaml
healthcheck:
  test: ["CMD", "curl", "-f", "http://localhost:3000/health"]
  interval: 30s
  timeout: 5s
  retries: 3
```

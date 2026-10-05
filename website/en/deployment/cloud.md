# Cloud Deployment (S3 + Redis)

For production deployments with multiple users and horizontal scaling.

## Requirements

Build with the `cloud-runtime` feature:

```bash
cargo build --release --features cloud-runtime
```

The bundled Dockerfile builds `recursive http` with `http` only; pass the
feature explicitly to enable the cloud backends:

```bash
docker build -t recursive:dev --target runtime --build-arg FEATURES=http,cloud-runtime .
```

## Redis (session hot-state)

`RedisSessionStore` is implemented and integration-tested, but **`recursive http`
does not consume it yet**: the kernel owns the session-store injection point and
never checkpoints per turn, so the HTTP server keeps `NoopSessionStore` and only
logs a note when `RECURSIVE_REDIS_URL` is set. In-flight sessions live in the
process — route them with sticky sessions. Exposing Redis as a shared session
table is future work.

```bash
RECURSIVE_REDIS_URL=redis://your-redis-host:6379
RECURSIVE_REDIS_KEY_PREFIX=recursive:    # optional namespace
RECURSIVE_REDIS_SESSION_TTL_SECS=7200    # 2 hours default
```

## S3 (transcript persistence)

When `RECURSIVE_S3_BUCKET` is set, `recursive http` stores full conversation
transcripts, memory entries and per-session metadata in S3, and `GET
/sessions/:id` cold-loads them on a memory miss — so a session torn down on one
pod is visible to a sibling replica sharing the bucket. Transcripts are written
on session teardown (DELETE / idle eviction / graceful shutdown) only, so a
hard-killed pod can lose the turns since its last save; there is no per-turn
write.

```bash
RECURSIVE_S3_BUCKET=my-recursive-bucket
RECURSIVE_S3_PREFIX=recursive
RECURSIVE_S3_TENANT_ID=default          # multi-tenant namespace
AWS_DEFAULT_REGION=us-east-1
```

For LocalStack (local S3 emulation):

```bash
AWS_ENDPOINT_URL=http://localhost:4566
AWS_ACCESS_KEY_ID=test
AWS_SECRET_ACCESS_KEY=test
```

## Kubernetes example

`replicas: 3` is only safe with sticky routing (e.g. a session-affinity
`Service`/ingress): a session is served by the pod that created it until it is
torn down and cold-loaded from S3 elsewhere. Without affinity, round-robin
traffic returns 404 for sessions held in another pod's memory.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: recursive
spec:
  replicas: 3
  selector:
    matchLabels:
      app: recursive
  template:
    metadata:
      labels:
        app: recursive
    spec:
      containers:
      - name: recursive
        image: ghcr.io/jeffkit/recursive:latest
        ports:
        - containerPort: 3000
        env:
        - name: RECURSIVE_API_KEY
          valueFrom:
            secretKeyRef:
              name: recursive-secrets
              key: api-key
        - name: RECURSIVE_REDIS_URL
          value: redis://redis-service:6379
        - name: RECURSIVE_S3_BUCKET
          value: my-recursive-bucket
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
```

## Multi-tenancy

Use `RECURSIVE_S3_TENANT_ID` to namespace data per tenant. Each tenant's transcripts and memory are isolated under `s3://bucket/prefix/tenant_id/`.

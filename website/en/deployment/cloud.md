# Cloud Deployment (S3)

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

## Redis (session hot-state) — not used

`RedisSessionStore` is implemented and integration-tested, but **`recursive http`
does not consume it**: the kernel owns the session-store injection point and
never checkpoints per turn, so the HTTP server keeps `NoopSessionStore`. Setting
`RECURSIVE_REDIS_URL` only logs a note. This direction is closed for the HTTP
server: per-turn S3 transcripts plus cold load already cover crash recovery, so
a shared Redis session table would be redundant. `RedisSessionStore` remains
available through the library API (`AgentRuntimeBuilder::session_store`).

## S3 (transcript persistence)

When `RECURSIVE_S3_BUCKET` is set, `recursive http` stores conversation
transcripts, memory entries and per-session metadata in S3, and `GET
/sessions/:id` cold-loads them on a memory miss — so a session from one pod is
visible to a sibling replica sharing the bucket. The runtime **persists each
turn's growth** to the transcript (issue #92), so a hard-killed or OOM-killed
pod loses at most the in-flight turn; a turn that rewrote the transcript
(compaction, microcompact pruning) is resynced with a full save instead of
being appended to. Session teardown (DELETE / idle eviction / graceful
shutdown) still performs a final full save.

S3 objects cannot be appended to, so `S3StorageBackend` takes the trait's
load-extend-save fallback: every turn rewrites the whole object (a full `GET` +
`PUT` whose size grows with the transcript), unlike `LocalStorageBackend`, which
appends in place. On S3 the per-turn write is therefore a cost to weigh against
the crash-recovery window; batching or a log-segmented layout is the fix if it
becomes the bottleneck.

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

`replicas: 3` needs sticky routing (e.g. a session-affinity `Service`/ingress)
because in-flight sessions live in the creating pod's memory. Shared S3 makes
the transcript durable and cold-loadable elsewhere, but cold load only happens
on a memory miss: without affinity, round-robin traffic returns 404 for
sessions held in another pod. Once a session is cold-loaded elsewhere it keeps
working there.

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
        - name: RECURSIVE_S3_BUCKET
          value: my-recursive-bucket
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
```

## Multi-tenancy

Use `RECURSIVE_S3_TENANT_ID` to namespace data per tenant. Each tenant's transcripts and memory are isolated under `s3://bucket/prefix/tenant_id/`.

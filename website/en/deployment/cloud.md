# Cloud Deployment (Redis + S3)

For production deployments with multiple users and horizontal scaling.

## Requirements

Build with the `cloud-runtime` feature:

```bash
cargo build --release --features cloud-runtime
```

Or use the Docker image (cloud-runtime is included).

## Redis (session hot-state)

Sessions are stored in Redis for fast access and cross-pod sharing.

```bash
RECURSIVE_REDIS_URL=redis://your-redis-host:6379
RECURSIVE_REDIS_KEY_PREFIX=recursive:    # optional namespace
RECURSIVE_REDIS_SESSION_TTL_SECS=7200    # 2 hours default
```

Sessions automatically expire after TTL. Extend the TTL on each access.

## S3 (transcript persistence)

Full conversation transcripts and memory are stored in S3.

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

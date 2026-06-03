# 云端部署（Redis + S3）

适用于多用户生产环境和水平扩展场景。

## 要求

使用 `cloud-runtime` feature 构建：

```bash
cargo build --release --features cloud-runtime
```

## Redis（会话热态）

```bash
RECURSIVE_REDIS_URL=redis://your-redis-host:6379
RECURSIVE_REDIS_KEY_PREFIX=recursive:
RECURSIVE_REDIS_SESSION_TTL_SECS=7200
```

## S3（对话记录持久化）

```bash
RECURSIVE_S3_BUCKET=my-recursive-bucket
RECURSIVE_S3_PREFIX=recursive
RECURSIVE_S3_TENANT_ID=default
AWS_DEFAULT_REGION=us-east-1
```

## 多租户

使用 `RECURSIVE_S3_TENANT_ID` 为每个租户隔离数据。每个租户的对话记录和内存存储在 `s3://bucket/prefix/tenant_id/` 下。

## Kubernetes 示例

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
    spec:
      containers:
      - name: recursive
        image: ghcr.io/jeffkit/recursive:latest
        ports:
        - containerPort: 3000
        env:
        - name: RECURSIVE_REDIS_URL
          value: redis://redis-service:6379
        - name: RECURSIVE_S3_BUCKET
          value: my-recursive-bucket
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
```

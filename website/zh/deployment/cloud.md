# 云端部署（S3 + Redis）

适用于多用户生产环境和水平扩展场景。

## 要求

使用 `cloud-runtime` feature 构建：

```bash
cargo build --release --features cloud-runtime
```

仓库自带的 Dockerfile 只给 `recursive http` 编 `http` feature，需要显式打开云后端：

```bash
docker build -t recursive:dev --target runtime --build-arg FEATURES=http,cloud-runtime .
```

## Redis（会话热态）

`RedisSessionStore` 已实现并有集成测试，但 **`recursive http` 目前不消费它**：内核持有
session-store 注入点但从不逐轮 checkpoint，所以 HTTP 侧仍是 `NoopSessionStore`，仅在
`RECURSIVE_REDIS_URL` 被设置时打一条日志。存活会话仍在进程内存里，多副本必须用 sticky
session 路由。把 Redis 作为共享会话表是后续工作。

```bash
RECURSIVE_REDIS_URL=redis://your-redis-host:6379
RECURSIVE_REDIS_KEY_PREFIX=recursive:
RECURSIVE_REDIS_SESSION_TTL_SECS=7200
```

## S3（对话记录持久化）

设置 `RECURSIVE_S3_BUCKET` 后，`recursive http` 把完整对话记录、内存条目与逐会话元数据
写入 S3，`GET /sessions/:id` 在内存 miss 时冷加载——在某个副本上被拆除的会话，可被共享
同一 bucket 的兄弟副本读回。写盘只发生在会话结束路径（DELETE / 空闲驱逐 / 优雅关停），
被硬杀（SIGKILL）的 pod 会丢失上次落盘之后的轮次；没有逐轮写。

```bash
RECURSIVE_S3_BUCKET=my-recursive-bucket
RECURSIVE_S3_PREFIX=recursive
RECURSIVE_S3_TENANT_ID=default
AWS_DEFAULT_REGION=us-east-1
```

## 多租户

使用 `RECURSIVE_S3_TENANT_ID` 为每个租户隔离数据。每个租户的对话记录和内存存储在 `s3://bucket/prefix/tenant_id/` 下。

## Kubernetes 示例

`replicas: 3` 只有在 sticky 路由（如 session-affinity Service/ingress）下才安全：会话由创建它
的 pod 服务，直到被拆除后从 S3 冷加载到别处。没有 affinity 时，轮询会把请求打到不持有该会话
的 pod，返回 404。

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

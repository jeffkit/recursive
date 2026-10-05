# 云端部署（S3）

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

## Redis（会话热态）——不使用

`RedisSessionStore` 已实现并有集成测试，但 **`recursive http` 不消费它**：内核持有
session-store 注入点但从不逐轮 checkpoint，所以 HTTP 侧仍是 `NoopSessionStore`，设置
`RECURSIVE_REDIS_URL` 只会打一条日志。该方向对 HTTP 服务已关闭：逐轮 S3 落盘 + 冷加载
已覆盖崩溃恢复，共享 Redis 会话表是冗余的。`RedisSessionStore` 仍可通过库 API 使用
（`AgentRuntimeBuilder::session_store`）。

## S3（对话记录持久化）

设置 `RECURSIVE_S3_BUCKET` 后，`recursive http` 把对话记录、内存条目与逐会话元数据写入
S3，`GET /sessions/:id` 在内存 miss 时冷加载——某个 pod 上的会话可被共享同一 bucket 的
兄弟副本读回。运行时**逐轮持久化**对话记录增量（issue #92）：被硬杀（SIGKILL）或 OOM 的
pod 最多丢当前进行中的这一轮；某一轮里对话记录被重写（compaction、microcompact 剪裁）
时改用全量 save 重新同步，而不是在其上追加。会话结束路径（DELETE / 空闲驱逐 / 优雅关停）
仍做最后一次全量落盘。

S3 对象不支持追加，`S3StorageBackend` 走 trait 的「读-扩展-写」兜底实现：每一轮都重写整个
对象（一次完整的 `GET` + `PUT`，体积随对话记录增长），与本地后端的原地追加不同。因此在 S3
上，逐轮写入的代价需要与崩溃恢复窗口权衡；若成为瓶颈，应改为批量写入或分段日志布局。

```bash
RECURSIVE_S3_BUCKET=my-recursive-bucket
RECURSIVE_S3_PREFIX=recursive
RECURSIVE_S3_TENANT_ID=default
AWS_DEFAULT_REGION=us-east-1
```

## 多租户

使用 `RECURSIVE_S3_TENANT_ID` 为每个租户隔离数据。每个租户的对话记录和内存存储在 `s3://bucket/prefix/tenant_id/` 下。

## Kubernetes 示例

`replicas: 3` 需要 sticky 路由（如 session-affinity Service/ingress）：存活会话在创建它的
pod 内存里。共享 S3 让对话记录持久、可被别的 pod 冷加载，但冷加载只发生在内存 miss 时；
没有 affinity 时，轮询会把请求打到不持有该会话的 pod，返回 404。会话一旦在别处被冷加载
即可继续服务。

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
        - name: RECURSIVE_S3_BUCKET
          value: my-recursive-bucket
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
```

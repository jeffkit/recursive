# Docker 部署

## 单容器

```bash
docker build -t recursive:dev --target runtime .
docker run -p 3000:3000 \
  -e RECURSIVE_API_KEY="$OPENAI_API_KEY" \
  -e RECURSIVE_API_BASE="https://api.openai.com/v1" \
  -e RECURSIVE_MODEL="gpt-4o-mini" \
  recursive:dev
```

镜像默认执行 `recursive http --addr 0.0.0.0:3000`，暴露 `/health` 健康检查端点。

## Docker Compose（Redis + S3）

```bash
cp .env.example .env    # 填写 RECURSIVE_API_KEY
docker compose up
```

## 健康探针

```bash
curl http://localhost:3000/health
# → "ok"
```

Docker Compose 健康检查配置：

```yaml
healthcheck:
  test: ["CMD", "curl", "-f", "http://localhost:3000/health"]
  interval: 30s
  timeout: 5s
  retries: 3
```

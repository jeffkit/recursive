# HTTP API 概览

Recursive 内置基于 axum 的 HTTP 服务器（`recursive http`），提供带会话和 SSE 流式输出的 REST API。

## 启动服务器

```bash
recursive http --addr 127.0.0.1:3000
```

## 接口汇总

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/health` | 健康检查 |
| `GET` | `/tools` | 列出已注册工具 |
| `POST` | `/run` | 无状态单次运行 |
| `POST` | `/sessions` | 创建会话 |
| `GET` | `/sessions` | 列出会话 |
| `GET` | `/sessions/:id` | 获取会话详情 |
| `DELETE` | `/sessions/:id` | 删除会话 |
| `POST` | `/sessions/:id/run` | 发送消息（SSE 流式） |
| `GET` | `/openapi.json` | OpenAPI 3.0 规范 |

## 快速开始

```bash
# 启动服务
recursive http &

# 健康检查
curl http://localhost:3000/health

# 创建会话
SESSION=$(curl -sX POST http://localhost:3000/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"你是一个有用的 Rust 助手。"}' \
  | jq -r .session_id)

# 运行（流式）
curl -N -X POST http://localhost:3000/sessions/$SESSION/run \
  -H 'Content-Type: application/json' \
  -d '{"message":"列出 /workspace 中的文件"}'
```

# recursive http

启动 HTTP API 服务器。

```bash
recursive http [选项]
```

## 说明

启动基于 axum 的 HTTP 服务器，提供带会话和 SSE 流式输出的 REST API。存活会话在服务器进程内；设置 `RECURSIVE_S3_BUCKET`（配合 `cloud-runtime` feature）可把每轮对话记录增量持久化到共享存储、重启后冷加载，并需用 sticky session 路由——Redis 未被消费。

## 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--addr <addr>` | `127.0.0.1:3000` | 绑定地址 |
| `--workspace <path>` | 当前目录 | 所有会话的文件系统沙箱根目录 |
| `--auth-keys <keys>` | *(开放)* | 逗号分隔的 `X-API-Key` 白名单 |

## 快速开始

```bash
recursive http --addr 127.0.0.1:3000
```

另开终端：

```bash
# 健康检查
curl http://localhost:3000/health

# 创建会话
SESSION=$(curl -sX POST http://localhost:3000/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"你是一个有用的助手。"}' | jq -r .session_id)

# 发送消息（流式）
curl -N http://localhost:3000/sessions/$SESSION/run \
  -H 'Content-Type: application/json' \
  -d '{"message":"列出 /workspace 中的文件"}'
```

## 另见

- [HTTP API 参考](../http-api/) — 完整接口文档
- [部署指南](../deployment/) — Docker、S3

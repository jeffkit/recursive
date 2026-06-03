# Sessions API

会话跨多个请求持久化 Agent 对话记录。每个会话由 UUID 标识。

## 创建会话

```http
POST /sessions
Content-Type: application/json

{
  "system_prompt": "你是一个有用的助手。",
  "workspace": "/path/to/project"
}
```

**响应**：
```json
{
  "session_id": "a1b2c3d4-e5f6-...",
  "created_at": "2024-01-01T00:00:00Z"
}
```

## 列出会话

```http
GET /sessions
```

## 获取会话详情

```http
GET /sessions/:id
```

## 删除会话

```http
DELETE /sessions/:id
```

**响应**：`204 No Content`

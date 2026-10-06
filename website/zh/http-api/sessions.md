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

## 获取会话用量与成本

```http
GET /sessions/:id/usage
```

**响应**：
```json
{
  "session_id": "a1b2c3d4-e5f6-...",
  "model": "deepseek-chat",
  "prompt_tokens": 600,
  "completion_tokens": 40,
  "cache_hit_tokens": 512,
  "cache_miss_tokens": 88,
  "reasoning_tokens": 0,
  "total_tokens": 640,
  "llm_latency_ms": 1200,
  "cost_usd": 0.000025
}
```

该值按会话累计，并在服务重启后仍然保留。`cost_usd` 按每一轮当时所用的模型计价
（无法计价时为 `null`），而 `model` 是后续轮次计费所用的模型——重启后换了模型
不会重算历史成本。

## 删除会话

```http
DELETE /sessions/:id
```

**响应**：`204 No Content`

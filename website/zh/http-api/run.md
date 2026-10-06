# Run 与流式输出

## 无状态运行

无需会话持久化的一次性请求：

```http
POST /run
Content-Type: application/json

{
  "goal": "当前目录有哪些文件？",
  "system_prompt": "你是一个有用的助手。",
  "max_steps": 10
}
```

**响应**（JSON）：
```json
{
  "status": "finished",
  "finish_reason": "NoMoreToolCalls",
  "messages": [...],
  "usage": {
    "total_steps": 2,
    "total_tokens": 640,
    "prompt_tokens": 600,
    "completion_tokens": 40,
    "cache_hit_tokens": 512,
    "cache_miss_tokens": 88,
    "reasoning_tokens": 0,
    "llm_latency_ms": 1200,
    "model": "deepseek-chat",
    "cost_usd": 0.000025
  }
}
```

`usage.cost_usd` 是本次运行折算的美元成本；模型没有价格表时为 `null`。
会话的**累计**用量与成本见 `GET /sessions/:id/usage`。

## 会话运行（SSE 流式）

向已有会话发送消息，并通过 `/sessions/:id/events` 订阅 Server-Sent Events：

```http
POST /sessions/:id/run
Content-Type: application/json

{ "goal": "列出 src/ 的文件" }
```

然后从 `/sessions/:id/events` 消费 SSE 流：

```
event: tool_call
data: {"name":"list_dir","step":1}

event: tool_result
data: {"name":"list_dir","success":true}

event: message
data: {"role":"assistant","content":[{"type":"text","text":"src/ 目录包含..."}]}

event: done
data: {"finish_reason":"NoMoreToolCalls","total_steps":1}
```

## SSE 事件类型

| 事件 | 数据字段 |
|---|---|
| `message` | `{ role, content: ContentBlock[] }` |
| `partial_message` | `{ text, step }` — 流式文本增量 |
| `tool_call` | `{ name, step }` |
| `tool_result` | `{ name, success }` |
| `usage` | `{ input_tokens, output_tokens, cache_hit_tokens, cache_miss_tokens, step }` — 单次 LLM 步骤的用量 |
| `done` | `{ finish_reason, total_steps }` |
| `error` | `{ message }` |
| `plan_proposed` | `{ plan }` — Agent 等待计划审批 |

## 在 JavaScript 中消费 SSE

```javascript
// 首先 POST 触发运行
await fetch('/sessions/abc123/run', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ goal: '列出文件' }),
});

// 然后订阅事件
const evtSource = new EventSource('/sessions/abc123/events');

evtSource.addEventListener('tool_call', (e) => {
  const data = JSON.parse(e.data);
  console.log(`[工具] ${data.name}`);
});

evtSource.addEventListener('message', (e) => {
  const data = JSON.parse(e.data);
  for (const block of data.content) {
    if (block.type === 'text') console.log(block.text);
  }
});

evtSource.addEventListener('done', (e) => {
  const data = JSON.parse(e.data);
  console.log('完成:', data.finish_reason);
  evtSource.close();
});
```

## 在 Python 中消费 SSE

```python
import json, sseclient, requests

# 触发运行
requests.post(
    'http://localhost:3000/sessions/abc123/run',
    json={'goal': '列出文件'},
)

# 订阅事件
resp = requests.get(
    'http://localhost:3000/sessions/abc123/events',
    stream=True,
)
client = sseclient.SSEClient(resp)
for event in client.events():
    data = json.loads(event.data)
    if event.event == 'message':
        for block in data.get('content', []):
            if block.get('type') == 'text':
                print(block['text'])
    elif event.event == 'done':
        print('完成:', data['finish_reason'])
        break
```

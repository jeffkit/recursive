# Run & Streaming

## Stateless run

For one-off requests without session persistence:

```http
POST /run
Content-Type: application/json

{
  "goal": "What files are in the current directory?",
  "system_prompt": "You are a helpful assistant.",
  "max_steps": 10
}
```

**Response** (JSON):
```json
{
  "status": "finished",
  "finish_reason": "NoMoreToolCalls",
  "messages": [...],
  "usage": { "total_steps": 2, "total_tokens": 640 }
}
```

## Session run (SSE streaming)

Send a message to an existing session and subscribe to Server-Sent Events on the `/sessions/:id/events` endpoint:

```http
POST /sessions/:id/run
Content-Type: application/json

{ "goal": "List the files in src/" }
```

Then consume the SSE stream on `/sessions/:id/events`:

```
event: tool_call
data: {"name":"list_dir","step":1}

event: tool_result
data: {"name":"list_dir","success":true}

event: message
data: {"role":"assistant","content":[{"type":"text","text":"The src/ directory contains..."}]}

event: done
data: {"finish_reason":"NoMoreToolCalls","total_steps":1}
```

## SSE event types

| Event | Data fields |
|---|---|
| `message` | `{ role, content: ContentBlock[] }` |
| `partial_message` | `{ text, step }` — streaming text delta |
| `tool_call` | `{ name, step }` |
| `tool_result` | `{ name, success }` |
| `done` | `{ finish_reason, total_steps }` |
| `error` | `{ message }` |
| `plan_proposed` | `{ plan }` — agent awaiting plan approval |

## Consuming SSE in JavaScript

```javascript
// First POST to trigger the run
await fetch('/sessions/abc123/run', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ goal: 'list files' }),
});

// Then subscribe to events
const evtSource = new EventSource('/sessions/abc123/events');

evtSource.addEventListener('tool_call', (e) => {
  const data = JSON.parse(e.data);
  console.log(`[tool] ${data.name}`);
});

evtSource.addEventListener('message', (e) => {
  const data = JSON.parse(e.data);
  for (const block of data.content) {
    if (block.type === 'text') console.log(block.text);
  }
});

evtSource.addEventListener('done', (e) => {
  const data = JSON.parse(e.data);
  console.log('Done:', data.finish_reason);
  evtSource.close();
});
```

## Consuming SSE in Python

```python
import json, sseclient, requests

# Trigger the run
requests.post(
    'http://localhost:3000/sessions/abc123/run',
    json={'goal': 'list files'},
)

# Subscribe to events
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
        print('Done:', data['finish_reason'])
        break
```

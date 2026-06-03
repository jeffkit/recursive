# Run & Streaming

## Stateless run

For one-off requests without session persistence:

```http
POST /run
Content-Type: application/json

{
  "message": "What files are in the current directory?",
  "system_prompt": "You are a helpful assistant.",
  "max_steps": 10
}
```

**Response** (JSON):
```json
{
  "finish_reason": "done",
  "final_message": "The directory contains: src/, tests/, Cargo.toml, README.md",
  "steps": 2,
  "token_usage": { "prompt": 512, "completion": 128, "total": 640 }
}
```

## Session run (SSE streaming)

Send a message to an existing session and receive a stream of Server-Sent Events:

```http
POST /sessions/:id/run
Content-Type: application/json

{
  "message": "List the files in src/"
}
```

**Response** (`text/event-stream`):

```
event: llm_start
data: {"step":1}

event: tool_start
data: {"step":1,"name":"list_dir","args":{"path":"src/"}}

event: tool_end
data: {"step":1,"name":"list_dir","result":"agent.rs\nlib.rs\ntools/\n..."}

event: llm_end
data: {"step":1,"message":"The src/ directory contains..."}

event: done
data: {"finish_reason":"done","final_message":"The src/ directory contains...","steps":1}
```

## Consuming SSE in JavaScript

```javascript
const evtSource = new EventSource('/sessions/abc123/run');

evtSource.addEventListener('tool_start', (e) => {
  const data = JSON.parse(e.data);
  console.log(`[tool] ${data.name}`);
});

evtSource.addEventListener('done', (e) => {
  const data = JSON.parse(e.data);
  console.log(data.final_message);
  evtSource.close();
});
```

## Consuming SSE in Python

```python
import sseclient, requests

resp = requests.post(
    'http://localhost:3000/sessions/abc123/run',
    json={'message': 'list files'},
    stream=True,
)
client = sseclient.SSEClient(resp)
for event in client.events():
    print(event.event, event.data)
```

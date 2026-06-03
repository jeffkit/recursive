# Run 与流式输出

## 无状态运行

```http
POST /run
Content-Type: application/json

{
  "message": "当前目录有哪些文件？",
  "system_prompt": "你是一个有用的助手。",
  "max_steps": 10
}
```

## 会话运行（SSE 流式）

```http
POST /sessions/:id/run
Content-Type: application/json

{
  "message": "列出 src/ 的文件"
}
```

**响应**（`text/event-stream`）：

```
event: tool_start
data: {"step":1,"name":"list_dir","args":{"path":"src/"}}

event: done
data: {"finish_reason":"done","final_message":"src/ 目录包含...","steps":1}
```

## 在 JavaScript 中消费 SSE

```javascript
const evtSource = new EventSource('/sessions/abc123/run');

evtSource.addEventListener('tool_start', (e) => {
  const data = JSON.parse(e.data);
  console.log(`[工具] ${data.name}`);
});

evtSource.addEventListener('done', (e) => {
  const data = JSON.parse(e.data);
  console.log(data.final_message);
  evtSource.close();
});
```

## 在 Python 中消费 SSE

```python
import sseclient, requests

resp = requests.post(
    'http://localhost:3000/sessions/abc123/run',
    json={'message': '列出文件'},
    stream=True,
)
client = sseclient.SSEClient(resp)
for event in client.events():
    print(event.event, event.data)
```

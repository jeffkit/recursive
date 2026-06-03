# Python SDK

Python SDK 为 Recursive HTTP API 提供轻量级客户端。

## 安装

```bash
pip install recursive-client
```

## 快速开始

```python
from recursive_client import RecursiveClient

client = RecursiveClient("http://127.0.0.1:3000")

# 健康检查
print(client.health())  # "ok"

# 无状态运行
result = client.run("列出 src/ 的文件")
print(result.finish_reason)
print(result.final_message)
```

## 会话管理

```python
# 创建会话
session = client.create_session(
    system_prompt="你是一个有用的 Rust 助手。",
    workspace="/path/to/project",
)

# 发送消息
result = session.run("agent.rs 是做什么的？")
print(result.final_message)

# 继续对话
result = session.run("主要入口有哪些？")

# 删除会话
session.delete()
```

## 流式输出

```python
for event in session.run_stream("列出所有工具"):
    if event.type == "tool_start":
        print(f"[工具] {event.data['name']}")
    elif event.type == "done":
        print(event.data['final_message'])
        break
```

## API 参考

### `AgentResult`

| 属性 | 类型 | 说明 |
|---|---|---|
| `finish_reason` | `str` | `"done"`、`"budget_exceeded"`、`"stuck"` 等 |
| `final_message` | `str \| None` | Agent 的最终答案 |
| `steps` | `int` | 已执行的步骤数 |
| `token_usage` | `dict \| None` | `{"prompt": N, "completion": N, "total": N}` |

# Python SDK

Python SDK（`recursive-sdk`）为 Recursive HTTP API 提供高层客户端，兼容 Claude Agent SDK 模式。

::: tip 包名
包在 PyPI 上发布为 `recursive-sdk`。如尚未发布，可从源码安装：
```bash
pip install -e sdk/python   # 在项目根目录执行
```
:::

## 安装

```bash
pip install recursive-sdk
```

## 前置条件

先启动 Recursive HTTP 服务：

```bash
recursive http --addr 127.0.0.1:3000
```

## 快速开始 — 一次性运行

```python
from recursive_sdk import Agent

result = Agent.prompt(
    "列出当前目录中的文件。",
    base_url="http://127.0.0.1:3000",
    max_steps=5,
)

print("状态        :", result.status)
print("finish_reason:", result.finish_reason)
if result.result:
    print("答案        :", result.result)
```

## 多轮会话

```python
from recursive_sdk import Agent

with Agent.create(base_url="http://127.0.0.1:3000") as agent:
    print("session:", agent.session_id)

    # 第一轮
    run = agent.send("agent.rs 是做什么的？")
    for msg in run.messages():
        if msg.type == "assistant":
            print(msg.text(), end="", flush=True)
    result = run.wait()
    print(f"\n[完成: {result.finish_reason}]")

    # 第二轮（同一会话——上下文保留）
    run2 = agent.send("主要入口点有哪些？")
    result2 = run2.wait()
    print(result2.result)
```

## 流式事件

```python
from recursive_sdk import Agent

with Agent.create(base_url="http://127.0.0.1:3000") as agent:
    run = agent.send("总结 src/ 目录")

    # 实时流式输出助手文本和工具调用
    for msg in run.stream():
        if msg.type == "assistant":
            print(msg.text(), end="", flush=True)
        elif msg.type == "tool_call":
            print(f"\n[工具] {msg.name}")

    result = run.wait()
    print(f"\n完成，共 {result.num_turns} 轮")
```

## API 参考

### `Agent`（静态方法）

| 方法 | 说明 |
|---|---|
| `Agent.prompt(message, *, base_url, ...)` | 一次性：创建会话、发送、等待、删除。返回 `RunResult`。 |
| `Agent.create(*, base_url, ...)` | 创建持久会话，用作上下文管理器。 |
| `Agent.resume(session_id, *, base_url, ...)` | 附加到现有会话。 |
| `Agent.list_sessions(*, base_url, ...)` | 列出活跃会话。 |
| `Agent.delete_session(session_id, *, base_url, ...)` | 删除会话。 |

### `AgentSession`

| 方法 | 说明 |
|---|---|
| `agent.send(message)` | 发送消息，返回 `Run`。 |
| `agent.session_id` | 会话 ID。 |

### `Run`

| 方法 | 说明 |
|---|---|
| `run.wait()` | 阻塞等待运行完成，返回 `RunResult`。 |
| `run.messages()` | 流式消息事件迭代器。 |
| `run.stream()` | 同 `messages()`。 |

### `RunResult`

| 属性 | 类型 | 说明 |
|---|---|---|
| `status` | `str` | `"finished"` \| `"error"` \| `"cancelled"` |
| `finish_reason` | `str \| None` | Rust `FinishReason` 调试字符串 |
| `result` | `str \| None` | 累积的最终助手文本 |
| `usage` | `UsageMeta \| None` | Token 使用统计 |
| `num_turns` | `int` | 助手轮数 |
| `ok` | `bool` | `status == "finished"` 时为 `True` |
| `subtype` | `str` | Claude Agent SDK 兼容标签（`"success"`、`"error_max_turns"` 等） |

# Recursive E2E Plugin 断言参考

本文档描述 `e2e/plugins/` 提供的自定义断言类型，专用于测试 Recursive AI agent 的行为。

在 `e2e.yaml` 里加载：
```yaml
plugins:
  - ./plugins/dist/index.js
```

---

## `recursive-session` — Agent Session 验证

验证 recursive agent 的 session 文件是否符合预期。

```yaml
cases:
  - name: "session recorded write_file tool call"
    recursive-session:
      container: recursive-e2e
      input: /tmp/sessions-01          # session 目录（含 .meta.json）
      status: ["completed", "success"] # 期望的 session 状态
      hasRoles: ["user", "assistant"]  # 必须出现的角色
      hasToolCalls: ["write_file"]     # 必须出现的工具调用名
      minMessages: 3                   # 最少消息数
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `input` | string | session 目录，递归搜索 `.meta.json`（最多3层） |
| `container` | string? | 容器名，框架会 docker cp |
| `status` | string[]? | 允许的 session 状态值 |
| `hasRoles` | string[]? | 必须出现的 role |
| `hasToolCalls` | string[]? | 必须出现的工具调用名 |
| `minMessages` | number? | 最少消息条数 |

**Session 路径说明**：recursive session 存储在 `RECURSIVE_HOME/workspaces/<hash>/sessions/<name>/<id>/`（5层深）。setup 里需要浅拷贝到 `/tmp/sessions-N/`：

```bash
SESSION_DIR=$(find /tmp/recursive-home-01 -name '.meta.json' -printf '%h\n' 2>/dev/null | head -1)
if [ -n "$SESSION_DIR" ]; then
  mkdir -p /tmp/sessions-01
  cp -r "$SESSION_DIR/." /tmp/sessions-01/
fi
```

---

## `recursive-cost` — Token 消耗验证

验证 session 产生的 `cost.json` 文件。

```yaml
cases:
  - name: "Cost data is valid"
    recursive-cost:
      container: recursive-e2e
      input: /tmp/sessions-01
      exists: true
      minPromptTokens: 1
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `input` | string | session 目录 |
| `container` | string? | 容器名 |
| `exists` | boolean? | 期望 cost.json 存在/不存在 |
| `minPromptTokens` | number? | prompt_tokens 最小值 |

---

## `llm-judge` — LLM 评审 Session 质量

用 LLM 评审 agent session，判断任务是否完成、质量是否达标。

```yaml
cases:
  - name: "Agent completed the task"
    llm-judge:
      container: recursive-e2e
      input: /tmp/sessions-01
      goal: "Create a file called smoke.txt with content ok"
      minScore: 7
      requireCompleted: true
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `input` | string | session 目录 |
| `goal` | string | 原始任务描述 |
| `minScore` | number? | 最低质量分（0-10） |
| `requireCompleted` | boolean? | 是否要求判定任务已完成 |
| `apiBase` | string? | LLM API base URL |
| `apiKey` | string? | LLM API key |
| `model` | string? | LLM 模型名 |

---

## `agent-judge` — Agent 自主评审

用另一个 recursive agent 评审 session。

```yaml
cases:
  - name: "Agent judge approves"
    agent-judge:
      container: recursive-e2e
      input: /tmp/sessions-01
      goal: "Verify smoke.txt was created with content ok"
      workspace: /workspace/smoke-01
      minScore: 7
      requireCompleted: true
      maxSteps: 10
```

---

## `deferred-tool-order` — 工具调用顺序验证

验证 session 里特定工具的调用顺序。

```yaml
cases:
  - name: "ToolSearchTool called before WebFetch"
    deferred-tool-order:
      container: recursive-e2e
      input: /tmp/sessions-01
      before: "ToolSearchTool"
      after: "WebFetch"
```

---

## `deferred-tool-absent` — 验证工具未被调用

```yaml
cases:
  - name: "No ToolSearchTool in eager mode"
    deferred-tool-absent:
      container: recursive-e2e
      input: /tmp/sessions-01
      tool: "ToolSearchTool"
```

---

## Setup 模板（RECURSIVE_HOME 隔离）

每个场景用独立的 `RECURSIVE_HOME` 避免 session 路径冲突：

```yaml
setup:
  - name: "Prepare"
    exec:
      container: recursive-e2e
      command: |
        rm -rf /tmp/recursive-home-01 /tmp/sessions-01
        mkdir -p /tmp/recursive-home-01

  - name: "Run agent"
    exec:
      container: recursive-e2e
      command: |
        RECURSIVE_HOME=/tmp/recursive-home-01 \
        recursive --workspace /workspace/test-01 \
          --api-base http://aimock:4010/v1 --api-key mock-key -m mock-chat \
          --max-steps 10 \
          run "your task here"
        SESSION_DIR=$(find /tmp/recursive-home-01 -name '.meta.json' -printf '%h\n' 2>/dev/null | head -1)
        if [ -n "$SESSION_DIR" ]; then
          mkdir -p /tmp/sessions-01
          cp -r "$SESSION_DIR/." /tmp/sessions-01/
        fi

teardown:
  - name: "Clean"
    ignoreError: true
    exec:
      container: recursive-e2e
      command: |
        rm -rf /tmp/recursive-home-01 /tmp/sessions-01 /workspace/test-01
```

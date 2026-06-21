# E2E 测试经验教训总结

**Date**: 2026-06-21  
**Context**: 经历了一次完整的 E2E 套件 review → 新增测试 → 修复 14 个 pre-existing 失败的完整循环  
**Result**: 37 套件，149 passed, 0 failed, 0 skipped

---

## 一、ArgusAI 规范合规度：诚实评估

### ✅ 已做到的

| 规范 | 实际状态 |
|------|---------|
| `recursive-session:` 断言验证 agent 工具调用 | 22 处，覆盖率良好 |
| `file:` 断言验证文件系统输出 | 44 处 |
| `llm-judge:` 断言用于语义验证 | 2 处（reserved for appropriate cases） |
| `sequential: true` for stateful tests | HTTP API / goal-loop / auth 套件均已标记 |
| setup/teardown 生命周期管理 | 所有套件均有清理逻辑 |

### ⚠️ 存在的合规缺口

**最大缺口：`request:` 断言完全未使用（0 次）**

ArgusAI 提供了原生的 `request:` 断言用于 HTTP API 测试，但 37 个套件里**一次都没用到**。
所有 HTTP 调用全部走的是 `exec: curl ...`，包括：

- `08-http-api.yaml`（23 个 exec，含大量 curl）
- `18-goal-loop.yaml`（13 个 exec，全是 curl）
- `19-http-interrupt.yaml`（9 个 exec，全是 curl）
- `22-compaction.yaml`（6 个 exec，含 curl）
- `39-http-auth.yaml`（11 个 exec，全是 curl）

**为什么会这样？**  
ArgusAI 的 `request:` 发出的是从 *测试运行机* 出发的 HTTP 请求，而 `recursive http` 服务跑在 Docker 容器内部，外部不可达。因此对"容器内的 HTTP 服务"只能用 `exec: curl`（在容器内执行）。这不是违规，而是架构约束。

> **结论**：当前规范合规度 **基本达标**，`exec: curl` 用于容器内 HTTP 服务是合理的架构折衷，
> 但 shell 断言占比普遍偏高（整体均值约 75%），未来新测试应尽量优先使用 `file:` 和 `recursive-session:`。

---

## 二、值得记录的关键教训

### 教训 1：Binary 与 Source 版本漂移是 E2E 失败的头号杀手

**发生了什么**：14 个失败里有 10 个直接源于容器里的 `recursive` binary 是旧版本，
不支持/已移除某些功能，但测试按源码的新行为写的。

**具体差异**：

| 特性 | 容器 binary（旧） | 源码（新） |
|------|-----------------|-----------|
| `RECURSIVE_SESSIONS_DIR` | 忽略，session 存在 `RECURSIVE_HOME/workspaces/{hash}/sessions/` | 正常支持 |
| `apply_patch` 工具 | 已移除 | 无此工具 |
| 工具命名 | 部分 snake_case (`write_file`) | PascalCase (`Write`, `Read`) |
| Rate limit burst | 硬编码 2，忽略环境变量 | 可通过 `RECURSIVE_RATE_LIMIT_BURST` 设置 |
| `Edit` 工具 | 不存在 | 已添加 |

**教训**：  
写 E2E 测试前，先用 `docker exec recursive-e2e recursive --version` 和
`docker exec recursive-e2e recursive tools list` 确认容器里的实际工具集，
不要假设容器 binary 和 `cargo build` 产出一致。

**预防措施**：
```yaml
# 在 e2e.yaml 的全局 setup 中加一个 binary 信息打印 case
- name: "Print binary version"
  exec:
    container: recursive-e2e
    command: recursive --version && recursive tools list 2>&1 | head -20
```

---

### 教训 2：Session 路径断言必须先验证路径，不能硬编码

**发生了什么**：多个套件断言 `/workspace/sessions/` 下有 session，但实际路径是
`/workspace/workspaces/{SHA256_HASH}/sessions/`，hash 是运行时生成的，无法预知。

**错误模式**：
```yaml
assert:
  recursive-session:
    input: /workspace/sessions  # ❌ 路径不存在
```

**正确模式** — 动态定位 + 复制到可预期路径：
```bash
# 先隔离 workspace
RECURSIVE_HOME=/tmp/rh-mytest recursive run ...

# 再动态找 transcript.jsonl
SESSION_DIR=$(find /tmp/rh-mytest -name "transcript.jsonl" 2>/dev/null | head -1 | xargs dirname)
mkdir -p /tmp/sessions-mytest
cp -r "$SESSION_DIR/." /tmp/sessions-mytest/
```

**教训**：`RECURSIVE_SESSIONS_DIR` 在旧 binary 中不工作，但 `RECURSIVE_HOME` 始终有效。
每个需要断言 session 的测试都应该用 `RECURSIVE_HOME=/tmp/rh-{unique}` 隔离，
不同 case 用不同目录名防止污染。

---

### 教训 3：aimock fixture 的 turnIndex 优于 userMessage 匹配

**发生了什么**：`13-apply-patch` 从 `apply_patch` 改为 `read_file + write_file` 两步流程，
需要 fixture 按对话轮次给出不同响应。

**低效方案**：用 `userMessage` 匹配用户请求文本（fragile，换个措辞就失效）。

**正确方案**：用 `turnIndex` + `hasToolResult` 组合：
```json
[
  {
    "turnIndex": 0,
    "response": { "tool_calls": [{ "name": "read_file", ... }] }
  },
  {
    "turnIndex": 1,
    "hasToolResult": true,
    "response": { "tool_calls": [{ "name": "write_file", ... }] }
  },
  {
    "turnIndex": 2,
    "hasToolResult": true,
    "response": { "content": "Done." }
  }
]
```

`turnIndex` 精确控制对话步骤，`hasToolResult: true` 确保工具调用结果已返回再触发下一步。

---

### 教训 4：Node.js 18+ fetch 在容器里默认解析 localhost 为 IPv6 (::1)

**发生了什么**：`21-typescript-sdk.yaml` 里 Node.js 脚本用 `http://localhost:9097` 连接
`recursive` HTTP 服务，一直报 `ECONNREFUSED`，但 curl 同样的地址能通。

**原因**：Node.js 18 的内置 `fetch`（基于 undici）按 RFC 6724 DNS 解析顺序，
优先返回 `::1`（IPv6），而 `recursive` 服务绑定的是 `0.0.0.0`（IPv4 only）。

**Fix**：
```javascript
// ❌ 有问题
baseUrl: 'http://localhost:9097'

// ✅ 明确指定 IPv4
baseUrl: 'http://127.0.0.1:9097'
```

**教训**：容器内 Node.js 测试脚本，服务地址永远用 `127.0.0.1` 不用 `localhost`。

---

### 教训 5：HTTP API 测试的端口隔离和进程清理必须严格

**发生了什么**：`39-http-auth.yaml`（端口 9097）和 `21-typescript-sdk.yaml`（原来也是 9097）
共用同一端口，前者的 auth server 进程没被清理干净，后者的 SDK 连上去拿到 `HTTP 401`。

**错误模式**：不同套件用相同端口，teardown 依赖 `pkill -f` 但进程残留。

**正确模式**：
1. 每个 HTTP 套件分配唯一端口（08=9090, 08b=9091, 18=9092, 19=9093, 22=9099, 21=9096, 39=9097）
2. Setup 开头先 kill + sleep 1 确保端口释放
3. Teardown 最后再 kill 一次

**教训**：在 `e2e.yaml` 或 test 注释里维护一张端口注册表，新增 HTTP 套件时先查表再分配。

---

### 教训 6：`recursive loop` 模式不产生 transcript.jsonl

**发生了什么**：`17-loop-mode.yaml` 用 `recursive-session:` 断言验证 loop 模式下的工具调用，
但 `recursive loop` 命令不会创建 transcript.jsonl，导致 "No session directory found"。

**教训**：`recursive loop` 是批处理模式，没有持久化 session 的语义。
对 loop 模式的验证只能用 `file:` 断言（验证输出文件内容）或 `exec:` 检查 stdout，
不能用 `recursive-session:`。

---

### 教训 7：POST /sessions 创建 session 必须带 Content-Type header

**发生了什么**：`22-compaction.yaml` 的 `POST /sessions` 没带 `-H 'Content-Type: application/json'`，
服务返回 `422 Unprocessable Entity`，测试报 exit code 22。

同一个坑的两个变体：
1. 缺 `Content-Type: application/json`
2. 消息体字段名写错：`{"message": "..."}` 应该是 `{"content": "..."}`

**防范模板**：
```bash
# 创建 session
SESSION=$(curl -sf -X POST http://127.0.0.1:9099/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"You are a test assistant."}' | jq -r .id)

# 发消息
curl -sf -X POST http://127.0.0.1:9099/sessions/$SESSION/messages \
  -H 'Content-Type: application/json' \
  -d '{"content": "Hello"}'
```

---

### 教训 8：`argusAI save:` 无法捕获 exec 的 stdout（引擎限制）

**发生了什么**：原本想用 `save:` 把 session ID 存成变量复用，但发现 ArgusAI 引擎
不支持从 `exec:` 的标准输出捕获变量。

**当前可行的 workaround**：把 session ID 等运行时产出写到临时文件，后续 case 读文件。
```bash
# Case 1: 保存
SESSION=$(recursive run ... | grep "session:" | awk '{print $2}')
echo "$SESSION" > /tmp/http-sid

# Case 2: 读取
SESSION=$(cat /tmp/http-sid)
curl .../sessions/$SESSION/...
```

**教训**：这是 ArgusAI 的引擎限制，不是 bug，设计 stateful 测试时要考虑临时文件传递状态。

---

### 教训 9：测试按 shell 比例分类，决定是否需要重构

当前 exec 断言占 75%+ 的套件（需要留意）：

| 套件 | shell 占比 | 是否合理 |
|------|-----------|---------|
| `08-http-api.yaml` | 85% | ✅ 合理（容器内 HTTP，只能 curl） |
| `18-goal-loop.yaml` | 100% | ✅ 合理（同上） |
| `39-http-auth.yaml` | 100% | ✅ 合理（同上） |
| `06-live-integration.yaml` | 100% | ⚠️ 待改善（需要真实 LLM，暂 skip） |
| `36-session-rewind.yaml` | 100% | ⚠️ 可以补 `recursive-session:` 断言 |
| `04-cost-tracking.yaml` | 100% | ⚠️ 可以补 `recursive-session:` 断言 |

**经验法则**：
- 容器内 HTTP 服务测试 → `exec: curl` 是正确做法，不是规范问题
- Agent 行为测试（工具调用、输出内容）→ 应优先 `recursive-session:` + `file:`
- 纯文件验证 → `file:` 而非 `exec: cat | grep`

---

## 三、给未来自己的 Checklist

新建 E2E 套件前要问自己：

- [ ] 确认了容器 binary 版本和工具列表？
- [ ] 需要断言 session 的，用了 `RECURSIVE_HOME` 隔离 + 动态 find 吗？
- [ ] HTTP 服务测试，端口与其他套件不重复？
- [ ] Node.js 脚本用的是 `127.0.0.1` 而不是 `localhost`？
- [ ] POST 请求带了 `Content-Type: application/json`，字段名用了 `content` 而不是 `message`？
- [ ] aimock fixture 用 `turnIndex` 而不是 fragile 的文本匹配？
- [ ] loop 模式测试不用 `recursive-session:`，改用 `file:` 断言？

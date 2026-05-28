# Proposal: E2E Smoke Test Gate in Self-Improve Flow

> **Status**: Draft — pending implementation
> **Created**: 2026-05-28
> **Context**: 基于已有的 ArgusAI E2E 框架 + Record-Replay Plan
> **前置 Plan**: `~/.claude-internal/plans/idempotent-zooming-eclipse.md`（Record → Replay 测试生命周期机制）

## Problem

当前 `self-improve.sh` 的验证链：

```
Agent 修改代码 → cargo test → [self-review] → commit → merge
```

**缺陷**：`cargo test` 只验证单元/集成测试通过。它**不能**证明：
1. 新构建的 binary 能作为 agent 正常执行一个 goal
2. 工具调用（read_file, write_file, shell 等）在真实运行时仍然工作
3. 多轮对话、session 持久化等端到端行为未被破坏

**后果**：Goal N 引入了一个 `cargo test` 覆盖不到的 bug → Goal N+1 用了新 binary → 运行失败 → orchestrator 误诊为"goal 太难"或"LLM 不行"。

## Solution: E2E Smoke Gate

在 `cargo test` 通过之后、`git commit` 之前，插入 **E2E 冒烟测试**：

```
Agent 修改代码 → cargo test → self-review → [E2E smoke] → commit → merge
                                              ↑ NEW
```

### 冒烟测试的定义

一组**轻量级、确定性、快速**的 E2E 用例，证明新 binary 能：
1. 启动并接受 goal
2. 调用至少一个工具并正确处理结果
3. 正常完成（exit code 0, FinishReason::NoMoreToolCalls）
4. 产出正确的 session 记录

这些用 **replay 模式**运行（aimock fixture，不需要真实 LLM），所以：
- 无 API key 依赖
- 确定性（不依赖 LLM 输出质量）
- 快速（<30 秒）

### 实现方式

#### 方案 A：统一到 ArgusAI E2E 框架（推荐）

复用已有的 e2e 测试套件，专门标记一个 "smoke" suite：

```yaml
# e2e/e2e.yaml
tests:
  suites:
    - name: "Smoke (self-improve gate)"
      id: smoke
      file: tests/00-smoke.yaml    # ← 新增
    - name: "Basic Agent Tools"
      id: basic
      file: tests/01-basic-tools.yaml
    # ...
```

```yaml
# e2e/tests/00-smoke.yaml
name: "Self-Improve Smoke Gate"
description: "Minimal tests proving the new binary works as an agent"
sequential: true

cases:
  - name: "Binary starts and runs a goal"
    exec:
      container: recursive-e2e
      command: >
        recursive --workspace /workspace/smoke
        --api-base http://aimock:4010/v1 --api-key mock-key -m mock-chat
        --max-steps 5
        run "Create hello.txt with content 'smoke test passed'"
    expect:
      exitCode: 0

  - name: "File was created"
    file:
      container: recursive-e2e
      path: /workspace/smoke/hello.txt
      exists: true
      contains: "smoke test passed"

  - name: "Session is valid"
    recursive-session:
      container: recursive-e2e
      input: /workspace/smoke/.recursive/sessions
      status: ["completed", "success"]
      hasRoles: ["user", "assistant"]
      hasToolCalls: ["write_file"]
      minMessages: 3
```

在 `self-improve.sh` 中调用：

```bash
# After cargo test passes, before commit
if command -v argusai >/dev/null 2>&1 && [[ -f "e2e/e2e.yaml" ]]; then
  echo "[self-improve] Running E2E smoke gate..."
  
  # Build the new binary (we need the MODIFIED binary, not the one running the agent)
  cargo build -q
  
  # Run smoke suite in replay mode (deterministic, no API key needed)
  if argusai -c e2e/e2e.yaml run -s smoke --quiet 2>&1; then
    echo "[self-improve] E2E smoke: PASSED ✓"
  else
    echo "[self-improve] E2E smoke: FAILED ✗ — rolling back"
    verdict_and_exit "rolled-back" "E2E smoke test failed"
  fi
fi
```

#### 方案 B：轻量独立脚本（不依赖 Docker/ArgusAI）

如果 ArgusAI/Docker 在 worktree 环境不方便运行，可以用一个更简单的替代：

```bash
# .dev/scripts/smoke-test.sh
# 用新构建的 binary 跑一个预设的 fixture-based goal
BIN=./target/debug/recursive

# 准备 mock 环境（或直接用已有的 fixture）
WORKSPACE=$(mktemp -d)
echo '{"fixtures":[{"match":{"userMessage":"smoke"},"response":{"toolCalls":[{"name":"write_file","arguments":{"path":"ok.txt","contents":"pass"}}]}},{"match":{"hasToolResult":true},"response":{"content":"Done"}}]}' > /tmp/smoke-fixture.json

# 如果有 aimock 可用
if command -v aimock >/dev/null 2>&1; then
  aimock -f /tmp/smoke-fixture.json &
  MOCK_PID=$!
  sleep 1
  
  $BIN --workspace "$WORKSPACE" \
    --api-base http://localhost:4010/v1 \
    --api-key mock --model mock \
    --max-steps 5 \
    run "smoke test" 2>/dev/null
  RESULT=$?
  
  kill $MOCK_PID 2>/dev/null
  rm -rf "$WORKSPACE"
  exit $RESULT
fi
```

### 推荐方案

**方案 A（ArgusAI 统一框架）**更好：
- 复用已有基础设施，不引入新工具
- 测试定义是声明式的（YAML），易于维护
- 天然支持 Record → Replay 升级路径
- 已有的 01-basic-tools 用例本身就是一个好的 smoke test

**唯一前提**：worktree 环境能跑 Docker（ArgusAI 需要 Docker）。如果 worktree 在同一台机器上，Docker 是共享的，应该没问题。

## 与 Record-Replay 生命周期的关系

```
┌─────────────────────────────────────────────────────────────────┐
│ Record-Replay 生命周期（前置 plan）                              │
│                                                                   │
│ [Record] 新功能 → 真实 LLM → llm-judge → PASS → 录制 fixture  │
│    ↓                                                             │
│ [Promote] fixture 归档 → 生成确定性断言 → 提交 git             │
│    ↓                                                             │
│ [Replay] fixture 回放 → 确定性断言 → CI 回归                   │
└─────────────────────────────────────────────────────────────────┘
                    ↕ 共用同一个 fixture 库

┌─────────────────────────────────────────────────────────────────┐
│ Smoke Gate（本 proposal）                                        │
│                                                                   │
│ Goal 完成 → cargo test ✓ → E2E smoke (replay 模式) ✓ → commit │
│                                                                   │
│ Smoke suite = 从 fixture 库中选出的最基本用例子集               │
│ 快速（<30s）、确定性、不需要 API key                           │
└─────────────────────────────────────────────────────────────────┘
```

**Smoke Gate 是 Record-Replay 的消费者**——它使用已经 promote 过的 fixture 来验证新 binary。不需要 record 模式。

## Implementation Steps

1. 确保 `e2e/tests/00-smoke.yaml` 存在（可从 01-basic-tools.yaml 简化）
2. 确保 smoke fixture 存在（`e2e/fixtures/00-smoke.json`）
3. 在 `self-improve.sh` 中 cargo test 后加入 smoke gate 调用
4. 决定：smoke 失败是 blocking（rollback）还是 warning-only？
   - **建议**：blocking —— smoke 失败说明新 binary 根本不能工作

## Open Questions

1. **Docker in worktree?** — ArgusAI 需要 Docker。worktree 是同机器上的另一个 checkout，Docker 可以共享。但 `e2e/Dockerfile` 的 build context 是 repo root——worktree 路径不同，需要调整。
2. **构建哪个 binary?** — smoke 测试应该验证的是**修改后的代码**编译出的 binary。self-improve.sh 在 smoke 前需要 `cargo build` 一次（用修改后的 src 重新构建）。
3. **性能预算** — Docker build + run 可能需要 30-60 秒。对 self-improve 来说可接受（总运行时间通常 5-30 分钟）。

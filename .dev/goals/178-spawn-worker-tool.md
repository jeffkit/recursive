# Goal 178 — `spawn_worker` Tool: First-class 委派工具

**Roadmap**: Phase 18 — Advanced Agent Patterns (coordinator pattern)
**Design principle check**:
- 新工具文件 `src/tools/spawn_worker.rs`，注册到 `src/tools/mod.rs`
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支
- ✅ 纯新增能力，不改现有接口

## Why

当前 `TeamOrchestrator` 让 lead LLM 用 **文本格式** `DELEGATE:<role>:<task>` 委派任务，
然后 orchestrator 解析文本。这种方式：
- **脆弱**：LLM 可能写错格式（多空格、大小写不一致）
- **非结构化**：没有类型安全
- **不可扩展**：不支持额外参数（timeout、tools 列表等）

Fake CC 的 `AgentTool` 是一个 **first-class tool call**：LLM 通过 JSON tool call 委派，
不存在文本解析问题。

本 Goal 实现 `spawn_worker` 工具，让协调者 Agent 可以通过标准 tool call 委派任务给子 Agent，
子 Agent 使用指定的系统提示词运行，返回完成结果。

## What this goal does

### 1. 新工具文件 `src/tools/spawn_worker.rs`

**Tool name**: `spawn_worker`

**Parameters**:
```json
{
  "type": "object",
  "properties": {
    "prompt": {
      "type": "string",
      "description": "发给子 Agent 的完整任务描述。应包含足够上下文，因为子 Agent 没有父 Agent 的对话历史。"
    },
    "system_prompt": {
      "type": "string",
      "description": "子 Agent 的系统提示词，定义其角色和能力。默认为通用工作者提示词。"
    },
    "worker_type": {
      "type": "string",
      "enum": ["general", "explore", "coder", "reviewer", "researcher"],
      "description": "预设工作者类型，会设置合适的系统提示词和工具访问权限。若同时提供 system_prompt，则以 system_prompt 为准。",
      "default": "general"
    },
    "max_steps": {
      "type": "integer",
      "description": "子 Agent 最大步骤数（默认 30）",
      "default": 30
    }
  },
  "required": ["prompt"]
}
```

**Side effect**: `ToolSideEffect::External`（general/coder/reviewer worker 可写文件）

**预设工作者类型**:
- `general`: 全工具访问，通用任务
- `explore`: 只读工具（read_file, list_dir, search_files, web_fetch），适合代码调研
- `coder`: 全工具访问，专注代码实现，系统提示词强调写代码和测试
- `reviewer`: 只读工具，专注代码审查
- `researcher`: 只读工具 + web_fetch，适合调研类任务

**执行流程**:
1. 根据 `worker_type` 选择默认 system_prompt 和 tools 列表
2. 若提供了 `system_prompt` 参数，覆盖默认值
3. 创建子 Agent 内核（复用 `SubAgent` 的逻辑）
4. 运行子 Agent，收集最终输出
5. 返回子 Agent 的最终 assistant 消息内容

**实现方式**:
- 与 `SubAgent` 工具共享执行核心逻辑（提取公共函数或直接复用）
- 不同点：`spawn_worker` 暴露更丰富的角色配置，`sub_agent` 保持向后兼容

### 2. 注册到 `src/tools/mod.rs`

在 `build_standard_tools()` 和 `build_tools()` 中注册 `SpawnWorkerTool`。

### 3. 测试 (in `src/tools/spawn_worker.rs`)

- `spawn_worker_general_type`: general 类型子 agent，运行并返回结果
- `spawn_worker_explore_type`: explore 类型只读限制
- `spawn_worker_custom_system_prompt`: 自定义 system_prompt 覆盖预设
- `spawn_worker_missing_prompt`: 无 prompt 参数 → `BadToolArgs` 错误

## Files to change

| File | Change |
|------|--------|
| `src/tools/spawn_worker.rs` (new) | `SpawnWorkerTool` 实现 |
| `src/tools/mod.rs` | `pub mod spawn_worker;` + 注册 |

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 干净
3. `spawn_worker` 出现在 `build_standard_tools()` 中
4. 4 个单元测试通过

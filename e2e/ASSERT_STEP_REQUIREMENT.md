# ArgusAI YAML Engine: 插件注册自定义步骤类型

> 提给 ArgusAI 开发的功能需求

## 概述

允许 Plugin 注册**自定义步骤类型**，与内置的 `file`、`exec`、`request`、`process`、`port` 平级使用。YAML engine 遇到未识别的步骤 key 时，查找 plugin registry 委托执行。

## 动机

当前 YAML engine 只认识固定的步骤类型。自定义断言逻辑（如 session JSONL 解析、LLM-as-judge）只能通过 `exec` 嵌入脚本实现，丧失了声明式优势。

Plugin 已经可以通过 `PluginModule.assertionPlugins` 注册断言逻辑，但 YAML engine 无法调用它们。

## 期望的 YAML 用法

```yaml
cases:
  # 内置步骤类型（已有）
  - name: "Run agent"
    exec:
      container: recursive-e2e
      command: "recursive --workspace /workspace/test-01 run 'Create hello.txt'"
    expect:
      exitCode: 0

  - name: "File created"
    file:
      container: recursive-e2e
      path: /workspace/test-01/hello.txt
      exists: true
      contains: "world"

  # Plugin 注册的步骤类型（新增能力）
  - name: "Session valid"
    recursive-session:
      container: recursive-e2e
      input: /workspace/test-01/.recursive/sessions
      status: ["completed", "success"]
      hasToolCalls: ["write_file"]
      minMessages: 3

  - name: "Cost within budget"
    recursive-cost:
      container: recursive-e2e
      input: /workspace/test-01/.recursive/sessions
      exists: true
      minPromptTokens: 1
      maxCostUsd: 0.10

  - name: "LLM Judge approves"
    llm-judge:
      container: recursive-e2e
      input: /workspace/test-01/.recursive/sessions
      goal: "Create hello.txt with content 'world'"
      minScore: 3
```

## 执行模型

YAML engine 的 `executeStep()` 增加一个 fallback 分支：

```typescript
async function executeStep(step, containerName, ctx) {
  if ('request' in step) return executeRequestStep(step, ...);
  if ('exec' in step) return executeExecStep(step, ...);
  if ('file' in step) return executeFileStep(step, ...);
  if ('process' in step) return executeProcessStep(step, ...);
  if ('port' in step) return executePortStep(step, ...);

  // NEW: 查找 plugin 注册的步骤类型
  const pluginStepKey = findPluginStepKey(step);
  if (pluginStepKey) {
    return executePluginStep(pluginStepKey, step, containerName, ctx);
  }

  return [`Step "${step.name}" has no recognized step type`];
}
```

`executePluginStep` 的逻辑：

```typescript
async function executePluginStep(key, step, containerName, ctx) {
  const stepConfig = step[key];  // e.g., step['recursive-session']
  const container = stepConfig.container || containerName;
  let input = resolveTemplateVars(stepConfig.input, ctx);

  // 如果 input 是容器内路径，docker cp 到本地
  if (container && input.startsWith('/')) {
    input = await copyFromContainer(container, input);
  }

  // 调用 plugin
  const results = globalAssertionPluginRegistry.runAll(key, input, stepConfig);

  // 收集失败
  const errors = results.filter(r => !r.passed).map(r => `[${key}] ${r.message}`);
  return errors;
}
```

## Plugin 侧接口（已有，无需改动）

```typescript
// 插件已通过 PluginModule.assertionPlugins 注册
const plugin: PluginModule = {
  name: 'recursive-agent',
  assertionPlugins: [
    {
      name: 'recursive-session',  // ← 同时作为 YAML 步骤类型 key
      assert(type, input, config) {
        // type = 'recursive-session'
        // input = 本地路径（已 docker cp）
        // config = YAML 中该步骤的所有字段（除 container/input）
        return [...assertionResults];
      },
    },
  ],
};
```

## 容器路径桥接

Plugin 跑在宿主机的 ArgusAI 进程中，`input` 可能是容器内路径。需要：

1. 检测 `container` 字段 + `input` 为绝对路径
2. `docker cp <container>:<input> <tmpdir>/`
3. 将本地临时路径传给 plugin
4. 执行完毕后清理临时目录

## 内置 vs Plugin 步骤类型

| 类型 | 来源 | 说明 |
|------|------|------|
| `request` | 内置 | HTTP 请求 + 响应断言 |
| `exec` | 内置 | 容器内命令 + 输出断言 |
| `file` | 内置 | 容器内文件断言 |
| `process` | 内置 | 容器内进程断言 |
| `port` | 内置 | 端口监听断言 |
| `llm-judge` | 内置/Plugin | LLM 语义评审（调 API） |
| `recursive-session` | Plugin | Session JSONL 结构断言 |
| `recursive-cost` | Plugin | Cost tracking 断言 |
| `xxx-custom` | Plugin | 任何项目自定义断言 |

## 验收标准

1. YAML engine 遇到未知步骤 key 时查找 plugin registry
2. 匹配到 plugin 后正确调用 `assert(type, input, config)`
3. `container` + 绝对路径 → 自动 docker cp 桥接
4. 断言结果正确映射为 case pass/fail
5. 无匹配 plugin 时报错提示可用的步骤类型列表

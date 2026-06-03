# recursive run

执行单次目标后退出。

```bash
recursive run [选项] <目标>
```

## 参数

| 参数 | 说明 |
|---|---|
| `<目标>` | 传递给 Agent 的目标字符串 |

## 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--workspace <path>` | 当前目录 | 文件系统沙箱根目录 |
| `--max-steps <n>` | `32` | 步骤预算 |
| `--session <id>` | *(新建)* | 恢复已有会话 |
| `--system-prompt-file <path>` | *(内置)* | 自定义系统提示 |
| `--json` | 关 | 以 JSON 格式输出结果 |

## 示例

```bash
# 基本用法
recursive run "列出 src/ 的文件并总结架构"

# 指定工作区
recursive run --workspace /my/project "审查最近的变更"

# 恢复会话
recursive run --session abc123 "继续上次的工作"

# JSON 输出
recursive run --json "2+2等于几" | jq .finish_reason
```

## 退出码

| 代码 | 含义 |
|---|---|
| `0` | `FinishReason::Done` |
| `1` | `FinishReason::Error` |
| `2` | `FinishReason::BudgetExceeded` |
| `3` | `FinishReason::Stuck` |

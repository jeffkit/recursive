# recursive repl

交互式 REPL——每行一个目标。

```bash
recursive repl [选项]
```

## 说明

启动交互式会话，每行输入被视为一个新目标。Agent 执行完毕并打印结果后，等待下一个目标。

同一 REPL 会话内，会话状态（对话记录、内存）跨目标持久保存。

## 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--workspace <path>` | 当前目录 | 文件系统沙箱根目录 |
| `--max-steps <n>` | `32` | 每个目标的步骤预算 |
| `--session <id>` | *(新建)* | 恢复已有会话 |

## REPL 命令

| 命令 | 说明 |
|---|---|
| `:q` 或 `:quit` | 退出 REPL |
| `:clear` | 清空对话记录（重新开始） |
| `:session` | 打印当前会话 ID |
| `:tools` | 列出可用工具 |

## 示例

```
$ recursive repl
Recursive REPL — type a goal, :q to exit
> 列出 src/ 目录的文件
[tool: list_dir] ...
src/ 目录包含：agent.rs, lib.rs, tools/, llm/, ...

> 解释 agent.rs 的作用
[tool: read_file] ...
agent.rs 实现了 ReAct 循环。它交替调用...

> :q
再见。
```

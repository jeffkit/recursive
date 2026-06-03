# recursive loop

自调度自主循环模式。

```bash
recursive loop [选项] <目标>
```

## 说明

Loop 模式让 Agent 持续循环运行。完成一个目标后，Agent 可以调度下次唤醒时间，适合用于监控、定期任务和自我改进工作流。

Agent 获得一个特殊的 `schedule_wakeup` 工具，可以设置下次休眠多长时间后再被唤醒。

## 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--workspace <path>` | 当前目录 | 文件系统沙箱根目录 |
| `--max-steps <n>` | `32` | 每轮迭代的步骤预算 |
| `--interval <secs>` | *(Agent 决定)* | 覆盖唤醒间隔 |
| `--max-iterations <n>` | 无限 | N 次迭代后停止 |

## 示例

```bash
# 监控目录并报告变更
recursive loop "监控 src/ 的变化，总结上次运行以来发生了什么"

# 自我改进循环（Recursive 项目本身使用的方式）
recursive loop "读取 .dev/goals/ 并实现下一个未完成的目标"
```

## 循环状态

迭代之间，Agent 可以使用 `remember` 和 `recall` 工具持久化状态，从而追踪已完成和待完成的任务。

# 沙箱模式

Recursive 支持多级工具执行隔离。

## 概览

| 模式 | 隔离级别 | 适用场景 |
|---|---|---|
| `local` | 宿主进程 | 开发、可信工作负载 |
| `policy` | 路径+命令限制 | 基于白名单的适度隔离 |
| `docker` | Docker 容器（L2） | 不可信代码、多用户 |
| `e2b` | E2B 微虚拟机（L3） | 完整 VM 隔离、云端执行 |

```bash
export RECURSIVE_SANDBOX_MODE=docker
```

## local（默认）

工具直接在宿主进程中运行。通过 `resolve_within` 强制文件系统沙箱。

## docker

每次 `run_shell` 调用都在全新 Docker 容器内执行。工作区以读写方式挂载，其他宿主路径均被排除。

需要宿主机运行 Docker daemon。

## e2b

每次 Agent 运行获得独立的 E2B 微虚拟机。完整 Linux 沙箱，网络访问可控。

```bash
RECURSIVE_SANDBOX_MODE=e2b
RECURSIVE_E2B_API_KEY=your-e2b-api-key
RECURSIVE_E2B_TEMPLATE=base
RECURSIVE_E2B_TIMEOUT_SECS=300
```

需要 [E2B](https://e2b.dev) 账号。

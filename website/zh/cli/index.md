# CLI 概览

`recursive` 二进制提供以下子命令：

| 命令 | 说明 |
|---|---|
| [`run`](./run) | 执行单次目标后退出 |
| [`repl`](./repl) | 交互式 REPL——每行一个目标 |
| [`loop`](./loop) | 自调度自主循环模式 |
| [`http`](./http) | 启动 HTTP API 服务器 |
| [`tools`](./tools) | 列出已注册的工具（无需 API Key） |
| [`sessions`](./sessions) | 管理持久化会话 |

## 安装

```bash
cargo install recursive-agent
```

## 全局参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--workspace <path>` | 当前目录 | 文件系统沙箱根目录 |
| `--max-steps <n>` | `32` | 每次运行的步骤预算 |
| `--model <name>` | `gpt-4o-mini` | 覆盖模型 |
| `--api-base <url>` | `https://api.openai.com/v1` | 覆盖接口地址 |
| `--provider <name>` | `default` | 来自 `providers.toml` 的命名 Provider |
| `-v / --verbose` | 关 | 详细输出 |

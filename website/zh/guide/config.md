# 配置参考

所有配置均通过环境变量完成（部分支持 CLI 参数覆盖）。基本使用无需配置文件。

## LLM Provider

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Chat-completions 接口地址 |
| `RECURSIVE_API_KEY` | *(必填)* | Bearer Token |
| `RECURSIVE_MODEL` | `gpt-4o-mini` | 模型名称 |
| `RECURSIVE_PROVIDER_TYPE` | `openai` | 协议适配器：`openai` 或 `anthropic` |
| `RECURSIVE_MAX_STEPS` | `32` | 每次运行最大工具调用循环次数 |
| `RECURSIVE_TEMPERATURE` | `0.2` | 采样温度 |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | *(内置)* | 自定义系统提示文件路径 |
| `RECURSIVE_WORKSPACE` | 当前目录 | 文件系统沙箱根目录 |

## HTTP 服务器

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RECURSIVE_HTTP_ADDR` | `0.0.0.0:3000` | 绑定地址 |
| `RECURSIVE_HTTP_AUTH_KEYS` | *(无——开放)* | 逗号分隔的 `X-API-Key` 白名单 |

## 云存储 — Redis

> 需要 `cloud-runtime` feature 标志（`--features cloud-runtime`）。

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RECURSIVE_REDIS_URL` | *(禁用)* | Redis 连接 URL，如 `redis://host:6379` |
| `RECURSIVE_REDIS_KEY_PREFIX` | `recursive:` | Key 命名空间前缀 |
| `RECURSIVE_REDIS_SESSION_TTL_SECS` | `7200` | 会话过期时间（2 小时） |

## 云存储 — S3

> 需要 `cloud-runtime` feature 标志。

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RECURSIVE_S3_BUCKET` | *(禁用)* | S3 桶名称 |
| `RECURSIVE_S3_PREFIX` | `recursive` | 对象键前缀 |
| `RECURSIVE_S3_TENANT_ID` | `default` | 桶内的租户命名空间 |
| `AWS_ACCESS_KEY_ID` | *(来自 SDK)* | AWS 凭证 |
| `AWS_SECRET_ACCESS_KEY` | *(来自 SDK)* | AWS 凭证 |
| `AWS_DEFAULT_REGION` | `us-east-1` | AWS 区域 |
| `AWS_ENDPOINT_URL` | *(AWS)* | LocalStack / MinIO 覆盖地址 |

## 沙箱

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RECURSIVE_SANDBOX_MODE` | `local` | `local` / `policy` / `docker` / `e2b` |
| `RECURSIVE_E2B_API_KEY` | *(e2b 模式必填)* | E2B API Key |
| `RECURSIVE_E2B_TEMPLATE` | `base` | E2B 沙箱模板 ID |
| `RECURSIVE_E2B_TIMEOUT_SECS` | `300` | 沙箱超时（秒） |
| `RECURSIVE_SHELL_TIMEOUT_SECS` | `30` | 单条 shell 命令超时 |

## 配置文件

也可以使用 TOML 配置文件。Recursive 默认查找 `~/.recursive/config.toml` 和当前目录的 `providers.toml`。

```toml
# providers.toml 示例
[default]
api_base = "https://api.openai.com/v1"
api_key  = "sk-..."
model    = "gpt-4o-mini"

[deepseek]
api_base = "https://api.deepseek.com/v1"
api_key  = "sk-..."
model    = "deepseek-coder"

[glm]
api_base = "https://open.bigmodel.cn/api/paas/v4"
api_key  = "..."
model    = "glm-4-flash"
```

通过 `--provider deepseek`（CLI）或 `RECURSIVE_PROVIDER=deepseek` 选择命名 Provider。
